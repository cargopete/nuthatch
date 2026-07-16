//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::chains::{self, Finality};
use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::seal;
use crate::serve;
use crate::source::Source;
use crate::store::Store;
use crate::views::{self, BalanceView};

/// Defaults for a chain not in the registry (a custom `rpc_urls` nest): a small `eth_getLogs` window
/// and a conservative Ethereum-style finality depth.
const DEFAULT_WINDOW: u64 = 20;
const DEFAULT_FINALITY: Finality = Finality::Depth(64);
const LAST_BLOCK_KEY: &str = "last_block";
const SEALED_THROUGH_KEY: &str = "sealed_through";
const START_BLOCK_KEY: &str = "start_block";

/// `nuthatch dev` — the RPC front-end. Builds an RPC `Source` from the nest's `rpc_urls` and runs
/// the shared pipeline. The colocated-reth front-end (`nuthatch-node`, RFC-0003) builds an ExEx
/// `Source` instead and calls [`run`] directly — same core, different tip source.
pub async fn dev(args: DevArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    // Today: RPC polling. The indexer only sees `dyn Source`, so an ExEx tip source slots in here
    // with no change to anything downstream.
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(config.nest.rpc_urls.clone())?);
    run(source, dir, config, args.listen, args.backfill).await
}

/// Run the indexing pipeline against any `Source` and serve the API — the source-agnostic entry both
/// front-ends share. Decode → hot store → seal → IVM → serve is identical regardless of whether tips
/// arrive by RPC polling or in-process from a reth ExEx.
pub async fn run(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    listen: String,
    backfill: u64,
) -> Result<()> {
    let store = Store::open(&dir.join(DB_FILE))?;
    // The decode registry drives all contracts; the indexer decodes every declared event of every
    // contract in the nest into per-table rows.
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, &config)?);
    let balances = BalanceView::start()?;

    // Warm restart: the balance view is derived, not persisted, so rebuild it from stored facts
    // before serving or ingesting. On a cold start there is nothing stored and this is a no-op.
    if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
        if let Err(e) = rebuild_balances(&dir, &store, &registry, &balances) {
            tracing::warn!("balance view rebuild failed (will re-derive as it indexes): {e:#}");
        }
    }

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

    // Per-chain policy from the registry; a custom (unregistered) chain falls back to defaults.
    let (finality, window) = match chains::lookup(&config.nest.chain) {
        Some(c) => (c.finality, c.log_window),
        None => (DEFAULT_FINALITY, DEFAULT_WINDOW),
    };

    tracing::info!(
        "indexing nest '{}' on {}: {} contract(s), {} table(s), {} anonymous skipped, finality {:?}, window {}, registry {}…",
        config.nest.name,
        config.nest.chain,
        config.contracts.len(),
        registry.tables().len(),
        registry.skipped_anonymous(),
        finality,
        window,
        &hex::encode(registry.hash())[..12],
    );

    // A nest that vendors deployment blocks backfills from the earliest one (full history from
    // deployment); otherwise a cold start falls back to the `--backfill` tip offset.
    let start_block = config.contracts.iter().filter_map(|c| c.start_block).min();

    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        source.clone(),
        store.clone(),
        registry.clone(),
        addresses,
        topic0s,
        backfill,
        start_block,
        dir.clone(),
        balances.clone(),
        finality,
        window,
    ));

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.primary()?.address.clone(),
        chain: config.nest.chain.clone(),
        dir: dir.clone(),
        balances,
        tables: Arc::new(registry.schema()),
    };
    serve::run(&listen, app_state).await?;

    ingest.abort();
    Ok(())
}

/// Batch size (rows) at which `backfill_direct` flushes a sealed segment — bounds RSS during a
/// from-history backfill regardless of how long the range is.
const SEAL_DIRECT_BATCH: usize = 20_000;

