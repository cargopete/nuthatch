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

/// A single nest's contribution to a running process: its serve state plus the background tasks that
/// keep it fed (the ingestion loop, and an optional alert/webhook delivery worker). Built by
/// [`spawn_nest`]; consumed either by [`run`] (one nest, served at the root) or by the roost
/// (RFC-0012 — many nests, each served under a `/<name>/…` prefix behind one listener).
pub struct NestRuntime {
    pub state: serve::AppState,
    /// The ingestion loop task. Its `Result` is `Ok` only on a clean shutdown; an error or panic here
    /// must surface as a process failure, never be served-over silently (deadlock-review C1).
    pub ingest: tokio::task::JoinHandle<Result<()>>,
    /// The shared alert/webhook delivery worker, if any sink or webhook is configured. Only ever
    /// aborted (it drains a durable outbox), so its output type doesn't matter.
    pub alert_worker: Option<tokio::task::JoinHandle<()>>,
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
    // Admin UI (RFC-0010 Part A): on by default on localhost. Off-localhost it needs an explicit token
    // (auth is the operator's gateway's job, but the local UI should never appear unguarded on a public
    // bind); `--no-admin` removes it entirely. Computed here since it depends on the process's `listen`.
    let admin_enabled = admin_enabled(no_admin, &listen);
    let admin_token = admin_required_token(admin_enabled, &listen);
    let NestRuntime {
        state,
        mut ingest,
        alert_worker,
    } = spawn_nest(
        source,
        dir,
        config,
        backfill,
        seal_direct,
        concurrency,
        window_override,
        admin_enabled,
        admin_token,
    )
    .await?;

    // The indexer and the API share a fate. If indexing dies (an error or a panic) the process must
    // not keep serving stale data as if healthy — a silent failure (deadlock-review finding C1). Select
    // over both: whichever ends first decides the exit, and an indexing error/panic propagates out.
    let result = tokio::select! {
        r = serve::run(&listen, state) => r,
        joined = &mut ingest => match joined {
            Ok(inner) => inner,
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

/// Whether the built-in admin UI should be served, given `--no-admin` and the bind address. Extracted
/// so the roost computes it once for the whole process (RFC-0010 Part A semantics unchanged).
pub fn admin_enabled(no_admin: bool, listen: &str) -> bool {
    let enabled =
        !no_admin && (serve::is_localhost(listen) || std::env::var("NUTHATCH_ADMIN_TOKEN").is_ok());
    if !no_admin && !enabled {
        tracing::warn!(
            "admin UI disabled: bound off-localhost without NUTHATCH_ADMIN_TOKEN set (RFC-0010 Part A)"
        );
    }
    enabled
}

/// The token an admin-UI request must present, given the bind (SEC-5). `None` on a localhost bind (the
/// UI is open there); `Some(token)` off-localhost (the request must carry `?token=…`) — actually
/// checking it per request, rather than the env var merely *enabling* the route.
pub fn admin_required_token(admin_enabled: bool, listen: &str) -> Option<String> {
    if admin_enabled && !serve::is_localhost(listen) {
        std::env::var("NUTHATCH_ADMIN_TOKEN").ok()
    } else {
        None
    }
}

/// Build one nest's runtime: open its store, build its decode registry + IVM views, spawn its
/// ingestion loop and delivery worker, and assemble its serve state — everything *except* binding a
/// listener. The serving decision (root vs a `/<name>/…` prefix, one nest vs many) belongs to the
/// caller. Per-nest isolation (own store, own segments, own views) is the CLAUDE.md non-negotiable a
/// roost preserves by calling this once per nest.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_nest(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
) -> Result<NestRuntime> {
    let (nest, state, alert_worker, window) = build_nest(
        &source,
        dir,
        &config,
        window_override,
        admin_enabled,
        admin_token,
    )
    .await?;
    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        source,
        nest,
        backfill,
        seal_direct,
        concurrency,
        window,
    ));
    Ok(NestRuntime {
        state,
        ingest,
        alert_worker,
    })
}

/// Case-insensitive membership: is `addr` in `addresses`? The demux + dedup primitive — a provider may
/// return checksummed addresses while our filter list is lowercase hex, so never compare raw.
fn addr_in(addresses: &[String], addr: &str) -> bool {
    addresses.iter().any(|a| a.eq_ignore_ascii_case(addr))
}

/// The roost demux decision (RFC-0012 §2). A **static** nest (non-empty `addresses`) owns a log by
/// emitting address; a **factory** nest (empty `addresses` — topic0-only) owns it by topic0, so it
/// catches its factory-creation events and its runtime-discovered children regardless of their address.
/// Pure so it's testable without a `NestIngest`.
fn log_owned(addresses: &[String], topic0s: &[String], log: &crate::rpc::Log) -> bool {
    if addresses.is_empty() {
        log.topics.first().is_some_and(|t0| addr_in(topic0s, t0))
    } else {
        addr_in(addresses, &log.address)
    }
}

/// The union `getLogs` filter across all mounted nests: the case-insensitively-deduped concatenation of
/// every nest's address list and topic0 list. One fetch feeds them all (RFC-0012 §2 — the density win:
/// N nests cost one nest's worth of RPC chatter, not N). Takes the raw `(addresses, topic0s)` of each
/// nest so it's testable without constructing a `NestIngest`.
///
/// **Factory nests force topic0-only (RFC-0012 slice 2b).** A factory nest has an empty address filter
/// (children are discovered at runtime, so it must see all addresses matching its topics). An empty
/// address list in `getLogs` means "any address", so if *any* mounted nest is a factory the whole union
/// fetch drops its address filter and goes topic0-only — the factory nest then sees every candidate,
/// and static co-tenants over-fetch but demux back to exactly their own logs (`NestIngest::owns`),
/// keeping per-nest output byte-identical to solo.
fn union_filter<'a>(
    nests: impl Iterator<Item = (&'a [String], &'a [String])>,
) -> (Vec<String>, Vec<String>) {
    let mut addrs: Vec<String> = Vec::new();
    let mut topics: Vec<String> = Vec::new();
    let mut any_factory = false;
    for (nest_addrs, nest_topics) in nests {
        // An empty address list is the factory / topic0-only signal (see `build_nest`).
        if nest_addrs.is_empty() {
            any_factory = true;
        }
        for a in nest_addrs {
            if !addr_in(&addrs, a) {
                addrs.push(a.clone());
            }
        }
        for t in nest_topics {
            if !addr_in(&topics, t) {
                topics.push(t.clone());
            }
        }
    }
    // Any factory nest → topic0-only fetch (empty address filter = "any address").
    if any_factory {
        addrs.clear();
    }
    (addrs, topics)
}

