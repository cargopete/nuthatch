//! The roost (RFC-0012 §1–4): one runtime hosting many nests on the same chain. Slice 1 landed the
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

/// A roost manifest: the shared chain identity plus the list of mounted nests. Chain identity is
/// hoisted **here**, above the per-nest configs, because it is what the shared cursor is keyed on — a
/// roost is one chain by definition (a second chain is a second cursor is a second process).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roost {
    pub roost: RoostMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoostMeta {
    /// Human name for the roost (logging/roster only).
    pub name: String,
    /// The shared chain the cursor follows. Every mounted nest's `[nest].chain` must equal this.
    pub chain: String,
    /// The shared chain id. Every mounted nest's `[nest].chain_id` must equal this.
    pub chain_id: u64,
    /// RPC endpoints for the shared chain (a nest's own `rpc_urls` are ignored in a roost — the roost
    /// owns the chain connection). Overridable at runtime with `--rpc`.
    pub rpc_urls: Vec<String>,
    /// The mounted nests, by directory name under `nests/`. (A future slice resolves blob hashes here
    /// via `nest load`; slice 1 takes plain directory names already present on disk.)
    pub nests: Vec<String>,
    /// Resident-set ceiling for the whole roost, in MB (RFC-0012 §3 — the footprint budget is
    /// per-runtime). A mount whose *projected* RSS exceeds this is refused before it starts. Absent →
    /// the CLAUDE.md 2 GB budget ([`DEFAULT_MAX_RSS_MB`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_mb: Option<u64>,
}

/// The default roost RSS ceiling: the CLAUDE.md ≤2 GB per-runtime budget.
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
}

/// Load a mounted nest's config and assert it belongs to this roost's chain. A chain (or chain-id)
/// mismatch is a hard error: a different chain needs its own roost (its own cursor), so co-mounting it
/// here would silently break the single-cursor model in slice 2.
fn load_mounted_nest(roost_dir: &Path, roost: &RoostMeta, name: &str) -> Result<(PathBuf, Config)> {
    let dir = Roost::nest_dir(roost_dir, name);
    let config = Config::load(&dir)
        .with_context(|| format!("loading mounted nest '{name}' from {}", dir.display()))?;
    if config.nest.chain != roost.chain || config.nest.chain_id != roost.chain_id {
        bail!(
            "nest '{name}' is on {} (chain_id {}) but this roost is {} (chain_id {}) — a different \
             chain needs its own roost",
            config.nest.chain,
            config.nest.chain_id,
            roost.chain,
            roost.chain_id
        );
    }
    Ok((dir, config))
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
    tracing::info!(
        "roost '{}' on {} (chain_id {}): mounting {} nest(s) — one shared cursor",
        meta.name,
        meta.chain,
        meta.chain_id,
        meta.nests.len(),
    );

    // The roost owns the chain connection: `--rpc` overrides win, else the roost's configured
    // endpoints. ONE source (cursor) drives every nest (RFC-0012 §2 — the density win).
    let rpc_urls = rpc::merge_rpcs(&rpc_override, meta.rpc_urls.clone());
    if rpc_urls.is_empty() {
        bail!(
            "roost '{}' has no rpc_urls (set them in {ROOST_FILE} or pass --rpc)",
            meta.name
        );
    }
    let concurrency = indexer::safe_backfill_concurrency(rpc_urls.len(), concurrency);
    let admin_enabled = indexer::admin_enabled(no_admin, &listen);
    let admin_token = indexer::admin_required_token(admin_enabled, &listen);

    // Load + chain-validate every mounted nest up front, and estimate each one's footprint. A failure
    // to mount any nest fails the whole roost — better a loud refusal at startup than a roost silently
    // serving a subset. Static and factory nests may be co-mounted (slice 2b); the cursor handles demux.
    let mut mounted = Vec::with_capacity(meta.nests.len());
    let mut estimates: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for name in &meta.nests {
        let (nest_path, config) = load_mounted_nest(&dir, meta, name)?;
        // Exposure view only spins up when the nest actually has labeled addresses (§3 estimate).
        let has_labels = !crate::labels::load(&nest_path).is_empty();
        estimates.insert(name.clone(), estimate_nest_rss_mb(&config, has_labels));
        mounted.push((name.clone(), nest_path, config));
    }

    // Per-runtime footprint budget (RFC-0012 §3): refuse a mount whose *projected* RSS exceeds
    // `max_rss` before it starts — density is RAM-bounded, not free. The projection is a rough estimate
    // (the `/nests` roster reports the real RSS alongside); the refusal is a real gate.
    let max_rss = meta.max_rss_mb.unwrap_or(DEFAULT_MAX_RSS_MB);
    let projected: u64 = ROOST_BASE_RSS_MB + estimates.values().sum::<u64>();
    tracing::info!(
        "roost footprint: ~{projected} MB projected (base {ROOST_BASE_RSS_MB} MB + {} nest(s)); budget {max_rss} MB",
        estimates.len()
    );
    if projected > max_rss {
        bail!(
            "roost '{}' projects ~{projected} MB but max_rss is {max_rss} MB — raise max_rss in \
             {ROOST_FILE}, drop a nest, or split into two roosts",
            meta.name
        );
    }

    // One shared source, one shared-cursor ingestion task driving all nests through the same per-window
    // code a solo `dev` runs (so per-nest tables are byte-identical to running each nest alone).
    let source: Arc<dyn Source> = Arc::new(RpcClient::new(rpc_urls)?);
    let (states, mut ingest, alert_workers) = indexer::spawn_roost(
        source,
        mounted,
        backfill,
        seal_direct,
        concurrency,
        window_override,
        admin_enabled,
        admin_token,
    )
    .await
    .with_context(|| format!("bringing up roost '{}'", meta.name))?;

    // Roster (`GET /nests`) built from the mounted states, with per-nest footprint attribution (§3)
    // and the roost's real resident set alongside the projection so operators can calibrate.
    let roster_entries: Vec<_> = states
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
        "chain": meta.chain,
        "projected_rss_mb": projected,
        "max_rss_mb": max_rss,
        "rss_bytes": crate::metrics::rss_bytes(),
        "nests": roster_entries,
    });

    // Fate-share the server with the single shared ingestion task: whichever ends first decides the
    // exit, and an ingestion error/panic propagates out (never serve stale data as if healthy).
    let result = tokio::select! {
        r = crate::serve::run_roost(&listen, roster, states) => r,
        joined = &mut ingest => match joined {
            Ok(inner) => inner,
            Err(e) if e.is_panic() => Err(anyhow::anyhow!("roost ingestion loop panicked")),
            Err(e) => Err(anyhow::anyhow!("roost ingestion loop task failed: {e}")),
        },
    };
    ingest.abort();
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

    #[test]
    fn loads_a_valid_roost() {
        let d = tempfile::tempdir().unwrap();
        write_roost(d.path(), "arbitrum-one", 42161, "arbitrum-one", 42161);
        let r = Roost::load(d.path()).unwrap();
        assert_eq!(r.roost.chain, "arbitrum-one");
        assert_eq!(r.roost.nests, vec!["a"]);
    }

    #[test]
    fn rejects_a_nest_on_the_wrong_chain() {
        let d = tempfile::tempdir().unwrap();
        // Roost says arbitrum-one; the nest claims mainnet → hard error.
        write_roost(d.path(), "arbitrum-one", 42161, "mainnet", 1);
        let roost = Roost::load(d.path()).unwrap();
        let err = load_mounted_nest(d.path(), &roost.roost, "a")
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs its own roost"), "got: {err}");
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
