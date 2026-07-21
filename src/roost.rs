//! The roost (RFC-0012 §1–4; multichain per RFC-0021): one runtime hosting many nests across **one or
//! more chains** — one isolated cursor per distinct chain (`group_by_chain` → a `spawn_roost` each),
//! held to a **per-cursor** RSS budget. A single-chain roost (top-level `chain`) is the N=1 case, still
//! byte-identical to solo `dev`. The single-cursor law holds per chain: never multiplex two chains
//! behind one cursor. Below is the original RFC-0012 single-chain history.
//!
//! (RFC-0012) one runtime hosting many nests on the same chain. Slice 1 landed the
//! **layout + serving surface** — a `roost.toml` naming the chain and the mounted nests, a `/nests`
//! roster, and every nest's full API under a `/<name>/…` prefix. Slice 2a landed the **shared cursor**:
//! `dev` now drives all nests from ONE `indexer::spawn_roost` task — one `getLogs` per window fanned
//! out to the owning nests (see `indexer::roost_index_loop`), so N nests cost one nest's worth of RPC
//! chatter. Per-nest tables stay byte-identical to running each nest solo (the same per-window code
//! runs either way). Static and factory nests can be co-mounted (slice 2b — a factory forces the union
//! fetch topic0-only, demuxing by topic0 instead of address); shared reorg fan-out is slice 3; and a
//! per-runtime footprint projection + `max_rss` refusal is slice 4.
//!
//! Isolation is by construction: each nest keeps its own directory (`nests/<name>/` — its own
//! `nuthatch.redb`, `segments/`, views), so one nest's bad view or runaway factory can't touch
//! another's data (the CLAUDE.md non-negotiable). The roost shares the *chain identity* and the
//! *cursor* — never the stores.

use crate::config::Config;
use crate::indexer;
use crate::rpc::{self, RpcClient};
use crate::source::Source;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The roost manifest file, at the roost directory root. Sibling of a nest's `nuthatch.toml`.
pub const ROOST_FILE: &str = "roost.toml";

/// Where mounted nests live under the roost dir: `nests/<name>/` is a nest directory, exactly as a
/// standalone nest is today.
pub const NESTS_DIR: &str = "nests";

/// A roost manifest: the mounted nests plus the chain(s) they follow. A roost may host nests across
/// **one or more chains** (RFC-0021) — one isolated cursor per distinct chain. The single-chain form
/// keeps the top-level `chain`/`chain_id`/`rpc_urls`; a multichain roost lists its chains under
/// `[[chains]]` and lets each nest declare its own chain. The single-cursor law holds **per chain**:
/// never multiplex two chains behind one cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roost {
    pub roost: RoostMeta,
    /// Multichain: each chain the roost serves, with its own RPC endpoints (RFC-0021). Mutually
    /// exclusive with the top-level `chain`/`chain_id`. Empty → the single-chain top-level form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainEndpoint>,
}

/// One chain a roost follows, plus how to reach it — a cursor's substrate (RFC-0021).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainEndpoint {
    pub chain: String,
    pub chain_id: u64,
    #[serde(default)]
    pub rpc_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoostMeta {
    /// Human name for the roost (logging/roster only).
    pub name: String,
    /// Single-chain form: the one chain the cursor follows. Omit (with `chain_id`) for a multichain
    /// roost that declares its chains under `[[chains]]` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// Single-chain form: the one chain id. Omit for a multichain roost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// Single-chain form: RPC endpoints for the one chain. Overridable at runtime with `--rpc`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rpc_urls: Vec<String>,
    /// The mounted nests, by directory name under `nests/`.
    pub nests: Vec<String>,
    /// Resident-set ceiling **per active-chain cursor**, in MB (RFC-0021 — the footprint budget is
    /// per-cursor; a roost's total is Σ cursors). A cursor whose *projected* RSS exceeds this is refused
    /// before it starts. Absent → the CLAUDE.md 2 GB budget ([`DEFAULT_MAX_RSS_MB`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_mb: Option<u64>,
}