/// The shared cursor (RFC-0012 slice 2a): one poll drives every mounted nest. One `source.tip()`, one
/// union `getLogs` per window, then each returned log is demuxed to the nest(s) that own it and run
/// through the SAME [`NestIngest::process_window`] a solo `dev` uses — so per-nest tables are
/// byte-identical to running that nest alone. Backfill stays per-nest (each `prepare`s its own history
/// first); the cursor only couples nests at the tip. Reorg is detected ONCE at the shared boundary and
/// fanned out to every nest (slice 3). Factory nests are supported (slice 2b): if any is mounted the
/// union fetch goes topic0-only and each nest demuxes by `owns` — address for static, topic0 for factory.
async fn roost_index_loop(
    source: Arc<dyn Source>,
    mut nests: Vec<NestIngest>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window: u64,
) -> Result<()> {
    if nests.is_empty() {
        return Ok(());
    }
    // Phase 0, per nest: each nest backfills its own history to near-tip independently (tip-only
    // coupling — the shared cursor never entangles backfill windows). Each returns its own start cursor.
    let mut nexts: Vec<u64> = Vec::with_capacity(nests.len());
    for nest in &mut nests {
        let next = nest
            .prepare(source.as_ref(), backfill, seal_direct, concurrency, window)
            .await?;
        nexts.push(next);
    }

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

        // Shared reorg detection + fan-out (RFC-0012 slice 3). A reorg is a chain event every nest at
        // the tip is exposed to identically, and all caught-up nests checkpoint the same boundaries with
        // the same hashes — so detect ONCE, at the most-caught-up nest's boundary, then fan the rollback
        // out to every nest. This is one detection (a handful of block-hash calls) instead of N, and one
        // observable reorg boundary. `rollback_reorg` is a no-op for any nest already at/below the fork
        // (a still-backfilling nest below finality can't be affected), so fanning to all is safe.
        let max_next = *nexts.iter().max().unwrap();
        if max_next > 0 {
            // Any caught-up nest is a valid checkpoint reference; use one at the max height.
            let reference = nexts.iter().position(|&n| n == max_next).unwrap();
            match detect_reorg(source.as_ref(), &nests[reference].store, max_next - 1).await {
                Ok(Some(ancestor)) => {
                    tracing::warn!(
                        "roost reorg to block {ancestor}: rolling back every mounted nest",
                    );
                    for (i, nest) in nests.iter_mut().enumerate() {
                        nest.rollback_reorg(ancestor)?;
                        nexts[i] = nexts[i].min(ancestor + 1);
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("roost reorg check skipped: {e:#}"),
            }
        }

        // The shared cursor advances from the *least* caught-up nest, so no nest ever skips a block.
        let global_next = *nexts.iter().min().unwrap();
        if global_next > tip {
            sleep_secs(2).await;
            continue;
        }
        let to = (global_next + chunker.window() - 1).min(tip);

        let (u_addrs, u_topics) = union_filter(
            nests
                .iter()
                .map(|n| (n.addresses.as_slice(), n.topic0s.as_slice())),
        );
        match source.logs(&u_addrs, &u_topics, global_next, to).await {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
                // Fan-out: hand each nest exactly the logs it owns within its own un-processed range,
                // through the same per-window path a solo nest runs. A nest already past this window is
                // skipped; a nest with zero owned logs still advances + checkpoints + seals (identical
                // to solo — a window with no matching logs still moves the cursor).
                for (i, nest) in nests.iter_mut().enumerate() {
                    if nexts[i] > to {
                        continue;
                    }
                    let nest_logs: Vec<crate::rpc::Log> = logs
                        .iter()
                        .filter(|l| l.block_number >= nexts[i] && nest.owns(l))
                        .cloned()
                        .collect();
                    // `Some(_)` → committed, advance this nest past the window. `None` → timestamps were
                    // unavailable, so leave its cursor put: `global_next` (the min) stays here, the next
                    // iteration re-fetches, and this nest retries while nests that did advance simply
                    // process the forward remainder — never re-processing.
                    if nest
                        .process_window(source.as_ref(), &nest_logs, nexts[i], to, tip)
                        .await?
                        .is_some()
                    {
                        nexts[i] = to + 1;
                    }
                }
            }
            Err(e) if chunker::is_result_too_large(&e) => {
                if global_next >= to {
                    return Err(e).with_context(|| single_block_over_cap(global_next));
                }
                chunker.too_large();
                tracing::debug!("range {global_next}..={to} too large; shrinking and retrying");
            }
            Err(e) => {
                tracing::warn!("get_logs {global_next}..={to} failed: {e:#}; retrying");
                sleep_secs(3).await;
            }
        }
    }
}

