//! `nuthatch dev` — the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::alerts::{self, AlertRouter};
use crate::chains::{self, Finality};
use crate::chunker::{self, AdaptiveWindow};
use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::exposure::{self, ExposureView};
use crate::factory::{ChildRegistry, FactorySet};
use crate::labels::{self, LabelSet};
use crate::metrics::METRICS;
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::screen::{self, LiveScreener, TransferRow};
use crate::seal;
use crate::serve;
use crate::source::Source;
use crate::store::Store;
use crate::velocity::{self, VelocityView};
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
    // with no change to anything downstream. `--rpc` overrides are tried ahead of the configured
    // endpoints without touching the nest's config on disk.
    let rpc_urls = crate::rpc::merge_rpcs(&args.rpc, config.nest.rpc_urls.clone());
    let endpoint_count = rpc_urls.len();
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(rpc_urls)?);
    // Guard the single-endpoint backfill deadlock (see `safe_backfill_concurrency`).
    let concurrency = safe_backfill_concurrency(endpoint_count, args.concurrency);
    if concurrency < args.concurrency {
        tracing::warn!(
            "single RPC endpoint: capping seal-direct backfill concurrency {} → {} (high concurrency \
             to one host can stall the runtime); configure multiple rpc_urls for a parallel backfill",
            args.concurrency,
            concurrency
        );
    }
    run(
        source,
        dir,
        config,
        args.listen,
        args.backfill,
        args.seal_direct,
        concurrency,
        args.window,
        args.no_admin,
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
    window_override: Option<u64>,
    no_admin: bool,
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

    // Optional threshold & velocity flags (RFC-0008 C3). Threshold flags are per-transfer stored
    // annotations (block-keyed → roll back with their transfer); velocity is a DBSP windowed view
    // (rebuilt on restart like balances/exposure).
    let threshold = config.flags.threshold_amount();
    let velocity_cfg = config.flags.velocity();
    let velocity = VelocityView::start()?;
    if threshold.is_some() || velocity_cfg.is_some() {
        tracing::info!("flags enabled: threshold={threshold:?}, velocity={velocity_cfg:?}");
    }

    // Warm restart: the derived views (balances, exposure, velocity) aren't persisted, so rebuild
    // them from stored facts before serving or ingesting. Cold start → nothing stored → no-op.
    if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
        if let Err(e) = rebuild_balances(&dir, &store, &registry, &balances) {
            tracing::warn!("balance view rebuild failed (will re-derive as it indexes): {e:#}");
        }
        if let Err(e) = rebuild_exposure(&dir, &store, &registry, &labels, &exposure) {
            tracing::warn!("exposure view rebuild failed (will re-derive as it indexes): {e:#}");
        }
        if let Some((_, w)) = velocity_cfg {
            if let Err(e) = rebuild_velocity(&dir, &store, &registry, w, &velocity) {
                tracing::warn!(
                    "velocity view rebuild failed (will re-derive as it indexes): {e:#}"
                );
            }
        }
    }

    // Factory rules (RFC-0009): validated at load. A factory nest discovers child contracts at
    // runtime, so the tip loop fetches topic0-only (empty address filter) — a child created and
    // traded in the same block is then already in hand, no extra RPC.
    let factory = {
        let fs = FactorySet::build(&config)?;
        if fs.is_empty() {
            None
        } else {
            tracing::info!(
                "factory nest: {} template(s), {} rule(s) — topic0-only tip fetch, children discovered at runtime",
                config.templates.len(),
                config.factories.len()
            );
            Some(Arc::new(fs))
        }
    };

    // The combined `eth_getLogs` filter: contract addresses (empty for a factory nest → topic0-only),
    // matching any registered topic0 (contract + template events).
    let addresses: Vec<String> = if factory.is_some() {
        Vec::new()
    } else {
        registry
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect()
    };
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    // Per-chain policy from the registry; a custom (unregistered) chain falls back to defaults.
    let (finality, chain_window) = match chains::lookup(&config.nest.chain) {
        Some(c) => (c.finality, c.log_window),
        None => (DEFAULT_FINALITY, DEFAULT_WINDOW),
    };
    // A `--window` override wins over the chain default (for sparse-contract long backfills).
    let window = effective_window(window_override, chain_window);

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

    // Optional alert sinks (RFC-0008 C5) + user webhooks (RFC-0010 Part B) — two producers, one
    // shared delivery engine. The worker drains the durable outbox on its own task, decoupled from
    // indexing, so a slow/dead endpoint never blocks the loop.
    let router = Arc::new(alerts::AlertRouter::new(config.alerts.clone()));
    let webhooks = Arc::new(config.webhooks.clone());
    let alert_worker = if router.is_empty() && webhooks.is_empty() {
        None
    } else {
        tracing::info!(
            "{} alert sink(s), {} webhook(s) configured",
            config.alerts.len(),
            config.webhooks.len()
        );
        Some(tokio::spawn(alerts::run_delivery_worker(
            store.clone(),
            crate::webhooks::secrets(&config.webhooks),
        )))
    };

    // Kick off the indexing loop in the background; serve the API on this task.
    let mut ingest = tokio::spawn(index_loop(
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
        velocity.clone(),
        threshold,
        velocity_cfg,
        router.clone(),
        webhooks.clone(),
        factory.clone(),
        finality,
        window,
        seal_direct,
        concurrency,
    ));

    // Admin UI (RFC-0010 Part A): on by default on localhost. Off-localhost it needs an explicit
    // token (auth is the operator's gateway's job, but the local UI should never appear unguarded on
    // a public bind); `--no-admin` removes it entirely for hosted deployments.
    let admin_enabled = !no_admin
        && (serve::is_localhost(&listen) || std::env::var("NUTHATCH_ADMIN_TOKEN").is_ok());
    if !no_admin && !admin_enabled {
        tracing::warn!(
            "admin UI disabled: bound off-localhost without NUTHATCH_ADMIN_TOKEN set (RFC-0010 Part A)"
        );
    }
    let nest_info = serde_json::json!({
        "name": config.nest.name,
        "chain": config.nest.chain,
        "chain_id": config.nest.chain_id,
        "registry_hash": format!("0x{}", hex::encode(registry.hash())),
        "table_count": registry.tables().len(),
        "contracts": config.contracts.iter()
            .map(|c| serde_json::json!({ "alias": c.alias, "address": c.address })).collect::<Vec<_>>(),
        "templates": config.templates,
        "factories": config.factories,
        "webhooks": config.webhooks.iter()
            .map(|w| serde_json::json!({ "name": w.name, "table": w.table, "url": w.url,
                "finality": w.finality.clone().unwrap_or_else(|| "sealed".into()) })).collect::<Vec<_>>(),
    });

    let app_state = serve::AppState {
        store: store.clone(),
        address: config.primary()?.address.clone(),
        chain: config.nest.chain.clone(),
        dir: dir.clone(),
        balances,
        exposure,
        velocity,
        threshold,
        velocity_threshold: velocity_cfg.map(|(amt, _)| amt),
        tables: Arc::new(registry.schema()),
        sql_gate: Arc::new(tokio::sync::Semaphore::new(serve::SQL_MAX_CONCURRENCY)),
        admin_enabled,
        nest_info: Arc::new(nest_info),
    };
    // The indexer and the API share a fate. Previously `serve` was awaited and `ingest` only aborted
    // afterwards, so if indexing died (an error or a panic) the process kept serving stale data as if
    // healthy — a silent failure (deadlock-review finding C1). Select over both: whichever ends first
    // decides the exit, and an indexing error/panic propagates out as a non-zero exit.
    let result = tokio::select! {
        r = serve::run(&listen, app_state) => r,
        joined = &mut ingest => match joined {
            Ok(inner) => inner, // index_loop's own Result (Ok only on a clean shutdown break)
            Err(e) if e.is_panic() => Err(anyhow::anyhow!("indexing loop panicked")),
            Err(e) => Err(anyhow::anyhow!("indexing loop task failed: {e}")),
        },
    };
    ingest.abort();
    if let Some(w) = alert_worker {
        w.abort();
    }
    result
}

