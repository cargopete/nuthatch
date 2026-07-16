//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::chains::{self, Finality};
use crate::chunker::{self, AdaptiveWindow};
use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::exposure::{self, ExposureView};
use crate::labels::{self, LabelSet};
use crate::metrics::METRICS;
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::screen::{self, LiveScreener, TransferRow};
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
/// Cold-start origin when a nest declares neither `start_block`s nor an explicit `--backfill`.
const DEFAULT_BACKFILL: u64 = 5_000;

/// `nuthatch dev` — the RPC front-end. Builds an RPC `Source` from the nest's `rpc_urls` and runs
/// the shared pipeline. The colocated-reth front-end (`nuthatch-node`, RFC-0003) builds an ExEx
/// `Source` instead and calls [`run`] directly — same core, different tip source.
pub async fn dev(args: DevArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    // Today: RPC polling. The indexer only sees `dyn Source`, so an ExEx tip source slots in here
    // with no change to anything downstream.
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(config.nest.rpc_urls.clone())?);
    run(
        source,
        dir,
        config,
        args.listen,
        args.backfill,
        args.seal_direct,
        args.concurrency,
    )
    .await
}

/// Run the indexing pipeline against any `Source` and serve the API — the source-agnostic entry both
/// front-ends share. Decode → hot store → seal → IVM → serve is identical regardless of whether tips
/// arrive by RPC polling or in-process from a reth ExEx.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    listen: String,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
) -> Result<()> {
    let store = Store::open(&dir.join(DB_FILE))?;
    // The decode registry drives all contracts; the indexer decodes every declared event of every
    // contract in the nest into per-table rows.
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, &config)?);
    let balances = BalanceView::start()?;
    let exposure = ExposureView::start()?;
    // Labels (RFC-0008 C1) are the annotation substrate the exposure view joins against. Loaded once
    // at startup from the content-addressed snapshots under `labels/`; empty when none were imported.
    let labels = Arc::new(labels::load(&dir));
    if !labels.is_empty() {
        tracing::info!(
            "loaded {} labeled address(es) for exposure tracking",
            labels.len()
        );
    }
    // Optional live sanctions screening (RFC-0008 C2). Absent unless the nest configures
    // `[screening].lists`; when present, every window's transfers are screened against the pure
    // component and `sanction_hit` annotations are stored + sealed alongside the transfers.
    let screener = Arc::new(screen::LiveScreener::from_config(
        &dir,
        &config.screening.lists,
    )?);

    // Warm restart: the derived views (balances, exposure) aren't persisted, so rebuild them from
    // stored facts before serving or ingesting. On a cold start there is nothing stored → no-op.
    if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
        if let Err(e) = rebuild_balances(&dir, &store, &registry, &balances) {
            tracing::warn!("balance view rebuild failed (will re-derive as it indexes): {e:#}");
        }
        if let Err(e) = rebuild_exposure(&dir, &store, &registry, &labels, &exposure) {
            tracing::warn!("exposure view rebuild failed (will re-derive as it indexes): {e:#}");
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
        exposure.clone(),
        labels.clone(),
        screener.clone(),
        finality,
        window,
        seal_direct,
        concurrency,
    ));

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.primary()?.address.clone(),
        chain: config.nest.chain.clone(),
        dir: dir.clone(),
        balances,
        exposure,
        tables: Arc::new(registry.schema()),
        sql_gate: Arc::new(tokio::sync::Semaphore::new(serve::SQL_MAX_CONCURRENCY)),
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
    // Adaptively size the getLogs range around the target response budget (RFC-0004 §2), starting
    // from the chain's default window — so dense and sparse ranges self-tune and provider result
    // caps are handled by shrink-and-retry rather than a hard failure.
    let mut chunker = AdaptiveWindow::for_window(window);
    while next <= to {
        let chunk_to = (next + chunker.window() - 1).min(to);
        let logs = match source.logs(addresses, topic0s, next, chunk_to).await {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
                logs
            }
            Err(e) if chunker::is_result_too_large(&e) => {
                chunker.too_large();
                tracing::debug!("range {next}..={chunk_to} too large; shrinking and retrying");
                continue; // retry the same `next` with a smaller window
            }
            Err(e) => return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}")),
        };
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