/// Build every mounted nest and spawn ONE shared-cursor ingestion task driving them all (RFC-0012
/// slice 2). Returns the per-nest serve states (for `/<name>/…` routing), the single shared ingest
/// handle, and the nests' alert-delivery workers. Static and factory nests may be co-mounted (slice 2b):
/// a factory nest forces the union fetch topic0-only and demuxes by topic0, static nests by address.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_roost(
    source: Arc<dyn Source>,
    nests: Vec<(String, PathBuf, Config)>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
) -> Result<(
    Vec<(String, serve::AppState)>,
    tokio::task::JoinHandle<Result<()>>,
    Vec<tokio::task::JoinHandle<()>>,
)> {
    let mut ingests = Vec::new();
    let mut states = Vec::new();
    let mut alert_workers = Vec::new();
    let mut window = None;
    for (name, dir, config) in nests {
        let (nest, state, worker, w) = build_nest(
            &source,
            dir,
            &config,
            window_override,
            admin_enabled,
            admin_token.clone(),
        )
        .await?;
        window.get_or_insert(w);
        ingests.push(nest);
        states.push((name, state));
        if let Some(worker) = worker {
            alert_workers.push(worker);
        }
    }
    let window = window.unwrap_or(DEFAULT_WINDOW);
    let ingest = tokio::spawn(roost_index_loop(
        source,
        ingests,
        backfill,
        seal_direct,
        concurrency,
        window,
    ));
    Ok((states, ingest, alert_workers))
}

/// Build one nest's runtime state *without* starting the tip loop: open its store, build its decode
/// registry + IVM views, run the warm-restart rebuilds, and assemble both the [`NestIngest`] the
/// ingestion loop drives and the [`serve::AppState`] the API serves — the two sharing the same view
/// handles (the API must see the same views the loop feeds). Also spawns the optional alert/webhook
/// delivery worker, and returns the effective `eth_getLogs` window. Spawning the ingestion loop is
/// the caller's job ([`spawn_nest`] today; a roost driver tomorrow, RFC-0012). Per-nest isolation
/// (own store, own segments, own views) is the CLAUDE.md non-negotiable a roost preserves by calling
/// this once per nest.
async fn build_nest(
    // Unused by the single-nest build (which leaves spawning the tip loop to the caller); kept in the
    // signature per the RFC-0012 contract so a roost driver can `build_nest` then `index_loop(source, …)`.
    _source: &Arc<dyn Source>,
    dir: PathBuf,
    config: &Config,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
) -> Result<(
    NestIngest,
    serve::AppState,
    Option<tokio::task::JoinHandle<()>>,
    u64,
)> {
    let store = Store::open(&dir.join(DB_FILE))?;
    // The decode registry drives all contracts; the indexer decodes every declared event of every
    // contract in the nest into per-table rows.
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, config)?);
    let balances = BalanceView::start()?;
    // Labels (RFC-0008 C1) are the annotation substrate the exposure view joins against. Loaded before
    // the exposure view so it only spins up when there's actually something to track.
    let labels = Arc::new(labels::load(&dir));
    if !labels.is_empty() {
        tracing::info!(
            "loaded {} labeled address(es) for exposure tracking",
            labels.len()
        );
    }
    // The exposure view joins transfers against the labeled set — with no labels it can only ever be
    // empty, so don't spend a DBSP circuit + dedicated thread on it (deadlock-review finding L10).
    let exposure = ExposureView::start(!labels.is_empty())?;
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
    // Only fed when a velocity flag is configured — skip its circuit + thread otherwise (L10).
    let velocity = VelocityView::start(velocity_cfg.is_some())?;
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
        let fs = FactorySet::build(config)?;
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

    // Group the per-nest state the loop owns and mutates into one struct, so a roost can drive many
    // nests from one cursor (RFC-0012). `source` stays shared and borrowed, not owned; `children`
    // starts empty (it is rebuilt/grown by `prepare`). The view handles are cloned here and shared
    // with the `AppState` below — the API must see the same views the loop feeds.
    let nest = NestIngest {
        dir: dir.clone(),
        store: store.clone(),
        registry: registry.clone(),
        balances: balances.clone(),
        exposure: exposure.clone(),
        velocity: velocity.clone(),
        labels: labels.clone(),
        screener: screener.clone(),
        threshold,
        velocity_cfg,
        router: router.clone(),
        webhooks: webhooks.clone(),
        factory: factory.clone(),
        children: ChildRegistry::new(),
        finality,
        addresses,
        topic0s,
        start_block,
    };

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
        admin_token,
        nest_info: Arc::new(nest_info),
    };

    Ok((nest, app_state, alert_worker, window))
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

/// All the per-nest state the tip-following loop owns and mutates, extracted from `index_loop`'s
/// argument list so a later change can drive many nests from one cursor (RFC-0012). This is a pure
/// mechanical grouping — the loop's behaviour is unchanged. The `Source` is deliberately NOT a field:
/// it is shared (`Arc<dyn Source>`) and stays borrowed into the two methods below.
struct NestIngest {
    dir: PathBuf,
    store: Store,
    registry: Arc<DecodeRegistry>,
    balances: BalanceView,
    exposure: ExposureView,
    velocity: VelocityView,
    labels: Arc<LabelSet>,
    screener: Arc<Option<LiveScreener>>,
    threshold: Option<i128>,
    velocity_cfg: Option<(i128, u64)>,
    router: Arc<AlertRouter>,
    webhooks: Arc<Vec<crate::config::Webhook>>,
    factory: Option<Arc<FactorySet>>,
    children: ChildRegistry,
    finality: Finality,
    addresses: Vec<String>,
    topic0s: Vec<String>,
    /// The nest's earliest vendored deployment block (the min of the contracts' `start_block`s), or
    /// `None`. Used only by [`prepare`]'s cold-start origin computation.
    start_block: Option<u64>,
}