/// Batch size (rows) at which `backfill_direct` flushes a sealed segment — bounds RSS during a
/// from-history backfill regardless of how long the range is.
const SEAL_DIRECT_BATCH: usize = 20_000;

/// Above this many discovered children, the factory backfill flips from an address-list filter to a
/// topic0-only fetch with local registry-lookup filtering (RFC-0009 §4) — providers cap address-list
/// size, and a huge list is slower than fetching by topic0 and discarding non-children locally.
const FACTORY_FLIP_THRESHOLD: usize = 500;

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
                if next >= chunk_to {
                    return Err(e).with_context(|| single_block_over_cap(next)); // H3: can't shrink a block
                }
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
        let ts = source.block_timestamps(&blocks).await?;
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

/// The error context when a single block's logs exceed a provider's `getLogs` result cap — it can't be
/// split or shrunk further, so the backfill/tip loop stops loudly instead of retrying forever (H3).
fn single_block_over_cap(block: u64) -> String {
    format!(
        "block {block} alone exceeds the provider's getLogs result cap — use a provider with a \
         higher/no cap"
    )
}

/// Fetch logs for `[from, to]`, transparently splitting the range in half and retrying each half when
/// a provider rejects it as "too many results" (RFC-0004 §2). The pipelined backfill uses a *fixed*
/// window and otherwise has no shrink-retry (deadlock-review finding H2), so an oversized `--window`
/// against a capped provider would abort the whole run; this makes it self-correct. A single block that
/// alone exceeds the cap can't be split further, so it fails with a clear message rather than looping
/// forever (finding H3).
fn fetch_logs_splitting<'a>(
    source: &'a dyn Source,
    addresses: &'a [String],
    topic0s: &'a [String],
    from: u64,
    to: u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<crate::rpc::Log>>> + Send + 'a>>
{
    Box::pin(async move {
        match source.logs(addresses, topic0s, from, to).await {
            Ok(logs) => Ok(logs),
            Err(e) if chunker::is_result_too_large(&e) => {
                if from >= to {
                    return Err(e).with_context(|| single_block_over_cap(from));
                }
                let mid = from + (to - from) / 2;
                let mut left = fetch_logs_splitting(source, addresses, topic0s, from, mid).await?;
                let right = fetch_logs_splitting(source, addresses, topic0s, mid + 1, to).await?;
                left.extend(right);
                Ok(left)
            }
            Err(e) => Err(e).with_context(|| format!("getLogs {from}..={to}")),
        }
    })
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
    // Called after each segment seals, with the highest block now durably sealed — the caller
    // persists it as a resume watermark so a mid-backfill failure resumes here instead of restarting
    // from `from` (which would re-fetch, and on an adaptive path re-seal, already-sealed ranges).
    mut on_seal: impl FnMut(u64) -> Result<()>,
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
            // Split-and-retry on a provider result cap instead of aborting the whole backfill (H2/H3).
            let logs = fetch_logs_splitting(source, addresses, topic0s, w_from, w_to).await?;
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
            let ts = source.block_timestamps(&blocks).await?;
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
            on_seal(w_to)?;
        }
    }
    if !buf.is_empty() {
        seal::seal_range(dir, &buf, batch_from, to)?;
        on_seal(to)?;
    }
    Ok(total)
}

