//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::decode::{self, TRANSFER_TOPIC0};
use crate::rpc::RpcClient;
use crate::seal;
use crate::source::Source;
use crate::serve;
use crate::store::Store;
use crate::views::{self, BalanceView};

/// Block window per `eth_getLogs` call. Kept small so high-volume contracts (e.g. USDC, ~thousands
/// of Transfers per handful of blocks) stay under public-RPC result-size caps.
const WINDOW: u64 = 20;
/// Blocks behind the tip a block must be before we treat it as final and seal it. A conservative
/// proxy for Ethereum finality (~2 epochs); real finality signals come with the ExEx mode.
const FINALITY_DEPTH: u64 = 64;
const LAST_BLOCK_KEY: &str = "last_block";
const SEALED_THROUGH_KEY: &str = "sealed_through";
const START_BLOCK_KEY: &str = "start_block";

pub async fn dev(args: DevArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    let store = Store::open(&dir.join(DB_FILE))?;
    // Today: RPC polling. The indexer only sees `dyn Source`, so an ExEx tip source (feature = "exex")
    // slots in here with no change to anything downstream.
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(config.rpc_urls.clone())?);
    let balances = BalanceView::start()?;

    tracing::info!(
        "indexing {} on {} — Transfer events only (skeleton)",
        config.address,
        config.chain
    );

    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        source.clone(),
        store.clone(),
        config.address.clone(),
        args.backfill,
        dir.clone(),
        balances.clone(),
    ));

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.address.clone(),
        chain: config.chain.clone(),
        dir: dir.clone(),
        balances,
    };
    serve::run(&args.listen, app_state).await?;

    ingest.abort();
    Ok(())
}

async fn index_loop(
    source: Arc<dyn Source>,
    store: Store,
    address: String,
    backfill: u64,
    dir: PathBuf,
    balances: BalanceView,
) -> Result<()> {
    // Resume from the last committed block, else start `backfill` blocks behind the tip.
    let mut next = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
        None => {
            let tip = source.tip().await?;
            let start = tip.saturating_sub(backfill);
            store.set_meta(START_BLOCK_KEY, &start.to_string())?;
            tracing::info!("cold start: backfilling from block {start} (tip {tip})");
            start
        }
    };

    loop {
        let tip = match source.tip().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("tip lookup failed: {e:#}; retrying");
                sleep_secs(3).await;
                continue;
            }
        };

        // Reorg check: has the last block we committed against stayed canonical? If not, the
        // mutable hot store rolls back to the deepest surviving checkpoint (the only place a
        // reorg ever lands — sealed segments, once they exist, are strictly past finality).
        if next > 0 {
            match detect_reorg(source.as_ref(), &store, next - 1).await {
                Ok(Some(ancestor)) => {
                    // Retract the rolled-back transfers from the IVM view *before* dropping them
                    // from the hot store — a reorg is just the same facts re-fed with weight −1.
                    let last_indexed = store
                        .get_meta(LAST_BLOCK_KEY)?
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(ancestor);
                    let doomed = store.entities_in_range(ancestor + 1, last_indexed)?;
                    balances.apply(retraction_batch(&doomed));

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
        match source.logs(&address, TRANSFER_TOPIC0, next, to).await {
            Ok(logs) => {
                let mut stored = 0usize;
                let mut deltas = Vec::new();
                for log in &logs {
                    if let Some(t) = decode::transfer(log) {
                        let key = Store::entity_key(t.block_number, t.log_index);
                        let json = serde_json::to_string(&t)?;
                        store.put_entity(&key, &json)?;
                        stored += 1;
                        // Feed the IVM balance view (weight +1). Values that don't fit i64 are
                        // skipped for now — the view accumulates in i64 base units.
                        if let Some(v) = t.value.as_deref().and_then(|s| s.parse::<i64>().ok()) {
                            deltas.extend(views::transfer_deltas(&t.from, &t.to, v, 1));
                        }
                    }
                }
                balances.apply(deltas);
                // Checkpoint the window boundary's canonical hash for future reorg detection.
                if let Ok(Some(hash)) = source.block_hash(to).await {
                    store.set_block_hash(to, &hash)?;
                }
                store.set_meta(LAST_BLOCK_KEY, &to.to_string())?;
                if stored > 0 {
                    tracing::info!("blocks {next}..={to}: +{stored} transfers (total {})", store.count()?);
                }
                next = to + 1;

                // Seal any newly-finalized range to an immutable Parquet segment.
                if let Err(e) = maybe_seal(&dir, &store, tip) {
                    tracing::warn!("sealing failed: {e:#}");
                }
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
async fn detect_reorg(source: &dyn Source, store: &Store, last: u64) -> Result<Option<u64>> {
    let stored = match store.get_block_hash(last)? {
        Some(h) => h,
        None => return Ok(None), // no checkpoint here (e.g. cold start) — nothing to verify
    };
    let canonical = match source.block_hash(last).await? {
        Some(h) => h,
        None => return Ok(None), // source can't answer right now; try again next tick
    };
    if stored == canonical {
        return Ok(None);
    }
    for (block, hash) in store.checkpoints_desc()? {
        if block >= last {
            continue;
        }
        if let Some(canon) = source.block_hash(block).await? {
            if canon == hash {
                return Ok(Some(block));
            }
        }
    }
    Ok(Some(0))
}

/// Seal every indexed block that has passed finality but isn't sealed yet, advancing the
/// `sealed_through` watermark. The hot store is deliberately NOT pruned here (that lands with the
/// DuckDB serving path), so point-reads keep working against redb meanwhile.
fn maybe_seal(dir: &std::path::Path, store: &Store, tip: u64) -> Result<()> {
    if tip < FINALITY_DEPTH {
        return Ok(());
    }
    let finalized_through = tip - FINALITY_DEPTH;
    let last_indexed = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")?,
        None => return Ok(()),
    };
    let ceiling = finalized_through.min(last_indexed);

    let from = match store.get_meta(SEALED_THROUGH_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt sealed_through")? + 1,
        None => store
            .get_meta(START_BLOCK_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
    };
    if ceiling < from {
        return Ok(()); // nothing new has finalized
    }

    let entities = store.entities_in_range(from, ceiling)?;
    match seal::seal_range(dir, &entities, from, ceiling)? {
        Some(seg) => {
            // Watermark advances only after the segment is durably on disk + catalogued; only then
            // is it safe to prune the now-redundant rows from the hot store.
            store.set_meta(SEALED_THROUGH_KEY, &ceiling.to_string())?;
            let pruned = store.prune_range(from, ceiling)?;
            tracing::info!(
                "sealed segment {}… blocks {from}..={ceiling} ({} rows); pruned {pruned} from hot",
                &seg.hash[..12],
                seg.rows
            );
        }
        None => {
            // Finalized range with no transfers — just advance the watermark.
            store.set_meta(SEALED_THROUGH_KEY, &ceiling.to_string())?;
            tracing::debug!("blocks {from}..={ceiling} finalized with no transfers; watermark advanced");
        }
    }
    Ok(())
}

/// Build a weight −1 retraction batch from stored transfer JSON (used on reorg rollback).
fn retraction_batch(entity_json: &[String]) -> Vec<dbsp::utils::Tup2<dbsp::utils::Tup2<String, i64>, i64>> {
    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else { continue };
        let (Some(from), Some(to)) = (v["from"].as_str(), v["to"].as_str()) else { continue };
        if let Some(val) = v["value"].as_str().and_then(|s| s.parse::<i64>().ok()) {
            batch.extend(views::transfer_deltas(from, to, val, -1));
        }
    }
    batch
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}