impl NestIngest {
    /// Run the one-time preamble before the tip loop, then return the block to begin tip-following
    /// from. Initialises webhook cursors, rebuilds the discovered-child registry on a warm restart,
    /// runs the `--seal-direct` phase-0 backfill on a cold start, and computes the cold-start `next`.
    /// Extracted verbatim from `index_loop` so a roost can build many `NestIngest`s and drive them
    /// through the same code; `source` stays borrowed (not owned) and `window` is the chunker seed the
    /// phase-0 backfill uses.
    async fn prepare(
        &mut self,
        source: &dyn Source,
        backfill: Option<u64>,
        seal_direct: bool,
        concurrency: usize,
        window: u64,
    ) -> Result<u64> {
        // User webhooks (RFC-0010 Part B): initialise each subscription's cursor before any sealing, so a
        // `since = "registration"` webhook starts at the tip and a `--seal-direct` backfill doesn't fire
        // its history. Best-effort — a tip lookup failure just defers registration to the first live tip.
        if !self.webhooks.is_empty() {
            if let Ok(tip) = source.tip().await {
                if let Err(e) = crate::webhooks::init_cursors(&self.store, &self.webhooks, tip) {
                    tracing::warn!("webhook cursor init failed: {e:#}");
                }
            }
        }

        // The discovered-child registry (RFC-0009). Empty for a static nest; for a factory nest it is
        // rebuilt from stored factory events on a warm restart (a pure fold — determinism preserved) and
        // grown inline as the loop decodes new factory events.
        if let Some(fs) = self.factory.as_deref() {
            if self.store.get_meta(LAST_BLOCK_KEY)?.is_some() {
                self.children = rebuild_children(&self.dir, &self.store, &self.registry, fs);
                if !self.children.is_empty() {
                    tracing::info!(
                        "rebuilt child registry: {} discovered child contract(s)",
                        self.children.len()
                    );
                }
            }
        }
        // Phase 0 (cold start, `--seal-direct`): fast-seal the finalized history straight to Parquet,
        // bypassing the hot store, then rebuild the IVM view from those segments. The tip-following loop
        // below picks up from where this left off and handles the near-tip (un-finalized) window the
        // normal way. Nothing here can reorg — it is all strictly past finality.
        if seal_direct && self.store.get_meta(LAST_BLOCK_KEY)?.is_none() {
            let tip = source.tip().await?;
            let origin = cold_start_block(self.start_block, backfill, tip);
            let finalized_tag = match self.finality {
                Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
                Finality::Depth(_) => None,
            };
            let finalized_through = seal_ceiling(self.finality, tip, finalized_tag);
            // Resume a partial backfill instead of restarting from `origin`. A mid-backfill failure (a
            // transient RPC error) leaves `SEALED_THROUGH` at the last durably-sealed block but `LAST_BLOCK`
            // unset, so we re-enter here; resuming from the watermark re-fetches nothing already sealed —
            // which on the adaptive factory path also avoids re-sealing overlapping ranges under fresh
            // content hashes (duplicate, permanently double-counted segments). A fresh start has no
            // watermark and resumes from `origin`.
            let sealed_watermark = self
                .store
                .get_meta(SEALED_THROUGH_KEY)?
                .and_then(|s| s.parse::<u64>().ok());
            let resume_from = resume_from_watermark(sealed_watermark, origin);
            if resume_from <= finalized_through {
                // Record where the backfill *began* once; a resume keeps the original origin.
                if self.store.get_meta(START_BLOCK_KEY)?.is_none() {
                    self.store.set_meta(START_BLOCK_KEY, &origin.to_string())?;
                }
                if resume_from > origin {
                    tracing::info!(
                        "resuming seal-direct backfill from block {resume_from} (a prior run sealed through {})",
                        resume_from - 1
                    );
                }
                // Persist the sealed watermark after every segment, so the backfill is resumable rather
                // than all-or-nothing (deadlock-review finding C1).
                let on_seal = |sealed_to: u64| {
                    self.store
                        .set_meta(SEALED_THROUGH_KEY, &sealed_to.to_string())
                };
                // A factory nest backfills with the sequential two-pass (RFC-0009 §3, address-filtered,
                // efficient, deterministic). Factory backfill is sequential regardless of `--concurrency`:
                // the child-event bulk is inherently ordered until the step-5 topic0-flip makes filters
                // version-independent, so pipelining below the flip buys little (RFC-0009 §3 risk note). A
                // static nest uses the pipelined path as before.
                let sealed = if let Some(fs) = self.factory.as_deref() {
                    if concurrency > 1 {
                        tracing::info!(
                            "factory backfill runs sequentially (--concurrency {concurrency} ignored until the step-5 filter flip)"
                        );
                    }
                    tracing::info!(
                        "seal-direct factory backfill: {resume_from}..={finalized_through} (tip {tip}, sequential two-pass)…"
                    );
                    backfill_direct_factory(
                        source,
                        &self.registry,
                        fs,
                        &mut self.children,
                        &self.dir,
                        &self.topic0s,
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
                        source,
                        &self.registry,
                        &self.dir,
                        &self.addresses,
                        &self.topic0s,
                        resume_from,
                        finalized_through,
                        window,
                        concurrency,
                        on_seal,
                    )
                    .await?
                };
                self.store
                    .set_meta(SEALED_THROUGH_KEY, &finalized_through.to_string())?;
                self.store
                    .set_meta(LAST_BLOCK_KEY, &finalized_through.to_string())?;
                tracing::info!(
                    "seal-direct backfill done: {sealed} rows sealed over {resume_from}..={finalized_through}"
                );
                if let Err(e) =
                    rebuild_balances(&self.dir, &self.store, &self.registry, &self.balances)
                {
                    tracing::warn!("balance rebuild after seal-direct failed: {e:#}");
                }
                if let Err(e) = rebuild_exposure(
                    &self.dir,
                    &self.store,
                    &self.registry,
                    &self.labels,
                    &self.exposure,
                ) {
                    tracing::warn!("exposure rebuild after seal-direct failed: {e:#}");
                }
                if let Some((_, w)) = self.velocity_cfg {
                    if let Err(e) =
                        rebuild_velocity(&self.dir, &self.store, &self.registry, w, &self.velocity)
                    {
                        tracing::warn!("velocity rebuild after seal-direct failed: {e:#}");
                    }
                }
                // Fire webhooks for the freshly-sealed history (a `since = "genesis"`/block webhook wants
                // it; a `since = "registration"` one is cursored past it, so this is a no-op there).
                if !self.webhooks.is_empty() {
                    if let Err(e) = crate::webhooks::deliver_sealed(
                        &self.store,
                        &self.dir,
                        &self.webhooks,
                        finalized_through,
                    ) {
                        tracing::warn!("webhook delivery after seal-direct failed: {e:#}");
                    }
                }
            }
        }

        // Resume from the last committed block; on a cold start, backfill from the nest's earliest
        // vendored deployment block (full history) if it has one, else from `--backfill` behind the tip.
        let next = match self.store.get_meta(LAST_BLOCK_KEY)? {
            Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
            None => {
                let tip = source.tip().await?;
                let start = cold_start_block(self.start_block, backfill, tip);
                self.store.set_meta(START_BLOCK_KEY, &start.to_string())?;
                let src = if backfill.is_none() && self.start_block.is_some() {
                    " (from deployment)"
                } else {
                    ""
                };
                tracing::info!("cold start: backfilling from block {start}{src} (tip {tip})");
                start
            }
        };
        Ok(next)
    }