/// The default per-cursor RSS ceiling: the CLAUDE.md ≤2 GB budget (RFC-0021 — now per active-chain
/// cursor, not per whole runtime).
pub const DEFAULT_MAX_RSS_MB: u64 = 2048;

// A deliberately rough, *honest* per-runtime footprint model (RFC-0012 §3). These are order-of-
// magnitude estimates for the pre-mount projection, not measurements — the roster reports the real
// `rss_bytes()` alongside so an operator can calibrate. The shared serving/runtime cost is paid once;
// each nest adds its hot-store working set + decode registry, plus a chunk per active IVM view.
const ROOST_BASE_RSS_MB: u64 = 120; // serving + async runtime + on-demand DuckDB, paid once
const NEST_BASE_RSS_MB: u64 = 90; // redb hot store + decode registry + the always-on balance view
const NEST_VIEW_RSS_MB: u64 = 40; // each extra load: exposure view, velocity view, or child registry

/// Rough projected RSS (MB) for one nest: base + a chunk per active IVM view / factory child registry.
/// `has_labels` gates the exposure view (only spun up when the nest has labeled addresses).
pub fn estimate_nest_rss_mb(config: &Config, has_labels: bool) -> u64 {
    let mut mb = NEST_BASE_RSS_MB;
    if has_labels {
        mb += NEST_VIEW_RSS_MB; // exposure view (RFC-0008 C1)
    }
    if config.flags.velocity().is_some() {
        mb += NEST_VIEW_RSS_MB; // velocity view (RFC-0008 C3)
    }
    if !config.factories.is_empty() {
        mb += NEST_VIEW_RSS_MB; // discovered-child registry (RFC-0009)
    }
    mb
}

impl Roost {
    /// Load and validate `roost.toml` from a roost directory.
    pub fn load(dir: &Path) -> Result<Roost> {
        let path = dir.join(ROOST_FILE);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no {ROOST_FILE} in {}", dir.display()))?;
        let roost: Roost =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if roost.roost.nests.is_empty() {
            bail!(
                "roost '{}' mounts no nests (empty `nests` list)",
                roost.roost.name
            );
        }
        // Reject duplicate mounts and any name that would collide with a reserved top-level route
        // (`/nests`, `/health`) — the roster and per-nest prefixes share one path namespace.
        let mut seen = std::collections::HashSet::new();
        for n in &roost.roost.nests {
            // SEC-10: a nest name is both a filesystem path segment (`nests/<name>/`) and a route
            // prefix (`/<name>/…`), so restrict it to a safe charset — no `/`, `..`, or empties that
            // could escape the nests dir or produce surprising routes (matters once names come from a
            // resolved blob roster, not just an operator-authored toml).
            if n.is_empty()
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                bail!("nest name '{n}' is invalid (allowed: letters, digits, '_', '-')");
            }
            if n == "nests" || n == "health" {
                bail!("nest name '{n}' is reserved (collides with a roost route)");
            }
            if !seen.insert(n) {
                bail!("nest '{n}' is mounted more than once");
            }
        }
        Ok(roost)
    }

    /// The on-disk directory of a mounted nest, relative to the roost dir.
    pub fn nest_dir(dir: &Path, name: &str) -> PathBuf {
        dir.join(NESTS_DIR).join(name)
    }

    /// The chains this roost serves, each with its RPC endpoints (RFC-0021). A single-chain roost
    /// synthesizes one entry from the top-level `chain`/`chain_id`/`rpc_urls`; a multichain roost
    /// returns its `[[chains]]`. Errors if both forms are present (ambiguous) or neither (no chain).
    pub fn chain_endpoints(&self) -> Result<Vec<ChainEndpoint>> {
        let has_top = self.roost.chain.is_some() || self.roost.chain_id.is_some();
        if !self.chains.is_empty() {
            if has_top {
                bail!(
                    "roost '{}' declares both a top-level chain and [[chains]] — use one form",
                    self.roost.name
                );
            }
            return Ok(self.chains.clone());
        }
        match (self.roost.chain.clone(), self.roost.chain_id) {
            (Some(chain), Some(chain_id)) => Ok(vec![ChainEndpoint {
                chain,
                chain_id,
                rpc_urls: self.roost.rpc_urls.clone(),
            }]),
            _ => bail!(
                "roost '{}' declares no chain — set [roost] chain/chain_id/rpc_urls, or [[chains]]",
                self.roost.name
            ),
        }
    }
}