/// Stream a *finalized* block range straight to sealed Parquet, bypassing the hot store entirely
/// (RFC-0004 §1): decode → buffered rows → content-addressed segments. No redb write, no read-back,
/// no prune — the churn a from-history backfill otherwise pays for every historical row. Rows carry
/// the same implicit columns (incl. `block_timestamp`) as the hot path and are sealed via the *same*
/// [`seal::seal_range`], so a given range yields byte-identical segments regardless of path (the
/// determinism guarantee, asserted in seal's path-equivalence test). The bounded buffer caps RSS by
/// construction. Only valid for ranges already past finality — there is no reorg risk to roll back.
/// Returns the number of rows sealed.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct(
    source: &dyn Source,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
) -> Result<u64> {
    let mut buf: Vec<String> = Vec::new();
    let mut batch_from = from;
    let mut next = from;
    let mut total = 0u64;
    while next <= to {
        let chunk_to = (next + window - 1).min(to);
        let logs = source
            .logs(addresses, topic0s, next, chunk_to)
            .await
            .with_context(|| format!("getLogs {next}..={chunk_to}"))?;
        let mut rows: Vec<_> = logs
            .iter()
            .filter_map(|log| match registry.decode(log) {
                Ok(Some(r)) => Some(r),
                Ok(None) => None,
                Err(e) => {
                    tracing::debug!("decode skipped: {e:#}");
                    None
                }
            })
            .collect();
        // Stamp block_timestamp (batched), identical to the hot path, so segments match byte-for-byte.
        let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let ts = source.block_timestamps(&blocks).await.unwrap_or_default();
        for r in &mut rows {
            r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
            buf.push(r.to_json().to_string());
            total += 1;
        }
        next = chunk_to + 1;

        // Flush a segment once the buffer fills or the range ends. `[batch_from, chunk_to]` covers
        // every window accumulated since the last flush.
        if buf.len() >= SEAL_DIRECT_BATCH || next > to {
            if !buf.is_empty() {
                seal::seal_range(dir, &buf, batch_from, chunk_to)?;
                buf.clear();
            }
            batch_from = next;
        }
    }
    Ok(total)
}