    /// Does this log belong to this nest? Two demux modes, mirroring the two nest kinds:
    /// - **Static nest** (non-empty address filter): by emitting address — the roost fetches the union
    ///   of every nest's addresses and each log routes to the nest(s) whose set contains it.
    /// - **Factory nest** (empty address filter — topic0-only, children discovered at runtime, RFC-0009):
    ///   by **topic0** — a child contract has an arbitrary address but its events carry a *template*
    ///   topic0 in this nest's set, so topic0 routing catches children (and factory-creation events)
    ///   regardless of address; `process_window`'s inline discovery then adopts them.
    ///
    /// Case-insensitive throughout (a provider may return checksummed hex while our filter is lowercase).
    /// Decode is the safety net either way — an over-routed log only yields rows this nest's registry
    /// (or discovered children) actually know, so per-nest output stays byte-identical to solo.
    fn owns(&self, log: &crate::rpc::Log) -> bool {
        log_owned(&self.addresses, &self.topic0s, log)
    }

    /// Detect and handle a reorg against the last committed block. Returns `Ok(Some(next))` — the
    /// block the caller should continue from — when a reorg was handled, `Ok(None)` when the chain
    /// stayed canonical (or there is nothing to check yet), and propagates the finality-violation
    /// `bail!` unchanged.
    async fn handle_reorg(&mut self, source: &dyn Source, next: u64) -> Result<Option<u64>> {
        // Reorg check: has the last block we committed against stayed canonical? If not, the
        // mutable hot store rolls back to the deepest surviving checkpoint (the only place a
        // reorg ever lands — sealed segments, once they exist, are strictly past finality).
        if next == 0 {
            return Ok(None);
        }
        match detect_reorg(source, &self.store, next - 1).await {
            Ok(Some(ancestor)) => {
                self.rollback_reorg(ancestor)?;
                Ok(Some(ancestor + 1))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::debug!("reorg check skipped: {e:#}");
                Ok(None)
            }
        }
    }