/// A chain's cursor unit (RFC-0021): the endpoint (RPC) plus the mounted nests that follow that chain.
/// Each becomes one isolated cursor — the single-cursor law, held per chain.
#[derive(Debug)]
pub struct ChainGroup {
    pub endpoint: ChainEndpoint,
    pub nests: Vec<(String, PathBuf, Config)>,
}

/// Load a mounted nest's config (chain grouping is validated by [`group_by_chain`], not here).
fn load_mounted_nest(roost_dir: &Path, name: &str) -> Result<(PathBuf, Config)> {
    let dir = Roost::nest_dir(roost_dir, name);
    let config = Config::load(&dir)
        .with_context(|| format!("loading mounted nest '{name}' from {}", dir.display()))?;
    Ok((dir, config))
}

/// Group loaded nests by their declared chain, matching each to a roost chain endpoint (RFC-0021).
/// A nest whose chain the roost doesn't declare is a hard error; declared-but-unused chains are dropped
/// (a cursor with no nests is pointless). Deterministic order (endpoints as declared).
pub fn group_by_chain(
    endpoints: &[ChainEndpoint],
    mounted: Vec<(String, PathBuf, Config)>,
) -> Result<Vec<ChainGroup>> {
    let mut groups: Vec<ChainGroup> = endpoints
        .iter()
        .map(|e| ChainGroup {
            endpoint: e.clone(),
            nests: Vec::new(),
        })
        .collect();
    for (name, path, config) in mounted {
        let idx = groups.iter().position(|g| {
            g.endpoint.chain == config.nest.chain && g.endpoint.chain_id == config.nest.chain_id
        });
        match idx {
            Some(i) => groups[i].nests.push((name, path, config)),
            None => bail!(
                "nest '{name}' is on {} (chain_id {}), which this roost doesn't declare — add it under \
                 [[chains]] (or [roost] chain/chain_id)",
                config.nest.chain,
                config.nest.chain_id
            ),
        }
    }
    groups.retain(|g| !g.nests.is_empty());
    if groups.is_empty() {
        bail!("roost mounts nests but none matched a declared chain");
    }
    Ok(groups)
}