#[allow(clippy::too_many_arguments)]
async fn index_loop(
    source: Arc<dyn Source>,
    store: Store,
    registry: Arc<DecodeRegistry>,
    addresses: Vec<String>,
    topic0s: Vec<String>,
    backfill: u64,
    start_block: Option<u64>,
    dir: PathBuf,
    balances: BalanceView,
    finality: Finality,
    window: u64,
) -> Result<()> {
    // Resume from the last committed block; on a cold start, backfill from the nest's earliest
    // vendored deployment block (full history) if it has one, else from `--backfill` behind the tip.
    let mut next = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
        None => {
            let tip = source.tip().await?;
            let start = cold_start_block(start_block, backfill, tip);
            store.set_meta(START_BLOCK_KEY, &start.to_string())?;
            match start_block {
                Some(b) => {
                    tracing::info!("cold start: backfilling from deployment block {b} (tip {tip})")
                }
                None => {
                    tracing::info!("cold start: backfilling from block {start} (tip {tip})")
                }
            }
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

        let to = (next + window - 1).min(tip);
        match source.logs(&addresses, &topic0s, next, to).await {
            Ok(logs) => {
                // Decode first so we know which blocks actually produced rows, then fetch just those
                // blocks' timestamps in one batch (cheap even for a dense window) and stamp each row.
                let mut rows: Vec<_> = logs
                    .iter()
                    .filter_map(|log| match registry.decode(log) {
                        Ok(Some(r)) => Some(r),
                        Ok(None) => None,
                        Err(e) => {
                            tracing::debug!("decode skipped: {e:#}");
                            None
                        }
                    })
                    .collect();
                let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
                blocks.sort_unstable();
                blocks.dedup();
                let timestamps = match source.block_timestamps(&blocks).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!("block timestamps unavailable: {e:#}");
                        std::collections::HashMap::new()
                    }
                };

                let mut stored = 0usize;
                let mut deltas = Vec::new();
                for row in &mut rows {
                    row.block_timestamp = timestamps.get(&row.block_number).copied().unwrap_or(0);
                    let key = Store::entity_key(row.block_number, row.log_index);
                    // Feed the IVM balance view for transfer rows (extracted before storing).
                    if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                        if let Some(v) = value.as_deref().and_then(|s| s.parse::<i128>().ok()) {
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

                // The highest block considered final under this chain's policy. For an L2 with the
                // `finalized` tag we ask the node; otherwise (and on tag failure) it's a fixed depth.
                let finalized_tag = match finality {
                    Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
                    Finality::Depth(_) => None,
                };
                let finalized_through = seal_ceiling(finality, tip, finalized_tag);

                // Seal any newly-finalized range to an immutable Parquet segment.
                if let Err(e) = maybe_seal(&dir, &store, finalized_through) {
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

/// Where a cold start begins backfilling: the nest's earliest vendored deployment block (clamped to
/// the tip) when present — full history from deployment — else `--backfill` blocks behind the tip.
/// Pure, so the origin policy is unit-testable.
fn cold_start_block(start_block: Option<u64>, backfill: u64, tip: u64) -> u64 {
    match start_block {
        Some(b) => b.min(tip),
        None => tip.saturating_sub(backfill),
    }
}

/// The highest block safe to seal under `finality`: the `finalized` tag when the chain uses it and
/// the node serves it, else a fixed depth below the tip. Pure, so the policy is unit-testable.
fn seal_ceiling(finality: Finality, tip: u64, finalized_tag: Option<u64>) -> u64 {
    match finality {
        Finality::Depth(d) => tip.saturating_sub(d),
        Finality::FinalizedTag { fallback_depth } => match finalized_tag {
            Some(n) => n.min(tip),
            None => tip.saturating_sub(fallback_depth),
        },
    }
}

/// Seal every indexed block up to `finalized_through` (the finality-safe ceiling) that isn't sealed
/// yet, advancing the `sealed_through` watermark and pruning the sealed range from the hot store.
fn maybe_seal(dir: &std::path::Path, store: &Store, finalized_through: u64) -> Result<()> {
    if finalized_through == 0 {
        return Ok(());
    }
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
fn retraction_batch(entity_json: &[String]) -> views::WeightedBatch {
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
        if let Some(val) = v["value"].as_str().and_then(|s| s.parse::<i128>().ok()) {
            batch.extend(views::transfer_deltas(from, to, val, -1));
        }
    }
    batch
}

/// Rebuild the in-memory IVM balance view from stored facts on a warm restart. The view is derived
/// state, not durable state — so rather than persist it (and risk drift from the canonical store),
/// we reconstruct it from the facts that *are* durable, using the same circuit that maintains it
/// live. Cold (sealed, immutable) segments are folded to one net-per-address row directly in DuckDB
/// — no need to replay millions of transfers — and only the small un-sealed hot tail is replayed
/// transfer-by-transfer. Hot and cold are disjoint (sealed rows are pruned from hot), so nothing is
/// double-counted; the result is identical to a view grown from genesis.
fn rebuild_balances(
    dir: &std::path::Path,
    store: &Store,
    registry: &DecodeRegistry,
    balances: &BalanceView,
) -> Result<()> {
    // Each transfer table with its (from, to, value) column names — which vary by token (USDC:
    // from/to/value; WETH: src/dst/wad), so we read them from the registry, never hardcode them.
    let transfer_tables: Vec<(String, String, String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, t, v)| (d.table.clone(), f.to_string(), t.to_string(), v.to_string()))
        })
        .collect();
    if transfer_tables.is_empty() {
        return Ok(());
    }

    let mut batch: views::WeightedBatch = Vec::new();

    // Cold seed: net balance per address, summed in DuckDB (HUGEINT = i128). A table with no sealed
    // segment yet has no view — that just means it has nothing cold to seed, so skip on error.
    let mut cold_addrs = 0usize;
    for (table, from_col, to_col, val_col) in &transfer_tables {
        match crate::analytics::net_balances(dir, table, from_col, to_col, val_col) {
            Ok(nets) => {
                cold_addrs += nets.len();
                for (addr, net) in nets {
                    batch.push(views::seed_delta(addr, net));
                }
            }
            Err(e) => tracing::debug!("no cold seed for {table}: {e:#}"),
        }
    }

    // Hot replay: the un-sealed tip transfers, fed through the circuit exactly as the live loop does.
    let mut hot = 0usize;
    for (table, from_col, to_col, val_col) in &transfer_tables {
        for raw in store.recent_by_table(table, usize::MAX).unwrap_or_default() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if let (Some(from), Some(to), Some(val)) = (
                v[from_col].as_str(),
                v[to_col].as_str(),
                v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
            ) {
                batch.extend(views::transfer_deltas(from, to, val, 1));
                hot += 1;
            }
        }
    }

    if batch.is_empty() {
        return Ok(());
    }
    balances.apply(batch);
    balances.flush();
    tracing::info!(
        "rebuilt balance view: {} holders ({cold_addrs} cold-seeded net(s) + {hot} hot transfer(s) replayed)",
        balances.holders()
    );
    Ok(())
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_prefers_vendored_deploy_block() {
        // A vendored start_block wins (full history), clamped to the tip.
        assert_eq!(
            cold_start_block(Some(42_449_585), 5_000, 484_000_000),
            42_449_585
        );
        assert_eq!(cold_start_block(Some(999), 5_000, 500), 500); // clamp to tip
                                                                  // No start_block → fall back to the --backfill offset.
        assert_eq!(cold_start_block(None, 5_000, 1_000_000), 995_000);
        assert_eq!(cold_start_block(None, 5_000, 100), 0); // no underflow
    }

    #[test]
    fn depth_finality_seals_behind_the_tip() {
        assert_eq!(seal_ceiling(Finality::Depth(64), 1000, None), 936);
        // Never underflow near genesis.
        assert_eq!(seal_ceiling(Finality::Depth(64), 10, None), 0);
    }

    #[test]
    fn finalized_tag_is_used_when_present_else_falls_back() {
        let f = Finality::FinalizedTag {
            fallback_depth: 1800,
        };
        // Tag present: seal up to it (clamped to tip).
        assert_eq!(seal_ceiling(f, 10_000, Some(8_500)), 8_500);
        assert_eq!(seal_ceiling(f, 10_000, Some(10_050)), 10_000);
        // Tag absent (endpoint doesn't serve it): fixed-depth fallback.
        assert_eq!(seal_ceiling(f, 10_000, None), 8_200);
    }
}
