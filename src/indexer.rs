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

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}