/// Concurrent-fetch variant of [`backfill_direct`]: up to `concurrency` window fetches are in flight
/// at once (overlapping the RPC round-trip latency that dominates once the storage path is cheap),
/// while results are consumed strictly **in block order** — so the buffered rows, the batch
/// boundaries, and therefore the sealed segments are identical to the sequential path. `buffered`
/// preserves input order, which is what makes concurrency safe for content-addressed sealing.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct_pipelined(
    source: &dyn Source,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
    concurrency: usize,
) -> Result<u64> {
    use futures::stream::StreamExt;

    let mut windows = Vec::new();
    let mut n = from;
    while n <= to {
        let chunk_to = (n + window - 1).min(to);
        windows.push((n, chunk_to));
        n = chunk_to + 1;
    }

    // Each window future fetches logs + timestamps and returns its decoded rows as JSON. Borrows
    // (`source`, `registry`, filters) are shared across the concurrent futures — fine, they run on
    // one task; `buffered` yields them back in window order.
    let mut stream = futures::stream::iter(windows)
        .map(|(w_from, w_to)| async move {
            let logs = source
                .logs(addresses, topic0s, w_from, w_to)
                .await
                .with_context(|| format!("getLogs {w_from}..={w_to}"))?;
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
            let ts = source.block_timestamps(&blocks).await.unwrap_or_default();
            let json: Vec<String> = rows
                .iter_mut()
                .map(|r| {
                    r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
                    r.to_json().to_string()
                })
                .collect();
            Ok::<(u64, Vec<String>), anyhow::Error>((w_to, json))
        })
        .buffered(concurrency.max(1));

    let mut buf: Vec<String> = Vec::new();
    let mut batch_from = from;
    let mut total = 0u64;
    while let Some(res) = stream.next().await {
        let (w_to, json) = res?;
        total += json.len() as u64;
        buf.extend(json);
        if buf.len() >= SEAL_DIRECT_BATCH {
            seal::seal_range(dir, &buf, batch_from, w_to)?;
            buf.clear();
            batch_from = w_to + 1;
        }
    }
    if !buf.is_empty() {
        seal::seal_range(dir, &buf, batch_from, to)?;
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
    backfill: Option<u64>,
    start_block: Option<u64>,
    dir: PathBuf,
    balances: BalanceView,
    exposure: ExposureView,
    labels: Arc<LabelSet>,
    screener: Arc<Option<LiveScreener>>,
    finality: Finality,
    window: u64,
    seal_direct: bool,
    concurrency: usize,
) -> Result<()> {
    // Phase 0 (cold start, `--seal-direct`): fast-seal the finalized history straight to Parquet,
    // bypassing the hot store, then rebuild the IVM view from those segments. The tip-following loop
    // below picks up from where this left off and handles the near-tip (un-finalized) window the
    // normal way. Nothing here can reorg — it is all strictly past finality.
    if seal_direct && store.get_meta(LAST_BLOCK_KEY)?.is_none() {
        let tip = source.tip().await?;
        let origin = cold_start_block(start_block, backfill, tip);
        let finalized_tag = match finality {
            Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
            Finality::Depth(_) => None,
        };
        let finalized_through = seal_ceiling(finality, tip, finalized_tag);
        if origin <= finalized_through {
            store.set_meta(START_BLOCK_KEY, &origin.to_string())?;
            tracing::info!(
                "seal-direct backfill: {origin}..={finalized_through} (tip {tip}, {concurrency}-way)…"
            );
            let sealed = backfill_direct_pipelined(
                source.as_ref(),
                &registry,
                &dir,
                &addresses,
                &topic0s,
                origin,
                finalized_through,
                window,
                concurrency,
            )
            .await?;
            store.set_meta(SEALED_THROUGH_KEY, &finalized_through.to_string())?;
            store.set_meta(LAST_BLOCK_KEY, &finalized_through.to_string())?;
            tracing::info!(
                "seal-direct backfill done: {sealed} rows sealed over {origin}..={finalized_through}"
            );
            if let Err(e) = rebuild_balances(&dir, &store, &registry, &balances) {
                tracing::warn!("balance rebuild after seal-direct failed: {e:#}");
            }
            if let Err(e) = rebuild_exposure(&dir, &store, &registry, &labels, &exposure) {
                tracing::warn!("exposure rebuild after seal-direct failed: {e:#}");
            }
        }
    }

    // Resume from the last committed block; on a cold start, backfill from the nest's earliest
    // vendored deployment block (full history) if it has one, else from `--backfill` behind the tip.
    let mut next = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
        None => {
            let tip = source.tip().await?;
            let start = cold_start_block(start_block, backfill, tip);
            store.set_meta(START_BLOCK_KEY, &start.to_string())?;
            let src = if backfill.is_none() && start_block.is_some() {
                " (from deployment)"
            } else {
                ""
            };
            tracing::info!("cold start: backfilling from block {start}{src} (tip {tip})");
            start
        }
    };

    // Adaptive getLogs sizing (RFC-0004 §2), seeded from the chain's default window.
    let mut chunker = AdaptiveWindow::for_window(window);
    loop {
        let tip = match source.tip().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("tip lookup failed: {e:#}; retrying");
                sleep_secs(3).await;
                continue;
            }
        };
        METRICS.set_tip(tip);

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
                    exposure.apply(exposure_retraction_batch(&doomed, &registry, &labels));

                    let removed = store.rollback_to(ancestor)?;
                    store.set_meta(LAST_BLOCK_KEY, &ancestor.to_string())?;
                    METRICS.inc_reorgs();
                    METRICS.set_last_block(ancestor);
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

        let to = (next + chunker.window() - 1).min(tip);
        match source.logs(&addresses, &topic0s, next, to).await {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
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
                let mut exp_deltas = Vec::new();
                // Transfers to screen this window (only collected when screening is on).
                let mut to_screen: Vec<TransferRow> = Vec::new();
                for row in &mut rows {
                    row.block_timestamp = timestamps.get(&row.block_number).copied().unwrap_or(0);
                    let key = Store::entity_key(row.block_number, row.log_index);
                    // Feed the IVM balance + exposure views for transfer rows (extracted before storing).
                    if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                        if let Some(v) = value.as_deref().and_then(|s| s.parse::<i128>().ok()) {
                            deltas.extend(views::transfer_deltas(&from, &to_addr, v, 1));
                            // Direct exposure to the labeled set (empty when neither side is labeled).
                            exp_deltas
                                .extend(exposure::exposure_deltas(&from, &to_addr, v, 1, &labels));
                        }
                        if screener.is_some() {
                            to_screen.push(TransferRow {
                                block_number: row.block_number,
                                log_index: row.log_index,
                                from: from.to_ascii_lowercase(),
                                to: to_addr.to_ascii_lowercase(),
                                value: value.unwrap_or_default(),
                                tx_hash: row.tx_hash.clone(),
                            });
                        }
                    }
                    // Every row is stored uniformly as typed JSON with a `table` field; per-table
                    // sealing groups by it.
                    store.put_entity(&key, &row.to_json().to_string())?;
                    stored += 1;
                }
                balances.apply(deltas);
                exposure.apply(exp_deltas);

                // Live sanctions screening (RFC-0008 C2): screen this window's transfers against the
                // configured list snapshots and store `sanction_hit` annotations. They share the
                // transfers' block keys, so they seal and roll back with the same range. Stored before
                // `maybe_seal` below so a freshly-finalized window seals its hits alongside its rows.
                if let Some(s) = screener.as_ref() {
                    let hits = s.screen_window(&to_screen);
                    for (key, ann) in &hits {
                        store.put_entity(key, &ann.to_string())?;
                    }
                    if !hits.is_empty() {
                        tracing::warn!(
                            "sanctions screening: {} hit(s) in {next}..={to}",
                            hits.len()
                        );
                    }
                }
                // Checkpoint the window boundary's canonical hash for future reorg detection.
                if let Ok(Some(hash)) = source.block_hash(to).await {
                    store.set_block_hash(to, &hash)?;
                }
                store.set_meta(LAST_BLOCK_KEY, &to.to_string())?;
                METRICS.set_last_block(to);
                METRICS.add_rows_decoded(stored as u64);
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
            Err(e) if chunker::is_result_too_large(&e) => {
                // Provider capped the response — shrink and retry the same range immediately.
                chunker.too_large();
                tracing::debug!("range {next}..={to} too large; shrinking and retrying");
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

/// Where a cold start begins backfilling. An explicit `--backfill N` always wins — "index the last N
/// blocks", overriding a vendored deploy block (this is what keeps the recent-history use working on
/// a nest that declares start blocks). Otherwise, the nest's earliest vendored `start_block` gives
/// full history from deployment; failing that, a default recent window. Pure, so it's unit-testable.
fn cold_start_block(start_block: Option<u64>, backfill: Option<u64>, tip: u64) -> u64 {
    match (backfill, start_block) {
        (Some(n), _) => tip.saturating_sub(n),
        (None, Some(b)) => b.min(tip),
        (None, None) => tip.saturating_sub(DEFAULT_BACKFILL),
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
            METRICS.set_sealed_through(ceiling);
            METRICS.add_rows_sealed(summary.rows as u64);
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
            METRICS.set_sealed_through(ceiling);
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

/// Build a weight −1 exposure retraction batch from rolled-back transfer rows (reorg). Reads each
/// table's (from, to, value) column names from the registry — they vary by token (USDC from/to/value,
/// WETH src/dst/wad) — then re-derives the same exposure deltas the live path fed, with weight −1, so
/// a reorged flag/exposure retracts exactly like a balance.
fn exposure_retraction_batch(
    entity_json: &[String],
    registry: &DecodeRegistry,
    labels: &LabelSet,
) -> exposure::ExposureBatch {
    // table → (from_col, to_col, value_col) for every transfer-shaped table.
    let cols: std::collections::HashMap<String, (String, String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns().map(|(f, t, v)| {
                (
                    d.table.clone(),
                    (f.to_string(), t.to_string(), v.to_string()),
                )
            })
        })
        .collect();

    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else {
            continue;
        };
        let Some(table) = v.get("table").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some((from_col, to_col, val_col)) = cols.get(table) else {
            continue; // not a transfer table
        };
        if let (Some(from), Some(to), Some(val)) = (
            v[from_col].as_str(),
            v[to_col].as_str(),
            v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
        ) {
            batch.extend(exposure::exposure_deltas(from, to, val, -1, labels));
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

/// Rebuild the derived exposure view on a warm restart (RFC-0008 C1), mirroring `rebuild_balances`:
/// cold (sealed) segments are folded to pre-summed (key, amount, count) aggregates directly in DuckDB
/// (joined against the `labels` view) and seeded; only the un-sealed hot tail is replayed transfer by
/// transfer. Hot and cold are disjoint (sealed rows are pruned), so nothing is double-counted. With no
/// labels imported this is a no-op — there is nothing to be exposed *to*.
fn rebuild_exposure(
    dir: &std::path::Path,
    store: &Store,
    registry: &DecodeRegistry,
    labels: &LabelSet,
    exposure: &ExposureView,
) -> Result<()> {
    if labels.is_empty() {
        return Ok(());
    }
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

    let mut batch: exposure::ExposureBatch = Vec::new();

    // Cold seed: pre-summed exposure per (address, label, direction), folded in DuckDB.
    let mut cold = 0usize;
    for (table, from_col, to_col, val_col) in &transfer_tables {
        match crate::analytics::cold_exposure(dir, table, from_col, to_col, val_col) {
            Ok(rows) => {
                cold += rows.len();
                for (key, amount, count) in rows {
                    batch.push(exposure::seed_item(key, amount, count));
                }
            }
            Err(e) => tracing::debug!("no cold exposure seed for {table}: {e:#}"),
        }
    }

    // Hot replay: the un-sealed tip transfers, re-derived through the same delta path.
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
                let d = exposure::exposure_deltas(from, to, val, 1, labels);
                if !d.is_empty() {
                    hot += 1;
                    batch.extend(d);
                }
            }
        }
    }

    if batch.is_empty() {
        return Ok(());
    }
    exposure.apply(batch);
    exposure.flush();
    tracing::info!(
        "rebuilt exposure view: {} entries ({cold} cold-seeded + {hot} hot transfer(s) replayed)",
        exposure.entries()
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
    fn cold_start_origin_policy() {
        // No --backfill + vendored start_block → full history from deployment (clamped to tip).
        assert_eq!(
            cold_start_block(Some(42_449_585), None, 484_000_000),
            42_449_585
        );
        assert_eq!(cold_start_block(Some(999), None, 500), 500); // clamp to tip
                                                                 // Explicit --backfill always wins — recent-history mode, even with a start_block present.
        assert_eq!(
            cold_start_block(Some(42_449_585), Some(200), 484_000_000),
            483_999_800
        );
        assert_eq!(cold_start_block(None, Some(5_000), 1_000_000), 995_000);
        assert_eq!(cold_start_block(None, Some(5_000), 100), 0); // no underflow
                                                                 // Neither → a default recent window.
        assert_eq!(cold_start_block(None, None, 1_000_000), 995_000);
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

    // A Source backed by canned logs — lets us drive both backfill paths deterministically, offline.
    struct MockSource {
        logs: Vec<crate::rpc::Log>,
    }

    #[async_trait::async_trait]
    impl Source for MockSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _a: &[String],
            _t: &[String],
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
        }
    }

    fn transfer_log(block: u64, li: u64) -> crate::rpc::Log {
        crate::rpc::Log {
            address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            topics: vec![
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".into(),
                "0x000000000000000000000000943f303a8019652d3a14b29954b2d780dde42ca3".into(),
                "0x000000000000000000000000db5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0".into(),
            ],
            data: "0x000000000000000000000000000000000000000000000000000000001cd4ad20".into(),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xtx".into(),
            log_index: li,
        }
    }

    /// RFC-0004 §3: the pipelined (concurrent-fetch) backfill produces **byte-identical** segments to
    /// the sequential path — concurrency overlaps latency without changing the output.
    #[tokio::test]
    async fn pipelined_backfill_matches_sequential() {
        use crate::registry::{ContractSpec, DecodeRegistry};
        const ERC20: &str = r#"[{"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#;
        let abi: alloy_json_abi::JsonAbi = serde_json::from_str(ERC20).unwrap();
        let addr: alloy_primitives::Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let reg = DecodeRegistry::build(vec![ContractSpec {
            alias: "usdc".into(),
            address: addr,
            abi,
        }])
        .unwrap();

        let logs: Vec<_> = (10u64..40)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        let source = MockSource { logs };
        let addresses: Vec<String> = reg
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect();
        let topic0s: Vec<String> = reg
            .topic0s()
            .iter()
            .map(|t| format!("0x{}", hex::encode(t)))
            .collect();

        let d_seq = tempfile::tempdir().unwrap();
        let seq = backfill_direct(&source, &reg, d_seq.path(), &addresses, &topic0s, 10, 39, 5)
            .await
            .unwrap();
        let d_pipe = tempfile::tempdir().unwrap();
        let pipe = backfill_direct_pipelined(
            &source,
            &reg,
            d_pipe.path(),
            &addresses,
            &topic0s,
            10,
            39,
            5,
            8,
        )
        .await
        .unwrap();

        assert_eq!(seq, pipe, "same event count");
        assert!(seq > 0);
        let hashes = |dir: &std::path::Path| -> Vec<(String, String)> {
            let m = seal::load_manifest(dir).unwrap();
            m.tables
                .iter()
                .flat_map(|(t, segs)| segs.iter().map(move |s| (t.clone(), s.hash.clone())))
                .collect()
        };
        assert_eq!(
            hashes(d_seq.path()),
            hashes(d_pipe.path()),
            "concurrency must not change the sealed bytes"
        );
    }
}