/// `nuthatch roost dev <dir>`: bring up every mounted nest and serve them behind one listener.
///
/// One shared source drives all nests through a single `indexer::spawn_roost` task (the shared cursor —
/// one `getLogs` per window fanned out to the owning nests). Before starting it projects the roost's
/// RSS and refuses a mount that would exceed `max_rss` (§3). The process shares a fate with its nests:
/// if the ingestion task dies, the whole roost exits non-zero rather than serve stale data as if healthy
/// (the single-failure-boundary rule, generalised).
#[allow(clippy::too_many_arguments)]
pub async fn dev(
    dir: PathBuf,
    listen: String,
    rpc_override: Vec<String>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    no_admin: bool,
) -> Result<()> {
    let roost = Roost::load(&dir)?;
    let meta = &roost.roost;
    let endpoints = roost.chain_endpoints()?;

    // Load every mounted nest, then group by chain — one isolated cursor per distinct chain (RFC-0021).
    let mut mounted = Vec::with_capacity(meta.nests.len());
    for name in &meta.nests {
        let (nest_path, config) = load_mounted_nest(&dir, name)?;
        mounted.push((name.clone(), nest_path, config));
    }
    let groups = group_by_chain(&endpoints, mounted)?;

    // `--rpc` is ambiguous once a roost spans chains (which chain would it override?). Allow it only for
    // a single-chain roost; a multichain roost sets rpc_urls per chain under [[chains]].
    if !rpc_override.is_empty() && groups.len() > 1 {
        bail!(
            "--rpc is ambiguous for a multichain roost ({} chains) — set rpc_urls per chain under [[chains]]",
            groups.len()
        );
    }
    tracing::info!(
        "roost '{}': mounting {} nest(s) across {} chain(s) — one isolated cursor per chain",
        meta.name,
        meta.nests.len(),
        groups.len(),
    );

    let admin_enabled = indexer::admin_enabled(no_admin, &listen);
    let admin_token = indexer::admin_required_token(admin_enabled, &listen);
    // The RSS budget is now **per active-chain cursor** (RFC-0021), not per whole runtime.
    let max_rss = meta.max_rss_mb.unwrap_or(DEFAULT_MAX_RSS_MB);

    // Bring up one cursor per chain group: its own source + `spawn_roost`, isolated tip/finality/reorg,
    // and held to the per-cursor RSS budget. A cursor's failure is the whole roost's failure (fate-share).
    let mut all_states: Vec<(String, crate::serve::AppState)> = Vec::new();
    let mut ingests: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    let mut alert_workers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut estimates: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut roost_total_mb = ROOST_BASE_RSS_MB;

    for group in groups {
        let rpc_urls = rpc::merge_rpcs(&rpc_override, group.endpoint.rpc_urls.clone());
        if rpc_urls.is_empty() {
            bail!(
                "roost '{}' chain {} has no rpc_urls (set them under [[chains]], or pass --rpc for a \
                 single-chain roost)",
                meta.name,
                group.endpoint.chain
            );
        }
        let concurrency = indexer::safe_backfill_concurrency(rpc_urls.len(), concurrency);

        // Per-cursor footprint budget (RFC-0021): this chain's nests must fit ≤ max_rss.
        let mut cursor_mb = 0u64;
        for (name, path, config) in &group.nests {
            let has_labels = !crate::labels::load(path).is_empty();
            let mb = estimate_nest_rss_mb(config, has_labels);
            estimates.insert(name.clone(), mb);
            cursor_mb += mb;
        }
        tracing::info!(
            "roost cursor on {} (chain_id {}): {} nest(s), ~{cursor_mb} MB projected; budget {max_rss} MB/cursor",
            group.endpoint.chain,
            group.endpoint.chain_id,
            group.nests.len(),
        );
        if cursor_mb > max_rss {
            bail!(
                "roost '{}' cursor on {} projects ~{cursor_mb} MB but max_rss is {max_rss} MB/cursor — \
                 raise max_rss, drop a nest, or move it to another roost",
                meta.name,
                group.endpoint.chain
            );
        }
        roost_total_mb += cursor_mb;

        // One source + one shared cursor per chain — per-nest tables stay byte-identical to solo `dev`.
        let source: Arc<dyn Source> = Arc::new(RpcClient::new(rpc_urls)?);
        let (states, ingest, alerts) = indexer::spawn_roost(
            source,
            group.nests,
            backfill,
            seal_direct,
            concurrency,
            window_override,
            admin_enabled,
            admin_token.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "bringing up roost '{}' cursor on {}",
                meta.name, group.endpoint.chain
            )
        })?;
        all_states.extend(states);
        ingests.push(ingest);
        alert_workers.extend(alerts);
    }

    tracing::info!(
        "roost footprint: ~{roost_total_mb} MB projected across {} cursor(s)",
        ingests.len()
    );

    // Roster (`GET /nests`) across every cursor's nests, with per-nest footprint attribution and the
    // roost's real resident set alongside the projection so operators can calibrate.
    let roster_entries: Vec<_> = all_states
        .iter()
        .map(|(name, state)| {
            serde_json::json!({
                "name": name,
                "chain": state.chain,
                "registry_hash": state.nest_info.get("registry_hash").cloned().unwrap_or_default(),
                "table_count": state.tables.len(),
                "base_path": format!("/{name}"),
                "estimated_rss_mb": estimates.get(name).copied().unwrap_or(0),
            })
        })
        .collect();
    let roster = serde_json::json!({
        "roost": meta.name,
        "chains": endpoints.iter().map(|e| e.chain.clone()).collect::<Vec<_>>(),
        "projected_rss_mb": roost_total_mb,
        "max_rss_mb_per_cursor": max_rss,
        "rss_bytes": crate::metrics::rss_bytes(),
        "nests": roster_entries,
    });

    // Fate-share the server with every cursor: whichever ends first decides the exit, and any cursor's
    // error/panic propagates out (never serve stale data as if healthy) — the single-failure-boundary
    // rule, held per cursor. `select_all` over `&mut` handles so the rest stay abortable afterwards.
    let result = tokio::select! {
        r = crate::serve::run_roost(&listen, roster, all_states) => r,
        (joined, _idx, _rest) = futures::future::select_all(ingests.iter_mut()) => match joined {
            Ok(inner) => inner,
            Err(e) if e.is_panic() => Err(anyhow::anyhow!("a roost ingestion loop panicked")),
            Err(e) => Err(anyhow::anyhow!("a roost ingestion loop task failed: {e}")),
        },
    };
    for h in &ingests {
        h.abort();
    }
    for w in &alert_workers {
        w.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_FILE;

    /// Write a minimal roost.toml + one nest dir on the given chain.
    fn write_roost(dir: &Path, chain: &str, chain_id: u64, nest_chain: &str, nest_chain_id: u64) {
        std::fs::write(
            dir.join(ROOST_FILE),
            format!(
                "[roost]\nname = \"test\"\nchain = \"{chain}\"\nchain_id = {chain_id}\n\
                 rpc_urls = [\"http://localhost:8545\"]\nnests = [\"a\"]\n"
            ),
        )
        .unwrap();
        let nest = Roost::nest_dir(dir, "a");
        std::fs::create_dir_all(&nest).unwrap();
        std::fs::write(
            nest.join(CONFIG_FILE),
            format!(
                "[nest]\nname = \"a\"\nchain = \"{nest_chain}\"\nchain_id = {nest_chain_id}\n\
                 rpc_urls = []\n\n[[contracts]]\nalias = \"t\"\naddress = \"0x0000000000000000000000000000000000000001\"\nabi = \"abi.json\"\n"
            ),
        )
        .unwrap();
        // A trivially-valid ABI so Config::load's downstream users don't choke (load itself doesn't read it).
        std::fs::write(nest.join("abi.json"), "[]").unwrap();
    }

    /// Write a nest dir on a given chain under a roost (for multichain grouping tests).
    fn write_nest_dir(roost_dir: &Path, name: &str, chain: &str, chain_id: u64) {
        let nest = Roost::nest_dir(roost_dir, name);
        std::fs::create_dir_all(&nest).unwrap();
        std::fs::write(
            nest.join(CONFIG_FILE),
            format!(
                "[nest]\nname = \"{name}\"\nchain = \"{chain}\"\nchain_id = {chain_id}\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"t\"\naddress = \"0x0000000000000000000000000000000000000001\"\nabi = \"abi.json\"\n"
            ),
        )
        .unwrap();
        std::fs::write(nest.join("abi.json"), "[]").unwrap();
    }

    fn mounted(roost_dir: &Path, name: &str) -> (String, PathBuf, Config) {
        let (p, c) = load_mounted_nest(roost_dir, name).unwrap();
        (name.to_string(), p, c)
    }

    #[test]
    fn loads_a_valid_roost() {
        let d = tempfile::tempdir().unwrap();
        write_roost(d.path(), "arbitrum-one", 42161, "arbitrum-one", 42161);
        let r = Roost::load(d.path()).unwrap();
        assert_eq!(r.roost.chain.as_deref(), Some("arbitrum-one"));
        assert_eq!(r.roost.nests, vec!["a"]);
        // A single-chain roost resolves to exactly one endpoint.
        assert_eq!(r.chain_endpoints().unwrap().len(), 1);
    }

    #[test]
    fn rejects_a_nest_whose_chain_isnt_declared() {
        let d = tempfile::tempdir().unwrap();
        // Roost declares arbitrum-one; the nest claims mainnet → hard error at grouping.
        write_roost(d.path(), "arbitrum-one", 42161, "mainnet", 1);
        let roost = Roost::load(d.path()).unwrap();
        let err = group_by_chain(
            &roost.chain_endpoints().unwrap(),
            vec![mounted(d.path(), "a")],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("doesn't declare"), "got: {err}");
    }

    #[test]
    fn multichain_roost_groups_nests_by_chain() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"multi\"\nnests = [\"a\", \"b\"]\n\n\
             [[chains]]\nchain = \"base\"\nchain_id = 8453\nrpc_urls = [\"http://base\"]\n\n\
             [[chains]]\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = [\"http://arb\"]\n",
        )
        .unwrap();
        write_nest_dir(d.path(), "a", "base", 8453);
        write_nest_dir(d.path(), "b", "arbitrum-one", 42161);
        let roost = Roost::load(d.path()).unwrap();
        let endpoints = roost.chain_endpoints().unwrap();
        assert_eq!(endpoints.len(), 2, "two declared chains");
        let groups = group_by_chain(
            &endpoints,
            vec![mounted(d.path(), "a"), mounted(d.path(), "b")],
        )
        .unwrap();
        assert_eq!(groups.len(), 2, "one cursor per chain");
        for g in &groups {
            assert_eq!(g.nests.len(), 1, "each chain has its one nest");
        }
    }

    #[test]
    fn rejects_both_top_level_and_multichain_forms() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"x\"\nchain = \"base\"\nchain_id = 8453\nrpc_urls = [\"u\"]\nnests = [\"a\"]\n\n\
             [[chains]]\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = [\"v\"]\n",
        )
        .unwrap();
        let roost = Roost::load(d.path()).unwrap();
        let err = roost.chain_endpoints().unwrap_err().to_string();
        assert!(
            err.contains("both a top-level chain and [[chains]]"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unsafe_nest_names() {
        // SEC-10: a nest name that could escape the nests dir or make a surprising route is refused.
        for bad in ["../etc", "a/b", "", "has space"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(
                d.path().join(ROOST_FILE),
                format!("[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"{bad}\"]\n"),
            )
            .unwrap();
            let err = Roost::load(d.path()).unwrap_err().to_string();
            assert!(
                err.contains("invalid") || err.contains("reserved"),
                "name {bad:?} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_reserved_and_duplicate_nest_names() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"nests\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("reserved"));

        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"a\", \"a\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("more than once"));
    }

    #[test]
    fn rejects_an_empty_nest_list() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = []\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("no nests"));
    }

    #[test]
    fn footprint_estimate_scales_with_views() {
        fn cfg(extra: &str) -> Config {
            let toml = format!(
                "[nest]\nname = \"n\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"t\"\naddress = \"0x1\"\nabi = \"a.json\"\n{extra}"
            );
            toml::from_str(&toml).unwrap()
        }
        // Plain static nest, no labels: just the per-nest base.
        assert_eq!(estimate_nest_rss_mb(&cfg(""), false), NEST_BASE_RSS_MB);
        // Labels present → the exposure view adds a chunk.
        assert_eq!(
            estimate_nest_rss_mb(&cfg(""), true),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // A velocity flag → the velocity view.
        let vel = cfg("\n[flags]\nvelocity_amount = \"1000\"\n");
        assert_eq!(
            estimate_nest_rss_mb(&vel, false),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // A factory → the discovered-child registry.
        let fac = cfg("\n[[templates]]\nname = \"p\"\nabi = \"p.json\"\n\n\
             [[factories]]\nwatch = \"t\"\nevent = \"E\"\nchild_param = \"c\"\ntemplate = \"p\"\n");
        assert_eq!(
            estimate_nest_rss_mb(&fac, false),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // All three loads stack on top of the base.
        let all = cfg(
            "\n[flags]\nvelocity_amount = \"1000\"\n\n[[templates]]\nname = \"p\"\nabi = \"p.json\"\n\n\
             [[factories]]\nwatch = \"t\"\nevent = \"E\"\nchild_param = \"c\"\ntemplate = \"p\"\n",
        );
        assert_eq!(
            estimate_nest_rss_mb(&all, true),
            NEST_BASE_RSS_MB + 3 * NEST_VIEW_RSS_MB
        );
    }
}
