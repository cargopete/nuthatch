//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::seal;
use crate::serve;
use crate::source::Source;
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
    // The decode registry drives all contracts; the indexer decodes every declared event of every
    // contract in the nest into per-table rows.
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, &config)?);
    // Today: RPC polling. The indexer only sees `dyn Source`, so an ExEx tip source (feature = "exex")
    // slots in here with no change to anything downstream.
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(config.nest.rpc_urls.clone())?);
    let balances = BalanceView::start()?;

    // The combined `eth_getLogs` filter: all contract addresses, matching any registered topic0.
    let addresses: Vec<String> = registry
        .addresses()
        .iter()
        .map(|a| format!("0x{}", hex::encode(a)))
        .collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    tracing::info!(
        "indexing nest '{}' on {}: {} contract(s), {} table(s), {} anonymous skipped, registry {}…",
        config.nest.name,
        config.nest.chain,
        config.contracts.len(),
        registry.tables().len(),
        registry.skipped_anonymous(),
        &hex::encode(registry.hash())[..12],
    );

    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        source.clone(),
        store.clone(),
        registry.clone(),
        addresses,
        topic0s,
        args.backfill,
        dir.clone(),
        balances.clone(),
    ));

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.primary()?.address.clone(),
        chain: config.nest.chain.clone(),
        dir: dir.clone(),
        balances,
        tables: Arc::new(registry.schema()),
    };
    serve::run(&args.listen, app_state).await?;

    ingest.abort();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn index_loop(
    source: Arc<dyn Source>,
    store: Store,
    registry: Arc<DecodeRegistry>,
    addresses: Vec<String>,
    topic0s: Vec<String>,
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
        match source.logs(&addresses, &topic0s, next, to).await {
            Ok(logs) => {
                let mut stored = 0usize;
                let mut deltas = Vec::new();
                for log in &logs {
                    let row = match registry.decode(log) {
                        Ok(Some(r)) => r,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::debug!("decode skipped: {e:#}");
                            continue;
                        }
                    };
                    let key = Store::entity_key(row.block_number, row.log_index);
                    // Feed the IVM balance view for transfer rows (extracted before storing).
                    if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                        if let Some(v) = value.as_deref().and_then(|s| s.parse::<i64>().ok()) {
                            deltas.extend(views::transfer_deltas(&from, &to_addr, v, 1));
                        }
                    }
                    // Every row is stored uniformly as typed JSON with a `table` field; per-table
                    // sealing groups by it.
                    store.put_entity(&key, &row.to_json().to_string())?;
                    stored += 1;
                }
                balances.apply(deltas);
                // Checkpoint the window boundary's canonical hash for future reorg detection.
                if let Ok(Some(hash)) = source.block_hash(to).await {
                    store.set_block_hash(to, &hash)?;
                }
                store.set_meta(LAST_BLOCK_KEY, &to.to_string())?;
                if stored > 0 {
                    tracing::info!(
                        "blocks {next}..={to}: +{stored} rows (total {})",
                        store.count()?
                    );
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
    // Every table in the range seals together (per-table segments), so once sealing succeeds the
    // whole range is safe to prune from the hot store — the watermark stays global.
    match seal::seal_range(dir, &entities, from, ceiling)? {
        Some(summary) => {
            store.set_meta(SEALED_THROUGH_KEY, &ceiling.to_string())?;
            let pruned = store.prune_range(from, ceiling)?;
            tracing::info!(
                "sealed blocks {from}..={ceiling}: {} rows across {} table(s); pruned {pruned} from hot",
                summary.rows,
                summary.tables
            );
        }
        None => {
            // Finalized range with no transfers — just advance the watermark.
            store.set_meta(SEALED_THROUGH_KEY, &ceiling.to_string())?;
            tracing::debug!(
                "blocks {from}..={ceiling} finalized with no transfers; watermark advanced"
            );
        }
    }
    Ok(())
}

/// Build a weight −1 retraction batch from stored transfer JSON (used on reorg rollback).
fn retraction_batch(
    entity_json: &[String],
) -> Vec<dbsp::utils::Tup2<dbsp::utils::Tup2<String, i64>, i64>> {
    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else {
            continue;
        };
        // Only transfer rows were fed to the balance view; retract only those.
        let is_transfer = v
            .get("table")
            .and_then(|t| t.as_str())
            .map(|t| t.ends_with("__transfer"))
            .unwrap_or(false);
        if !is_transfer {
            continue;
        }
        let (Some(from), Some(to)) = (v["from"].as_str(), v["to"].as_str()) else {
            continue;
        };
        if let Some(val) = v["value"].as_str().and_then(|s| s.parse::<i64>().ok()) {
            batch.extend(views::transfer_deltas(from, to, val, -1));
        }
    }
    batch
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}