    /// Roll this nest's mutable hot store + IVM views back to `ancestor` (the deepest reorg-survivor
    /// block). Detection is the *caller's* job: a solo nest detects on its own cursor (`handle_reorg`);
    /// a roost detects **once** at the shared boundary and fans this out to every nest (slice 3). A
    /// nest already at or below `ancestor` (e.g. a still-backfilling nest in a roost while the tip
    /// reorgs) is a no-op — nothing above `ancestor` to undo, and its cursor must NOT be bumped up to
    /// `ancestor` (that would claim blocks it never indexed). Propagates the finality-violation bail.
    fn rollback_reorg(&mut self, ancestor: u64) -> Result<()> {
        // Retract the rolled-back transfers from the IVM view *before* dropping them from the hot
        // store — a reorg is just the same facts re-fed with weight −1.
        let last_indexed = self
            .store
            .get_meta(LAST_BLOCK_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(ancestor);
        // This nest hasn't reached past the fork: nothing to undo, and don't advance its cursor.
        if last_indexed <= ancestor {
            return Ok(());
        }
        // A reorg below the sealed watermark is a finality violation this model can't repair: the
        // doomed blocks are already in immutable sealed segments (and pruned from hot), so the
        // retraction below would be silently incomplete and the sealed layer would permanently disagree
        // with the canonical chain. Halt loudly instead (deadlock-review finding M6). The `--seal-direct`
        // finality depth / `finalized` tag is the contract; if it's being violated, it needs raising.
        let sealed_through = self
            .store
            .get_meta(SEALED_THROUGH_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if ancestor < sealed_through {
            anyhow::bail!(
                "reorg to block {ancestor} is below the sealed/finalized watermark \
                 {sealed_through} — a finality violation this indexer cannot repair; \
                 halting. Raise the chain's finality depth."
            );
        }
        let doomed = self.store.entities_in_range(ancestor + 1, last_indexed)?;
        self.balances.apply(retraction_batch(&doomed));
        self.exposure.apply(exposure_retraction_batch(
            &doomed,
            &self.registry,
            &self.labels,
        ));
        if let Some((_, w)) = self.velocity_cfg {
            self.velocity
                .apply(velocity_retraction_batch(&doomed, &self.registry, w));
        }
        // Drop children whose announcing factory event was rolled back (RFC-0009): the registry state
        // at B is a pure fold over factory events ≤ B.
        if self.factory.is_some() {
            let dropped = self.children.rollback_to(ancestor);
            if dropped > 0 {
                tracing::warn!("reorg: dropped {dropped} discovered child contract(s)");
            }
        }
        // Fire a `flag_retracted` alert for every rolled-back annotation a sink watches — a consumer
        // that acted on a flag learns the chain took it back (RFC-0008 C5).
        if !self.router.is_empty() {
            for j in &doomed {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(j) {
                    if let Some(kind) = v.get("kind").and_then(|k| k.as_str()) {
                        if self.router.watches(kind) {
                            alerts::enqueue(&self.store, &self.router, "flag_retracted", kind, &v)?;
                        }
                    }
                }
            }
        }

        let removed = self.store.rollback_to(ancestor)?;
        self.store.set_meta(LAST_BLOCK_KEY, &ancestor.to_string())?;
        METRICS.inc_reorgs();
        METRICS.set_last_block(ancestor);
        tracing::warn!(
            "reorg detected: rolled back to block {ancestor} (removed {removed} entities)"
        );
        Ok(())
    }

    /// Decode, store, IVM-feed, screen, checkpoint, seal and deliver webhooks for one fetched window
    /// `[next, to]` (with `tip` the current chain tip, used for the finality ceiling). Returns
    /// `Ok(Some(stored))` — the row count, caller advances the cursor — or `Ok(None)` when block
    /// timestamps were unavailable and the window must be retried WITHOUT advancing (the cursor stays
    /// put so a freshly-finalized window never seals `block_timestamp = 0`, deadlock-review H4).
    async fn process_window(
        &mut self,
        source: &dyn Source,
        logs: &[crate::rpc::Log],
        next: u64,
        to: u64,
        tip: u64,
    ) -> Result<Option<usize>> {
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
                return Ok(None);
            }
        };
        let mut rows = decode_window(
            &self.registry,
            self.factory.as_deref(),
            &mut self.children,
            logs,
            &timestamps,
        );

        let mut stored = 0usize;
        let mut deltas = Vec::new();
        let mut exp_deltas = Vec::new();
        let mut vel_deltas = Vec::new();
        // Transfers to screen this window (only collected when screening is on).
        let mut to_screen: Vec<TransferRow> = Vec::new();
        // PERF-2: accumulate every write and commit the whole window in ONE redb txn at the end,
        // instead of a `begin_write`/`commit` (fsync) per row. `(key, json)` for rows + annotations.
        let mut to_store: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for row in &mut rows {
            let key = Store::entity_key(row.block_number, row.log_index);
            // Feed the IVM balance + exposure views for transfer rows (extracted before storing).
            if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                if let Some(v) = value.as_deref().and_then(|s| s.parse::<i128>().ok()) {
                    deltas.extend(views::transfer_deltas(&from, &to_addr, v, 1));
                    // Direct exposure to the labeled set (empty when neither side is labeled).
                    exp_deltas.extend(exposure::exposure_deltas(
                        &from,
                        &to_addr,
                        v,
                        1,
                        &self.labels,
                    ));
                    // Velocity: the sender's outbound volume in this block's window (C3).
                    if let Some((_, w)) = self.velocity_cfg {
                        vel_deltas.extend(velocity::velocity_deltas(
                            &from,
                            row.block_number,
                            v,
                            1,
                            w,
                        ));
                    }
                    // Threshold flag: a single transfer at/above the configured amount (C3).
                    if let Some(t) = self.threshold {
                        if let Some((fkey, ann)) = crate::flags::threshold_annotation(
                            &from,
                            &to_addr,
                            v,
                            row.block_number,
                            row.log_index,
                            &row.tx_hash,
                            t,
                        ) {
                            to_store.push((fkey, ann.to_string()));
                            alerts::enqueue(
                                &self.store,
                                &self.router,
                                "flag",
                                "threshold_flag",
                                &ann,
                            )?;
                        }
                    }
                }
                if self.screener.is_some() {
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
            to_store.push((key, row.to_json().to_string()));
            stored += 1;
        }
        self.balances.apply(deltas);
        self.exposure.apply(exp_deltas);
        self.velocity.apply(vel_deltas);

        // Live sanctions screening (RFC-0008 C2): screen this window's transfers against the
        // configured list snapshots and store `sanction_hit` annotations. They share the
        // transfers' block keys, so they seal and roll back with the same range. Stored before
        // `maybe_seal` below so a freshly-finalized window seals its hits alongside its rows.
        if let Some(s) = self.screener.as_ref() {
            let hits = s.screen_window(&to_screen);
            for (key, ann) in &hits {
                to_store.push((key.clone(), ann.to_string()));
                alerts::enqueue(&self.store, &self.router, "flag", "sanction_hit", ann)?;
            }
            if !hits.is_empty() {
                tracing::warn!(
                    "sanctions screening: {} hit(s) in {next}..={to}",
                    hits.len()
                );
            }
        }
        // Fetch the window boundary's canonical hash for future reorg detection, then commit the whole
        // window — rows + annotations + the checkpoint + the `last_block` watermark — in one atomic txn.
        let checkpoint = match source.block_hash(to).await {
            Ok(Some(hash)) => Some((to, hash)),
            _ => None,
        };
        self.store.commit_window(
            &to_store,
            checkpoint.as_ref().map(|(b, h)| (*b, h.as_str())),
            to,
        )?;
        METRICS.set_last_block(to);
        METRICS.add_rows_decoded(stored as u64);
        if stored > 0 {
            tracing::info!(
                "blocks {next}..={to}: +{stored} rows (total {})",
                self.store.count()?
            );
        }

        // The highest block considered final under this chain's policy. For an L2 with the
        // `finalized` tag we ask the node; otherwise (and on tag failure) it's a fixed depth.
        let finalized_tag = match self.finality {
            Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
            Finality::Depth(_) => None,
        };
        let finalized_through = seal_ceiling(self.finality, tip, finalized_tag);

        // Seal any newly-finalized range to an immutable Parquet segment, stamping the
        // discovered-child registry snapshot for a factory nest (RFC-0009 step 4).
        let snapshot = self.factory.as_ref().map(|_| self.children.hash());
        if let Err(e) = maybe_seal(
            &self.dir,
            &self.store,
            finalized_through,
            snapshot.as_deref(),
        ) {
            tracing::warn!("sealing failed: {e:#}");
        }
        // Deliver user webhooks for whatever just sealed (RFC-0010 Part B) — enqueue only,
        // the background worker POSTs; a slow endpoint never blocks the loop.
        if !self.webhooks.is_empty() {
            if let Err(e) = crate::webhooks::deliver_sealed(
                &self.store,
                &self.dir,
                &self.webhooks,
                finalized_through,
            ) {
                tracing::warn!("webhook delivery failed: {e:#}");
            }
        }
        Ok(Some(stored))
    }
}

async fn index_loop(
    source: Arc<dyn Source>,
    mut nest: NestIngest,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window: u64,
) -> Result<()> {
    let mut next = nest
        .prepare(source.as_ref(), backfill, seal_direct, concurrency, window)
        .await?;

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

        if let Some(new_next) = nest.handle_reorg(source.as_ref(), next).await? {
            next = new_next;
            continue;
        }

        if next > tip {
            // Caught up to the tip — poll for new blocks.
            sleep_secs(2).await;
            continue;
        }

        let to = (next + chunker.window() - 1).min(tip);
        match source.logs(&nest.addresses, &nest.topic0s, next, to).await {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
                match nest
                    .process_window(source.as_ref(), &logs, next, to, tip)
                    .await?
                {
                    // Window processed and committed — advance the cursor past it.
                    Some(_stored) => next = to + 1,
                    // Timestamps were unavailable; the cursor stayed put, retry the same window.
                    None => continue,
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
    // Usually `last` itself, but if that boundary's hash couldn't be stored (a transient block_hash
    // failure at checkpoint time), fall back to the newest checkpoint we *do* have at/below `last`, so
    // a reorg is still verified against a real checkpoint instead of giving up entirely — the previous
    // "no hash here → nothing to verify" was a reorg blind spot (deadlock-review finding M7).
    let (checkpoint, stored) = match store.get_block_hash(last)? {
        Some(h) => (last, h),
        None => match store
            .checkpoints_desc()?
            .into_iter()
            .find(|(b, _)| *b <= last)
        {
            Some((b, h)) => (b, h),
            None => return Ok(None), // genuinely no checkpoint yet (cold start)
        },
    };
    let canonical = match source.block_hash(checkpoint).await? {
        Some(h) => h,
        None => return Ok(None), // source can't answer right now; try again next tick
    };
    if stored == canonical {
        return Ok(None);
    }
    for (block, hash) in store.checkpoints_desc()? {
        if block >= checkpoint {
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
pub fn safe_backfill_concurrency(endpoint_count: usize, requested: usize) -> usize {
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
            // COR-1: prune the sealed rows from hot AND advance the watermark in one atomic txn, AFTER
            // the segment is durable. The watermark advancing is what makes the range "cold", so it must
            // happen with the prune, never before it — else a crash between the two would leave the range
            // permanently in both layers (double-counted forever). `seal_range` is idempotent, so a crash
            // before this line just re-seals on restart.
            let pruned = store.prune_and_set_meta(
                from,
                ceiling,
                SEALED_THROUGH_KEY,
                &ceiling.to_string(),
            )?;
            METRICS.set_sealed_through(ceiling);
            METRICS.add_rows_sealed(summary.rows as u64);
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

    #[test]
    fn addr_in_is_case_insensitive() {
        let set = vec!["0xabc123".to_string(), "0xdef456".to_string()];
        assert!(addr_in(&set, "0xABC123")); // checksummed provider address matches lowercase filter
        assert!(addr_in(&set, "0xdef456"));
        assert!(!addr_in(&set, "0x999999"));
        assert!(!addr_in(&[], "0xabc123")); // a topic0-only (factory) nest owns nothing by address
    }

    #[test]
    fn log_owned_static_by_address_factory_by_topic0() {
        let transfer = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let mut child_log = transfer_log(20, 0);
        child_log.address = "0xchildaddress0000000000000000000000000000".into(); // some runtime child

        // Static nest: owns by address. It watches a fixed set; the child's address isn't in it.
        let static_addrs = vec!["0xAAA0000000000000000000000000000000000000".to_string()];
        let static_topics = vec![transfer.to_string()];
        assert!(!log_owned(&static_addrs, &static_topics, &child_log)); // wrong address → not owned
        let mut own_log = transfer_log(20, 1);
        own_log.address = "0xaaa0000000000000000000000000000000000000".into(); // checksum differs
        assert!(log_owned(&static_addrs, &static_topics, &own_log)); // address match (case-insensitive)

        // Factory nest: empty addresses → owns by topic0. It catches the child regardless of address,
        // because the child's event carries a template topic0 in the nest's set.
        let factory_addrs: Vec<String> = Vec::new();
        let factory_topics = vec![transfer.to_string()];
        assert!(log_owned(&factory_addrs, &factory_topics, &child_log)); // topic0 match → owned
        let mut other_topic = transfer_log(20, 2);
        other_topic.topics[0] = "0xdeadbeef".into();
        assert!(!log_owned(&factory_addrs, &factory_topics, &other_topic)); // topic not watched → not owned
    }

    #[test]
    fn union_filter_goes_topic0_only_when_a_factory_is_present() {
        let transfer = "0xddf252...".to_string();
        let created = "0xpaircreated...".to_string();
        // A static nest (fixed addresses) co-mounted with a factory nest (empty addresses).
        let static_addrs = vec!["0xAAA".to_string()];
        let factory_addrs: Vec<String> = Vec::new();
        let (addrs, topics) = union_filter(
            [
                (static_addrs.as_slice(), [transfer.clone()].as_slice()),
                (factory_addrs.as_slice(), [created.clone()].as_slice()),
            ]
            .into_iter(),
        );
        // The factory forces the whole fetch topic0-only: no address filter, both topics unioned.
        assert!(
            addrs.is_empty(),
            "a factory co-tenant must drop the address filter"
        );
        assert_eq!(topics, vec![transfer, created]);
    }

    /// Build a minimal static ERC20 `NestIngest` on disk through the real `build_nest` path.
    async fn build_test_nest(dir: &std::path::Path, addr: &str) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            format!(
                "[nest]\nname = \"n\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"tok\"\naddress = \"{addr}\"\nabi = \"abis/tok.json\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) =
            build_nest(&source, dir.to_path_buf(), &config, None, false, None)
                .await
                .unwrap();
        if let Some(w) = worker {
            w.abort();
        }
        nest
    }

    /// Seed a nest's hot store with one row per block and set `LAST_BLOCK` to the max.
    fn seed_blocks(nest: &NestIngest, blocks: &[u64]) {
        for &b in blocks {
            let key = Store::entity_key(b, 0);
            nest.store
                .put_entity(&key, &format!(r#"{{"table":"t","block_number":{b}}}"#))
                .unwrap();
        }
        let last = *blocks.iter().max().unwrap();
        nest.store
            .set_meta(LAST_BLOCK_KEY, &last.to_string())
            .unwrap();
    }

    /// RFC-0012 slice 3: one shared reorg fans out to every nest. A caught-up nest rolls back to the
    /// fork; a still-backfilling nest below the fork is spared and — crucially — its cursor is NOT
    /// bumped up to the ancestor (that would claim blocks it never indexed).
    #[tokio::test]
    async fn roost_reorg_fans_out_and_spares_behind_nests() {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let mut caught_up =
            build_test_nest(da.path(), "0x0000000000000000000000000000000000000001").await;
        let mut behind =
            build_test_nest(db.path(), "0x0000000000000000000000000000000000000002").await;

        // caught_up is at the tip (block 100); behind is still backfilling (block 30, below the fork).
        seed_blocks(&caught_up, &[10, 20, 30, 40, 50, 60, 80, 100]);
        seed_blocks(&behind, &[10, 20, 30]);

        // One shared reorg to ancestor 50, fanned to both nests (as `roost_index_loop` does).
        caught_up.rollback_reorg(50).unwrap();
        behind.rollback_reorg(50).unwrap();

        // Caught-up nest: rolled back to 50 — nothing above survives, cursor at 50.
        assert!(caught_up
            .store
            .entities_in_range(51, 1_000)
            .unwrap()
            .is_empty());
        assert_eq!(caught_up.store.entities_in_range(10, 50).unwrap().len(), 5); // 10,20,30,40,50
        assert_eq!(
            caught_up.store.get_meta(LAST_BLOCK_KEY).unwrap().as_deref(),
            Some("50")
        );

        // Behind nest: below the fork → untouched; cursor stays at 30 (NOT bumped to 50).
        assert_eq!(behind.store.entities_in_range(10, 1_000).unwrap().len(), 3);
        assert_eq!(
            behind.store.get_meta(LAST_BLOCK_KEY).unwrap().as_deref(),
            Some("30")
        );
    }

    #[test]
    fn union_filter_dedups_across_nests_case_insensitively() {
        // Two nests, overlapping on one address ("0xAAA"/"0xaaa") and one topic (the Transfer sig).
        let a_addrs = vec!["0xAAA".to_string(), "0xBBB".to_string()];
        let a_topics = vec!["0xtransfer".to_string()];
        let b_addrs = vec!["0xaaa".to_string(), "0xCCC".to_string()];
        let b_topics = vec!["0xTRANSFER".to_string(), "0xapproval".to_string()];
        let (addrs, topics) = union_filter(
            [
                (a_addrs.as_slice(), a_topics.as_slice()),
                (b_addrs.as_slice(), b_topics.as_slice()),
            ]
            .into_iter(),
        );
        // 0xAAA and 0xaaa collapse to one; BBB and CCC distinct → 3 addresses, first-seen casing kept.
        assert_eq!(addrs, vec!["0xAAA", "0xBBB", "0xCCC"]);
        // The Transfer topic collapses across casing; Approval is B-only → 2 topics.
        assert_eq!(topics, vec!["0xtransfer", "0xapproval"]);
    }

    #[test]
    fn owns_demux_reproduces_the_solo_address_filter() {
        // The core byte-identity claim of slice 2a: routing the union fetch through `owns` hands a nest
        // exactly the logs a solo, address-filtered fetch would have — no more, no less. So its decode
        // input is identical, and therefore its stored output is too.
        let mut a = transfer_log(10, 0);
        a.address = "0xAAA0000000000000000000000000000000000000".into();
        let mut b = transfer_log(10, 1);
        b.address = "0xBBB0000000000000000000000000000000000000".into();
        let mut a2 = transfer_log(11, 0);
        a2.address = "0xaaa0000000000000000000000000000000000000".into(); // same nest A, checksummed differently
        let union = [a.clone(), b.clone(), a2.clone()];

        let nest_a_addrs = vec!["0xAAA0000000000000000000000000000000000000".to_string()];
        // Compare by a stable key (Log isn't PartialEq): (address-lowercased, block, log_index).
        let key =
            |l: &crate::rpc::Log| (l.address.to_ascii_lowercase(), l.block_number, l.log_index);
        // What the roost feeds nest A: union filtered by A's ownership.
        let roost_input: Vec<_> = union
            .iter()
            .filter(|l| addr_in(&nest_a_addrs, &l.address))
            .map(key)
            .collect();
        // What a solo, address-filtered source would return for nest A: only A's own logs.
        let solo_input: Vec<_> = [&a, &a2].into_iter().map(key).collect();
        assert_eq!(roost_input, solo_input);
        // Nest B's log is never routed to A.
        assert!(!roost_input
            .iter()
            .any(|(addr, _, _)| addr.eq_ignore_ascii_case(&b.address)));
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