/// Factory-aware sequential seal-direct backfill (RFC-0009 §3). Per chunk, two passes: pass 1 fetches
/// with the current address filter (base contracts + children discovered so far) and updates the
/// child registry from the factory events it decodes; pass 2 (a fixpoint loop, for nested factories
/// within one chunk) re-fetches the same range for *only* the newly discovered children. All logs are
/// then decoded together with the full registry, stamped, sorted by `(block, log_index)`, and sealed —
/// so the segments are deterministic and (step 3a) will match the pipelined path byte-for-byte. Uses
/// the efficient address filter, not the tip loop's topic0-only fetch. Grows `children`.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct_factory(
    source: &dyn Source,
    registry: &DecodeRegistry,
    factory: &FactorySet,
    children: &mut ChildRegistry,
    dir: &std::path::Path,
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
    force_topic0: bool,
    // Resume watermark callback — see [`backfill_direct_pipelined`]. The factory path uses an adaptive
    // window (non-deterministic boundaries), so resuming from the last sealed block instead of `from`
    // is what prevents a re-run from re-sealing overlapping ranges under new hashes (duplicate data).
    mut on_seal: impl FnMut(u64) -> Result<()>,
) -> Result<u64> {
    use std::collections::HashSet;
    let base: Vec<String> = registry
        .addresses()
        .iter()
        .map(|a| format!("0x{}", hex::encode(a)))
        .collect();
    let empty_ts = std::collections::HashMap::new();

    let mut buf: Vec<String> = Vec::new();
    let mut batch_from = from;
    let mut next = from;
    let mut total = 0u64;
    let mut flipped_logged = false;
    let mut chunker = AdaptiveWindow::for_window(window);
    while next <= to {
        let chunk_to = (next + chunker.window() - 1).min(to);

        // Filter flip (RFC-0009 §4): a forced override or a discovered set past the threshold switches
        // this chunk from the address-list two-pass to a single topic0-only fetch + local filtering.
        let use_topic0 = force_topic0 || base.len() + children.len() > FACTORY_FLIP_THRESHOLD;
        if use_topic0 && !flipped_logged {
            tracing::info!(
                "factory backfill filter flipped to topic0-only + local filter ({} children)",
                children.len()
            );
            flipped_logged = true;
        }

        let mut all_logs;
        if use_topic0 {
            // Topic0-only: every matching log (contract + all children) is in hand in one fetch, so
            // there is no second pass; `decode_window` filters locally by registry membership.
            all_logs = match source.logs(&[], topic0s, next, chunk_to).await {
                Ok(l) => {
                    chunker.observed(l.len() as u64);
                    l
                }
                Err(e) if chunker::is_result_too_large(&e) => {
                    if next >= chunk_to {
                        return Err(e).with_context(|| single_block_over_cap(next));
                    }
                    chunker.too_large();
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}")),
            };
            let _ = decode_window(registry, Some(factory), children, &all_logs, &empty_ts);
        } else {
            // Pass 1: current filter = base contracts + all children discovered so far.
            let mut fetched: HashSet<String> =
                base.iter().map(|s| s.to_ascii_lowercase()).collect();
            let mut current: Vec<String> = base.clone();
            for c in children.addresses() {
                if fetched.insert(c.to_ascii_lowercase()) {
                    current.push(c.to_string());
                }
            }
            let logs1 = match source.logs(&current, topic0s, next, chunk_to).await {
                Ok(l) => {
                    chunker.observed(l.len() as u64);
                    l
                }
                Err(e) if chunker::is_result_too_large(&e) => {
                    if next >= chunk_to {
                        return Err(e).with_context(|| single_block_over_cap(next));
                    }
                    chunker.too_large();
                    continue; // retry the same range with a smaller window
                }
                Err(e) => return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}")),
            };
            all_logs = logs1;
            // Decode to discover children (rows discarded here; the authoritative decode is below once
            // every child in this chunk is known and timestamps are in hand).
            let _ = decode_window(registry, Some(factory), children, &all_logs, &empty_ts);

            // Pass 2+ (fixpoint): re-fetch the chunk for children discovered here but not yet fetched.
            loop {
                let new: Vec<String> = children
                    .addresses()
                    .iter()
                    .filter(|c| !fetched.contains(&c.to_ascii_lowercase()))
                    .map(|c| c.to_string())
                    .collect();
                if new.is_empty() {
                    break;
                }
                for c in &new {
                    fetched.insert(c.to_ascii_lowercase());
                }
                let more = source
                    .logs(&new, topic0s, next, chunk_to)
                    .await
                    .with_context(|| format!("getLogs (children) {next}..={chunk_to}"))?;
                let _ = decode_window(registry, Some(factory), children, &more, &empty_ts);
                all_logs.extend(more);
            }
        }

        // Authoritative decode with the full child set, real timestamps, deterministic order.
        let mut blocks: Vec<u64> = all_logs.iter().map(|l| l.block_number).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let ts = source.block_timestamps(&blocks).await?;
        let rows = decode_window(registry, Some(factory), children, &all_logs, &ts);
        for r in &rows {
            buf.push(r.to_json().to_string());
            total += 1;
        }
        next = chunk_to + 1;

        if buf.len() >= SEAL_DIRECT_BATCH || next > to {
            if !buf.is_empty() {
                // Stamp the discovered-child set that produced these rows (RFC-0009 step 4).
                seal::seal_range_with_snapshot(
                    dir,
                    &buf,
                    batch_from,
                    chunk_to,
                    Some(&children.hash()),
                )?;
                buf.clear();
                on_seal(chunk_to)?;
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
    backfill: Option<u64>,
    start_block: Option<u64>,
    dir: PathBuf,
    balances: BalanceView,
    exposure: ExposureView,
    labels: Arc<LabelSet>,
    screener: Arc<Option<LiveScreener>>,
    velocity: VelocityView,
    threshold: Option<i128>,
    velocity_cfg: Option<(i128, u64)>,
    router: Arc<AlertRouter>,
    webhooks: Arc<Vec<crate::config::Webhook>>,
    factory: Option<Arc<FactorySet>>,
    finality: Finality,
    window: u64,
    seal_direct: bool,
    concurrency: usize,
) -> Result<()> {
    // User webhooks (RFC-0010 Part B): initialise each subscription's cursor before any sealing, so a
    // `since = "registration"` webhook starts at the tip and a `--seal-direct` backfill doesn't fire
    // its history. Best-effort — a tip lookup failure just defers registration to the first live tip.
    if !webhooks.is_empty() {
        if let Ok(tip) = source.tip().await {
            if let Err(e) = crate::webhooks::init_cursors(&store, &webhooks, tip) {
                tracing::warn!("webhook cursor init failed: {e:#}");
            }
        }
    }

    // The discovered-child registry (RFC-0009). Empty for a static nest; for a factory nest it is
    // rebuilt from stored factory events on a warm restart (a pure fold — determinism preserved) and
    // grown inline as the loop decodes new factory events.
    let mut children = ChildRegistry::new();
    if let Some(fs) = factory.as_deref() {
        if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
            children = rebuild_children(&dir, &store, &registry, fs);
            if !children.is_empty() {
                tracing::info!(
                    "rebuilt child registry: {} discovered child contract(s)",
                    children.len()
                );
            }
        }
    }
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
        // Resume a partial backfill instead of restarting from `origin`. A mid-backfill failure (a
        // transient RPC error) leaves `SEALED_THROUGH` at the last durably-sealed block but `LAST_BLOCK`
        // unset, so we re-enter here; resuming from the watermark re-fetches nothing already sealed —
        // which on the adaptive factory path also avoids re-sealing overlapping ranges under fresh
        // content hashes (duplicate, permanently double-counted segments). A fresh start has no
        // watermark and resumes from `origin`.
        let sealed_watermark = store
            .get_meta(SEALED_THROUGH_KEY)?
            .and_then(|s| s.parse::<u64>().ok());
        let resume_from = resume_from_watermark(sealed_watermark, origin);
        if resume_from <= finalized_through {
            // Record where the backfill *began* once; a resume keeps the original origin.
            if store.get_meta(START_BLOCK_KEY)?.is_none() {
                store.set_meta(START_BLOCK_KEY, &origin.to_string())?;
            }
            if resume_from > origin {
                tracing::info!(
                    "resuming seal-direct backfill from block {resume_from} (a prior run sealed through {})",
                    resume_from - 1
                );
            }
            // Persist the sealed watermark after every segment, so the backfill is resumable rather
            // than all-or-nothing (deadlock-review finding C1).
            let on_seal =
                |sealed_to: u64| store.set_meta(SEALED_THROUGH_KEY, &sealed_to.to_string());
            // A factory nest backfills with the sequential two-pass (RFC-0009 §3, address-filtered,
            // efficient, deterministic). Factory backfill is sequential regardless of `--concurrency`:
            // the child-event bulk is inherently ordered until the step-5 topic0-flip makes filters
            // version-independent, so pipelining below the flip buys little (RFC-0009 §3 risk note). A
            // static nest uses the pipelined path as before.
            let sealed = if let Some(fs) = factory.as_deref() {
                if concurrency > 1 {
                    tracing::info!(
                        "factory backfill runs sequentially (--concurrency {concurrency} ignored until the step-5 filter flip)"
                    );
                }
                tracing::info!(
                    "seal-direct factory backfill: {resume_from}..={finalized_through} (tip {tip}, sequential two-pass)…"
                );
                backfill_direct_factory(
                    source.as_ref(),
                    &registry,
                    fs,
                    &mut children,
                    &dir,
                    &topic0s,
                    resume_from,
                    finalized_through,
                    window,
                    fs.force_topic0(),
                    on_seal,
                )
                .await?
            } else {
                tracing::info!(
                    "seal-direct backfill: {resume_from}..={finalized_through} (tip {tip}, {concurrency}-way)…"
                );
                backfill_direct_pipelined(
                    source.as_ref(),
                    &registry,
                    &dir,
                    &addresses,
                    &topic0s,
                    resume_from,
                    finalized_through,
                    window,
                    concurrency,
                    on_seal,
                )
                .await?
            };
            store.set_meta(SEALED_THROUGH_KEY, &finalized_through.to_string())?;
            store.set_meta(LAST_BLOCK_KEY, &finalized_through.to_string())?;
            tracing::info!(
                "seal-direct backfill done: {sealed} rows sealed over {resume_from}..={finalized_through}"
            );
            if let Err(e) = rebuild_balances(&dir, &store, &registry, &balances) {
                tracing::warn!("balance rebuild after seal-direct failed: {e:#}");
            }
            if let Err(e) = rebuild_exposure(&dir, &store, &registry, &labels, &exposure) {
                tracing::warn!("exposure rebuild after seal-direct failed: {e:#}");
            }
            if let Some((_, w)) = velocity_cfg {
                if let Err(e) = rebuild_velocity(&dir, &store, &registry, w, &velocity) {
                    tracing::warn!("velocity rebuild after seal-direct failed: {e:#}");
                }
            }
            // Fire webhooks for the freshly-sealed history (a `since = "genesis"`/block webhook wants
            // it; a `since = "registration"` one is cursored past it, so this is a no-op there).
            if !webhooks.is_empty() {
                if let Err(e) =
                    crate::webhooks::deliver_sealed(&store, &dir, &webhooks, finalized_through)
                {
                    tracing::warn!("webhook delivery after seal-direct failed: {e:#}");
                }
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
                    if let Some((_, w)) = velocity_cfg {
                        velocity.apply(velocity_retraction_batch(&doomed, &registry, w));
                    }
                    // Drop children whose announcing factory event was rolled back (RFC-0009): the
                    // registry state at B is a pure fold over factory events ≤ B.
                    if factory.is_some() {
                        let dropped = children.rollback_to(ancestor);
                        if dropped > 0 {
                            tracing::warn!("reorg: dropped {dropped} discovered child contract(s)");
                        }
                    }
                    // Fire a `flag_retracted` alert for every rolled-back annotation a sink watches —
                    // a consumer that acted on a flag learns the chain took it back (RFC-0008 C5).
                    if !router.is_empty() {
                        for j in &doomed {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(j) {
                                if let Some(kind) = v.get("kind").and_then(|k| k.as_str()) {
                                    if router.watches(kind) {
                                        alerts::enqueue(
                                            &store,
                                            &router,
                                            "flag_retracted",
                                            kind,
                                            &v,
                                        )?;
                                    }
                                }
                            }
                        }
                    }

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
                // Fetch timestamps for the blocks these logs touch, then decode in chain order so
                // factory discovery is inline: a child created at log i is in the registry before its
                // own activity at log j>i in the same window decodes (RFC-0009 same-block handling).
                let mut blocks: Vec<u64> = logs.iter().map(|l| l.block_number).collect();
                blocks.sort_unstable();
                blocks.dedup();
                let timestamps = match source.block_timestamps(&blocks).await {
                    Ok(t) => t,
                    Err(e) => {
                        // Don't store this window with zeroed timestamps — once it finalizes it would
                        // seal `block_timestamp = 0` permanently (deadlock-review finding H4). The
                        // cursor hasn't advanced, so skip and re-fetch the same window next poll.
                        tracing::warn!(
                            "block timestamps unavailable for {next}..={to}: {e:#} — retrying window"
                        );
                        sleep_secs(2).await;
                        continue;
                    }
                };
                let mut rows = decode_window(
                    &registry,
                    factory.as_deref(),
                    &mut children,
                    &logs,
                    &timestamps,
                );

                let mut stored = 0usize;
                let mut deltas = Vec::new();
                let mut exp_deltas = Vec::new();
                let mut vel_deltas = Vec::new();
                // Transfers to screen this window (only collected when screening is on).
                let mut to_screen: Vec<TransferRow> = Vec::new();
                for row in &mut rows {
                    let key = Store::entity_key(row.block_number, row.log_index);
                    // Feed the IVM balance + exposure views for transfer rows (extracted before storing).
                    if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                        if let Some(v) = value.as_deref().and_then(|s| s.parse::<i128>().ok()) {
                            deltas.extend(views::transfer_deltas(&from, &to_addr, v, 1));
                            // Direct exposure to the labeled set (empty when neither side is labeled).
                            exp_deltas
                                .extend(exposure::exposure_deltas(&from, &to_addr, v, 1, &labels));
                            // Velocity: the sender's outbound volume in this block's window (C3).
                            if let Some((_, w)) = velocity_cfg {
                                vel_deltas.extend(velocity::velocity_deltas(
                                    &from,
                                    row.block_number,
                                    v,
                                    1,
                                    w,
                                ));
                            }
                            // Threshold flag: a single transfer at/above the configured amount (C3).
                            if let Some(t) = threshold {
                                if let Some((fkey, ann)) = crate::flags::threshold_annotation(
                                    &from,
                                    &to_addr,
                                    v,
                                    row.block_number,
                                    row.log_index,
                                    &row.tx_hash,
                                    t,
                                ) {
                                    store.put_entity(&fkey, &ann.to_string())?;
                                    alerts::enqueue(
                                        &store,
                                        &router,
                                        "flag",
                                        "threshold_flag",
                                        &ann,
                                    )?;
                                }
                            }
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
                velocity.apply(vel_deltas);

                // Live sanctions screening (RFC-0008 C2): screen this window's transfers against the
                // configured list snapshots and store `sanction_hit` annotations. They share the
                // transfers' block keys, so they seal and roll back with the same range. Stored before
                // `maybe_seal` below so a freshly-finalized window seals its hits alongside its rows.
                if let Some(s) = screener.as_ref() {
                    let hits = s.screen_window(&to_screen);
                    for (key, ann) in &hits {
                        store.put_entity(key, &ann.to_string())?;
                        alerts::enqueue(&store, &router, "flag", "sanction_hit", ann)?;
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

                // Seal any newly-finalized range to an immutable Parquet segment, stamping the
                // discovered-child registry snapshot for a factory nest (RFC-0009 step 4).
                let snapshot = factory.as_ref().map(|_| children.hash());
                if let Err(e) = maybe_seal(&dir, &store, finalized_through, snapshot.as_deref()) {
                    tracing::warn!("sealing failed: {e:#}");
                }
                // Deliver user webhooks for whatever just sealed (RFC-0010 Part B) — enqueue only,
                // the background worker POSTs; a slow endpoint never blocks the loop.
                if !webhooks.is_empty() {
                    if let Err(e) =
                        crate::webhooks::deliver_sealed(&store, &dir, &webhooks, finalized_through)
                    {
                        tracing::warn!("webhook delivery failed: {e:#}");
                    }
                }
            }
            Err(e) if chunker::is_result_too_large(&e) => {
                if next >= to {
                    return Err(e).with_context(|| single_block_over_cap(next)); // H3: can't shrink a block
                }
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
/// The seal-direct backfill concurrency that's safe for the configured endpoints. A *single* RPC host
/// can't absorb a high-concurrency backfill: many concurrent requests to one host stall the whole
/// tokio runtime — a lost wakeup that parks every worker and never fires, so even the per-request
/// timeout can't rescue it, and the backfill hangs forever (reproduced at `--concurrency 8` to one
/// host; multiple hosts spread the load over separate connections and never hit it). So a single
/// endpoint is capped to sequential; two or more keep the requested parallelism. The caller logs the
/// cap so the operator knows to add endpoints for a faster backfill.
fn safe_backfill_concurrency(endpoint_count: usize, requested: usize) -> usize {
    if endpoint_count <= 1 {
        1
    } else {
        requested
    }
}

/// Where a seal-direct backfill starts: one past the last durably-sealed block if a prior run left a
/// watermark (resume a partial backfill), else the computed `origin` (a fresh start). Resuming is what
/// keeps a mid-backfill failure from re-fetching — and, on the adaptive factory path, re-sealing under
/// fresh content hashes — ranges already sealed (deadlock-review finding C1).
fn resume_from_watermark(sealed_through: Option<u64>, origin: u64) -> u64 {
    match sealed_through {
        Some(s) => s.saturating_add(1),
        None => origin,
    }
}

/// The `eth_getLogs` window to use: an explicit `--window` override, else the chain default. A zero
/// override is ignored (a zero-block window can't make progress).
fn effective_window(override_: Option<u64>, chain_window: u64) -> u64 {
    match override_ {
        Some(w) if w > 0 => w,
        _ => chain_window,
    }
}

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
fn maybe_seal(
    dir: &std::path::Path,
    store: &Store,
    finalized_through: u64,
    registry_snapshot: Option<&str>,
) -> Result<()> {
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
    match seal::seal_range_with_snapshot(dir, &entities, from, ceiling, registry_snapshot)? {
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

/// Build a weight −1 velocity retraction batch from rolled-back transfer rows (reorg). Re-derives the
/// sender's outbound-volume delta the live path fed, with weight −1, so a reorged velocity flag drops.
fn velocity_retraction_batch(
    entity_json: &[String],
    registry: &DecodeRegistry,
    window: u64,
) -> velocity::VelocityBatch {
    let cols: std::collections::HashMap<String, (String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, _t, v)| (d.table.clone(), (f.to_string(), v.to_string())))
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
        let Some((from_col, val_col)) = cols.get(table) else {
            continue;
        };
        if let (Some(from), Some(block), Some(val)) = (
            v[from_col].as_str(),
            v["block_number"].as_u64(),
            v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
        ) {
            batch.extend(velocity::velocity_deltas(from, block, val, -1, window));
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

/// Rebuild the derived velocity view on a warm restart (RFC-0008 C3), mirroring `rebuild_exposure`:
/// cold sealed segments fold to pre-summed (address, window) volume+count in DuckDB and seed; only
/// the un-sealed hot tail replays. Windowing is by `window` (the same block-bucketing the live path
/// uses), so cold and hot land in identical buckets.
fn rebuild_velocity(
    dir: &std::path::Path,
    store: &Store,
    registry: &DecodeRegistry,
    window: u64,
    velocity: &VelocityView,
) -> Result<()> {
    let transfer_tables: Vec<(String, String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, _t, v)| (d.table.clone(), f.to_string(), v.to_string()))
        })
        .collect();
    if transfer_tables.is_empty() {
        return Ok(());
    }

    let mut batch: velocity::VelocityBatch = Vec::new();

    let mut cold = 0usize;
    for (table, from_col, val_col) in &transfer_tables {
        match crate::analytics::cold_velocity(dir, table, from_col, val_col, window) {
            Ok(rows) => {
                cold += rows.len();
                for (key, volume, count) in rows {
                    batch.push(velocity::seed_item(key, volume, count));
                }
            }
            Err(e) => tracing::debug!("no cold velocity seed for {table}: {e:#}"),
        }
    }

    let mut hot = 0usize;
    for (table, from_col, val_col) in &transfer_tables {
        for raw in store.recent_by_table(table, usize::MAX).unwrap_or_default() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            if let (Some(from), Some(block), Some(val)) = (
                v[from_col].as_str(),
                v["block_number"].as_u64(),
                v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
            ) {
                batch.extend(velocity::velocity_deltas(from, block, val, 1, window));
                hot += 1;
            }
        }
    }

    if batch.is_empty() {
        return Ok(());
    }
    velocity.apply(batch);
    velocity.flush();
    tracing::info!(
        "rebuilt velocity view: {} bucket(s) ({cold} cold-seeded + {hot} hot transfer(s) replayed)",
        velocity.entries()
    );
    Ok(())
}

/// Decode a window's logs in chain order (block, log_index), routing each to a contract decoder or —
/// for a factory nest — a discovered child's template decoder, and discovering new children inline so
/// same-window child activity decodes (RFC-0009). Each row is stamped with its block timestamp before
/// discovery so a child's `discovered_timestamp` is exact. Pure aside from growing `children`.
fn decode_window(
    registry: &DecodeRegistry,
    factory: Option<&FactorySet>,
    children: &mut ChildRegistry,
    logs: &[crate::rpc::Log],
    timestamps: &std::collections::HashMap<u64, u64>,
) -> Vec<crate::registry::DecodedRow> {
    let mut ordered: Vec<&crate::rpc::Log> = logs.iter().collect();
    ordered.sort_by(|a, b| {
        a.block_number
            .cmp(&b.block_number)
            .then_with(|| a.log_index.cmp(&b.log_index))
    });

    let mut rows = Vec::new();
    for log in ordered {
        let decoded = match registry.decode(log) {
            Ok(Some(r)) => Some(r),
            Ok(None) => {
                // Not a contract event — route to a discovered child's template decoder, if any.
                factory.and_then(|_| {
                    let addr = log.address.to_ascii_lowercase();
                    children
                        .template_of(&addr)
                        .map(str::to_string)
                        .and_then(|tmpl| registry.decode_child(log, &tmpl).ok().flatten())
                })
            }
            Err(e) => {
                tracing::debug!("decode skipped: {e:#}");
                None
            }
        };
        if let Some(mut r) = decoded {
            r.block_timestamp = timestamps.get(&r.block_number).copied().unwrap_or(0);
            if let Some(fs) = factory {
                if let Some(child) = fs.discover(&r) {
                    if children.insert(child.clone()) {
                        tracing::info!(
                            "factory discovered {} child {}… at block {}",
                            child.template,
                            &child.address[..12.min(child.address.len())],
                            child.discovered_block
                        );
                    }
                }
            }
            rows.push(r);
        }
    }
    rows
}

/// Rebuild the discovered-child registry on a warm restart by folding the stored factory events
/// (RFC-0009). Cold (sealed) and hot factory-event rows are read as JSON and re-discovered — a pure
/// fold, so the reconstructed registry is identical to the one grown live. Best-effort per table.
fn rebuild_children(
    dir: &std::path::Path,
    store: &Store,
    _registry: &DecodeRegistry,
    factory: &FactorySet,
) -> ChildRegistry {
    let mut children = ChildRegistry::new();
    // Fold in block order so the earliest discovery of each child wins (matches the live path).
    for table in factory.factory_tables() {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        // Cold (sealed) rows via DuckDB, then hot (un-sealed) rows from the store; a table with no
        // sealed segment yet just yields nothing cold.
        if let Ok(cold) = crate::analytics::query(dir, &format!("SELECT * FROM \"{table}\"")) {
            rows.extend(cold);
        }
        for raw in store
            .recent_by_table(&table, usize::MAX)
            .unwrap_or_default()
        {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                rows.push(v);
            }
        }
        rows.sort_by(|a, b| {
            let key = |v: &serde_json::Value| {
                (
                    v.get("block_number")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.get("log_index")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                )
            };
            key(a).cmp(&key(b))
        });
        for v in &rows {
            if let Some(child) = factory.discover_stored(&table, v) {
                children.insert(child);
            }
        }
    }
    children
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H2/H3: `fetch_logs_splitting` halves a range and retries when a provider caps the result, so an
    /// oversized window self-corrects instead of aborting the backfill; a single block that alone
    /// exceeds the cap can't be split, so it fails loudly rather than looping forever.
    #[tokio::test]
    async fn fetch_logs_splitting_shrinks_then_fails_on_a_single_block() {
        use crate::rpc::Log;
        struct CappedSource {
            cap: u64,
        }
        #[async_trait::async_trait]
        impl Source for CappedSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1000)
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
            ) -> Result<Vec<Log>> {
                if to - from + 1 > self.cap {
                    anyhow::bail!("query returned more than 10000 results");
                }
                Ok((from..=to)
                    .map(|b| Log {
                        address: "0xabc".into(),
                        topics: vec![],
                        data: "0x".into(),
                        block_number: b,
                        block_hash: "0x".into(),
                        tx_hash: "0x".into(),
                        log_index: 0,
                    })
                    .collect())
            }
        }
        // A 100-block range against an 8-block cap splits all the way down and returns every log.
        let src = CappedSource { cap: 8 };
        let logs = fetch_logs_splitting(&src, &[], &[], 1, 100).await.unwrap();
        assert_eq!(logs.len(), 100);

        // A single block that itself exceeds the cap can't be split → a clear, loud error.
        let tiny = CappedSource { cap: 0 };
        let err = fetch_logs_splitting(&tiny, &[], &[], 42, 42)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("block 42 alone exceeds"), "got: {err}");
    }

    /// An address-aware mock source: `logs` respects the address filter (empty = all), so a factory
    /// backfill's pass 1 (contracts) and pass 2 (children-only) return different logs, as on a real
    /// provider. Used to prove the two-pass discovery (RFC-0009 §3).
    struct FilteringSource {
        logs: Vec<crate::rpc::Log>,
    }

    #[async_trait::async_trait]
    impl Source for FilteringSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            addrs: &[String],
            _t: &[String],
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            let allow: std::collections::HashSet<String> =
                addrs.iter().map(|a| a.to_ascii_lowercase()).collect();
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .filter(|l| allow.is_empty() || allow.contains(&l.address.to_ascii_lowercase()))
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

    /// RFC-0009 step 3 gate: the sequential two-pass backfill discovers a child in a chunk (pass 1's
    /// factory event) and re-fetches the chunk for that child (pass 2), so the child's *historical*
    /// activity is sealed — even though it wasn't in pass 1's address filter.
    #[tokio::test]
    async fn factory_backfill_two_pass_seals_child_activity() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";
        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                ).unwrap(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        // Pool created at block 10; its Swap at block 15 — both in the backfill range, but the Swap
        // is only reachable in pass 2 (the pool isn't in pass 1's contract-only filter).
        let source = FilteringSource {
            logs: vec![
                Log {
                    address: factory_addr.into(),
                    topics: vec![topic0("factory__pool_created")],
                    data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                    block_number: 10,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt1".into(),
                    log_index: 0,
                },
                Log {
                    address: pool_addr.into(),
                    topics: vec![topic0("pool__swap")],
                    data: format!("0x{:064x}", 999u64),
                    block_number: 15,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt2".into(),
                    log_index: 0,
                },
            ],
        };

        let dir = tempfile::tempdir().unwrap();
        let mut children = ChildRegistry::new();
        let sealed = backfill_direct_factory(
            &source,
            &reg,
            &fs,
            &mut children,
            dir.path(),
            &[],
            10,
            20,
            100,
            false,
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(
            sealed, 2,
            "the factory event and the child's historical swap both sealed"
        );
        assert!(
            children.contains(pool_addr),
            "the pool was discovered during backfill"
        );
        // RFC-0009 step 4: every factory segment records the discovered-child registry snapshot.
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let snap = children.hash();
        assert!(
            manifest
                .tables
                .values()
                .flatten()
                .all(|s| s.registry_snapshot.as_deref() == Some(snap.as_str())),
            "factory segments carry the registry snapshot"
        );
        // The child's Swap is queryable from the sealed segment.
        let n = crate::analytics::query(dir.path(), r#"SELECT count(*) AS n FROM "pool__swap""#)
            .unwrap();
        assert_eq!(n[0]["n"], serde_json::Value::from(1u64));
        let row =
            crate::analytics::query(dir.path(), r#"SELECT address FROM "pool__swap""#).unwrap();
        assert_eq!(row[0]["address"], serde_json::Value::from(pool_addr));
    }

    /// RFC-0009 step 3a: the factory backfill is **deterministic** — the same range over the same
    /// chain history seals byte-identical segments (identical content-address hashes). This is the
    /// reproducibility property content-addressing needs, and the equivalence a pipelined variant
    /// would have to preserve; factory backfill runs sequentially, so this is the guarantee that
    /// matters (the filter-version pipeline is deferred to the step-5 flip per the RFC risk note).
    #[tokio::test]
    async fn factory_backfill_is_byte_identical_across_runs() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_a = "0x2222222222222222222222222222222222222222";
        let pool_b = "0x3333333333333333333333333333333333333333";
        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                ).unwrap(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        let created = |block, li, pool: &str| Log {
            address: factory_addr.into(),
            topics: vec![topic0("factory__pool_created")],
            data: format!("0x{:0>64}", pool.trim_start_matches("0x")),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xt".into(),
            log_index: li,
        };
        let swap = |block, li, pool: &str, amt: u64| Log {
            address: pool.into(),
            topics: vec![topic0("pool__swap")],
            data: format!("0x{amt:064x}"),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xt".into(),
            log_index: li,
        };
        // Two pools, interleaved swaps across several blocks — a non-trivial discovered set.
        let logs = vec![
            created(10, 0, pool_a),
            swap(11, 0, pool_a, 100),
            created(12, 0, pool_b),
            swap(13, 0, pool_b, 200),
            swap(13, 1, pool_a, 150),
            swap(14, 0, pool_b, 250),
        ];

        async fn seal_sig(
            logs: Vec<crate::rpc::Log>,
            reg: &crate::registry::DecodeRegistry,
            fs: &FactorySet,
            force_topic0: bool,
        ) -> (tempfile::TempDir, Vec<String>) {
            let source = FilteringSource { logs };
            let dir = tempfile::tempdir().unwrap();
            let mut children = ChildRegistry::new();
            backfill_direct_factory(
                &source,
                reg,
                fs,
                &mut children,
                dir.path(),
                &[],
                10,
                20,
                100,
                force_topic0,
                |_| Ok(()),
            )
            .await
            .unwrap();
            let m = crate::seal::load_manifest(dir.path()).unwrap();
            let mut sig: Vec<String> = m
                .tables
                .iter()
                .flat_map(|(t, segs)| segs.iter().map(move |s| format!("{t}:{}", s.hash)))
                .collect();
            sig.sort();
            (dir, sig)
        }

        // Address-list mode is reproducible; and the RFC-0009 §4 topic0-flip produces byte-identical
        // segments (the flip changes only the fetch strategy, never the output).
        let (_d1, sig1) = seal_sig(logs.clone(), &reg, &fs, false).await;
        let (_d2, sig2) = seal_sig(logs.clone(), &reg, &fs, false).await;
        let (_d3, sig3) = seal_sig(logs.clone(), &reg, &fs, true).await;
        assert!(!sig1.is_empty(), "something was sealed");
        assert_eq!(
            sig1, sig3,
            "topic0-flip mode seals byte-identical segments to address-list mode"
        );
        assert_eq!(
            sig1, sig2,
            "identical range + history → byte-identical sealed segments"
        );
    }

    /// RFC-0009 step 2 gate: a child created and active in the *same* window is decoded — the
    /// factory's `PoolCreated` (log 0) discovers the pool, so the pool's `Swap` (log 1) routes to the
    /// template decoder in one in-order pass, no extra RPC. Verifies both rows and the child registry.
    #[test]
    fn factory_same_block_discovery_and_child_decode() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_abi = serde_json::from_str(
            r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
        )
        .unwrap();
        let pool_abi = serde_json::from_str(
            r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";

        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: factory_abi,
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: pool_abi,
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };

        let config: Config = toml::from_str(
            r#"
[nest]
name = "t"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc"]
[[contracts]]
alias = "factory"
address = "0x1111111111111111111111111111111111111111"
abi = "abis/f.json"
[[templates]]
name = "pool"
abi = "abis/p.json"
[[factories]]
watch = "factory"
event = "PoolCreated"
child_param = "pool"
template = "pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        // PoolCreated(pool) at log 0, then the pool's Swap(500) at log 1 — same block.
        let logs = vec![
            Log {
                address: factory_addr.into(),
                topics: vec![topic0("factory__pool_created")],
                data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                block_number: 100,
                block_hash: "0xbh".into(),
                tx_hash: "0xt1".into(),
                log_index: 0,
            },
            Log {
                address: pool_addr.into(),
                topics: vec![topic0("pool__swap")],
                data: format!("0x{:064x}", 500u64),
                block_number: 100,
                block_hash: "0xbh".into(),
                tx_hash: "0xt2".into(),
                log_index: 1,
            },
        ];

        let mut children = ChildRegistry::new();
        let ts = std::collections::HashMap::from([(100u64, 1_700_000_000u64)]);
        let rows = decode_window(&reg, Some(&fs), &mut children, &logs, &ts);

        assert_eq!(
            rows.len(),
            2,
            "both the factory event and the child event decoded"
        );
        assert_eq!(rows[0].table, "factory__pool_created");
        assert_eq!(
            rows[1].table, "pool__swap",
            "same-block child activity routed to the template"
        );
        assert_eq!(
            rows[1].address, pool_addr,
            "child row carries the child address"
        );
        assert!(children.contains(pool_addr), "the pool was discovered");
        assert_eq!(children.template_of(pool_addr), Some("pool"));
        // The child registry rolls the pool back on a reorg to before its creation block.
        assert_eq!(children.clone().rollback_to(99), 1);
    }

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
    fn window_override_policy() {
        // No override → the chain default window.
        assert_eq!(effective_window(None, 2_000), 2_000);
        // A positive override wins (sparse-contract long backfill).
        assert_eq!(effective_window(Some(50_000), 2_000), 50_000);
        // A zero override is ignored — a zero-block window can't make progress.
        assert_eq!(effective_window(Some(0), 2_000), 2_000);
    }

    #[test]
    fn backfill_resumes_from_the_sealed_watermark() {
        // No watermark → fresh start from origin.
        assert_eq!(resume_from_watermark(None, 100), 100);
        // A watermark → resume one past the last durably-sealed block (no re-fetch of sealed ranges).
        assert_eq!(resume_from_watermark(Some(150), 100), 151);
        // A watermark below origin still resumes from the watermark (keeps the partial work).
        assert_eq!(resume_from_watermark(Some(40), 100), 41);
        // No overflow at the ceiling.
        assert_eq!(resume_from_watermark(Some(u64::MAX), 100), u64::MAX);
    }

    #[test]
    fn single_endpoint_backfill_is_capped_to_sequential() {
        // One endpoint → forced sequential regardless of the requested concurrency (deadlock guard).
        assert_eq!(safe_backfill_concurrency(1, 8), 1);
        assert_eq!(safe_backfill_concurrency(0, 8), 1);
        assert_eq!(safe_backfill_concurrency(1, 1), 1);
        // Two or more endpoints → the requested concurrency is honored.
        assert_eq!(safe_backfill_concurrency(3, 8), 8);
        assert_eq!(safe_backfill_concurrency(2, 4), 4);
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
            events: Vec::new(),
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
            |_| Ok(()),
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
