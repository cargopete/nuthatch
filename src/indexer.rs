//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::decode::{self, TRANSFER_TOPIC0};
use crate::rpc::RpcClient;
use crate::serve;
use crate::store::Store;

/// Block window per `eth_getLogs` call. Kept small so high-volume contracts (e.g. USDC, ~thousands
/// of Transfers per handful of blocks) stay under public-RPC result-size caps.
const WINDOW: u64 = 20;
const LAST_BLOCK_KEY: &str = "last_block";

pub async fn dev(args: DevArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    let store = Store::open(&dir.join(DB_FILE))?;
    let rpc = Arc::new(RpcClient::new(config.rpc_urls.clone())?);

    tracing::info!(
        "indexing {} on {} — Transfer events only (skeleton)",
        config.address,
        config.chain
    );

    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        rpc.clone(),
        store.clone(),
        config.address.clone(),
        args.backfill,
    ));

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.address.clone(),
        chain: config.chain.clone(),
    };
    serve::run(&args.listen, app_state).await?;

    ingest.abort();
    Ok(())
}

async fn index_loop(rpc: Arc<RpcClient>, store: Store, address: String, backfill: u64) -> Result<()> {
    // Resume from the last committed block, else start `backfill` blocks behind the tip.
    let mut next = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
        None => {
            let tip = rpc.block_number().await?;
            let start = tip.saturating_sub(backfill);
            tracing::info!("cold start: backfilling from block {start} (tip {tip})");
            start
        }
    };

    loop {
        let tip = match rpc.block_number().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("block_number failed: {e:#}; retrying");
                sleep_secs(3).await;
                continue;
            }
        };

        // Reorg check: has the last block we committed against stayed canonical? If not, the
        // mutable hot store rolls back to the deepest surviving checkpoint (the only place a
        // reorg ever lands — sealed segments, once they exist, are strictly past finality).
        if next > 0 {
            match detect_reorg(&rpc, &store, next - 1).await {
                Ok(Some(ancestor)) => {
                    let removed = store.rollback_to(ancestor)?;
                    store.set_meta(LAST_BLOCK_KEY, &ancestor.to_string())?;
                    tracing::warn!("reorg detected: rolled back to block {ancestor} (removed {removed} entities)");
                    next = ancestor + 1;
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("reorg check skipped: {e:#}"),
            }
        }

        if next > tip {
            // Caught up to the tip — poll for new blocks.
            sleep_secs(2).await;
            continue;
        }

        let to = (next + WINDOW - 1).min(tip);
        match rpc.get_logs(&address, TRANSFER_TOPIC0, next, to).await {
            Ok(logs) => {
                let mut stored = 0usize;
                for log in &logs {
                    if let Some(t) = decode::transfer(log) {
                        let key = Store::entity_key(t.block_number, t.log_index);
                        let json = serde_json::to_string(&t)?;
                        store.put_entity(&key, &json)?;
                        stored += 1;
                    }
                }
                // Checkpoint the window boundary's canonical hash for future reorg detection.
                if let Ok(Some(hash)) = rpc.block_hash(to).await {
                    store.set_block_hash(to, &hash)?;
                }
                store.set_meta(LAST_BLOCK_KEY, &to.to_string())?;
                if stored > 0 {
                    tracing::info!("blocks {next}..={to}: +{stored} transfers (total {})", store.count()?);
                }
                next = to + 1;
            }
            Err(e) => {
                tracing::warn!("get_logs {next}..={to} failed: {e:#}; retrying");
                sleep_secs(3).await;
            }
        }
    }
}

/// If the checkpoint at `last` is no longer canonical, return the deepest checkpoint that still
/// is (the common ancestor to roll back to); otherwise None. Returns Some(0) if none survive.
async fn detect_reorg(rpc: &RpcClient, store: &Store, last: u64) -> Result<Option<u64>> {
    let stored = match store.get_block_hash(last)? {
        Some(h) => h,
        None => return Ok(None), // no checkpoint here (e.g. cold start) — nothing to verify
    };
    let canonical = match rpc.block_hash(last).await? {
        Some(h) => h,
        None => return Ok(None), // node can't answer right now; try again next tick
    };
    if stored == canonical {
        return Ok(None);
    }
    for (block, hash) in store.checkpoints_desc()? {
        if block >= last {
            continue;
        }
        if let Some(canon) = rpc.block_hash(block).await? {
            if canon == hash {
                return Ok(Some(block));
            }
        }
    }
    Ok(Some(0))
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}
