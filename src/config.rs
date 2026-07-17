//! The one config file a nest has: `nuthatch.toml`.
//!
//! v2 (RFC-0001) is a `[nest]` header plus a `[[contracts]]` array — many contracts per nest. A
//! v1 file (single top-level `address`) is migrated transparently on load, so existing projects
//! keep working.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONFIG_FILE: &str = "nuthatch.toml";
pub const DB_FILE: &str = "nuthatch.redb";
/// v1 default ABI filename, retained for migration of old single-contract projects.
pub const ABI_FILE: &str = "abi.json";

/// The nest-config schema this build understands. A nest declaring a higher version is rejected on
/// load (it was authored by a newer nuthatch) — the guard that makes `init --from` safe.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub nest: Nest,
    #[serde(default)]
    pub contracts: Vec<Contract>,
    /// Optional sanctions-screening stage (RFC-0008 C2). When present with a non-empty `lists`, the
    /// indexer screens every transfer against those list snapshots live and records `sanction_hit`
    /// annotations. Absent → no screening, zero cost. Not serialised when empty (keeps nests clean).
    #[serde(default, skip_serializing_if = "Screening::is_empty")]
    pub screening: Screening,
    /// Optional threshold & velocity flags (RFC-0008 C3). Absent → no flags, zero cost.
    #[serde(default, skip_serializing_if = "Flags::is_empty")]
    pub flags: Flags,
    /// Optional alert webhook sinks (RFC-0008 C5). Each routes annotations of the named kinds to a
    /// URL. Absent → no alerts. Delivery is at-least-once via a durable outbox; a stalled sink never
    /// blocks indexing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<Alert>,
    /// Optional child-contract templates (RFC-0009). A template is an ABI applied to contracts
    /// discovered at runtime by a [`Factory`], rather than a fixed address. Absent → no factories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Optional factory rules (RFC-0009): a watched contract's event announces a child contract to
    /// index with a template. Absent → static nest (no dynamic discovery).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factories: Vec<Factory>,
    /// Optional user webhooks (RFC-0010 Part B): POST rows of an event table matching a predicate to
    /// a URL as they seal. Feeds the same host-side delivery engine as the compliance alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,
}

/// A user webhook subscription (RFC-0010 Part B): rows of `table` matching `where` are POSTed to `url`.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Webhook {
    pub name: String,
    /// The event table to watch, e.g. `staking__stake_delegated`.
    pub table: String,
    /// Optional SQL predicate over the table's columns (operator-authored, trusted like nest views).
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<String>,
    pub url: String,
    /// Max rows per delivery POST (default [`crate::webhooks::DEFAULT_BATCH_MAX`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_max: Option<usize>,
    /// `"sealed"` (default — never lies, finality-gated) or `"tip"` (fast, may send retractions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finality: Option<String>,
    /// Where the cursor starts on first registration: `"registration"` (default — only rows sealed
    /// after the webhook is added, so a `--seal-direct` backfill doesn't fire history), `"genesis"`,
    /// or a block number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Optional HMAC-SHA256 secret. When set, each delivery carries an `X-Nuthatch-Signature:
    /// sha256=<hex>` header over the POST body, so the receiver can verify it came from this nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// One alert webhook sink: annotations whose kind is in `kinds` are POSTed to `url` (RFC-0008 C5).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Alert {
    /// Annotation kinds to deliver, e.g. `["sanction_hit", "threshold_flag"]`.
    pub kinds: Vec<String>,
    /// The webhook endpoint. The operator configures it — it is the delivery allowlist (a sink only
    /// ever POSTs to the URLs a nest declares here).
    pub url: String,
}

/// A child-contract template (RFC-0009): a name + a vendored ABI, applied to every contract a
/// factory discovers. All children of one template share tables (`{template}__{event}`),
/// distinguished by the implicit `address` column.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Template {
    pub name: String,
    /// ABI path relative to the nest dir, e.g. "abis/uniswap_v3_pool.json".
    pub abi: String,
    /// Backfill filter strategy override (RFC-0009 §4): `"topic0"` forces the topic0-only fetch (with
    /// local registry-lookup filtering) instead of the address list — useful when a template is known
    /// to have many children. Omit for the automatic address-list → topic0 flip above ~500 children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

/// A factory rule (RFC-0009): when `watch`'s `event` fires, the child address in `child_param` is
/// indexed under `template`. `watch` is a `[[contracts]]` alias or another template (nested).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Factory {
    /// The alias of the watched contract (or a template, for nested factories).
    pub watch: String,
    /// The announcing event name, e.g. "PoolCreated".
    pub event: String,
    /// The event parameter holding the child contract address, e.g. "pool".
    pub child_param: String,
    /// Which [`Template`] to apply to the discovered child.
    pub template: String,
    /// Optional: only honour discoveries at or after this block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
}

/// One alert webhook sink: annotations whose kind is in `kinds` are POSTed to `url` (RFC-0008 C5).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Screening {
    /// Content-addressed list-snapshot hashes to screen against (see `nuthatch lists fetch`).
    #[serde(default)]
    pub lists: Vec<String>,
}

impl Screening {
    fn is_empty(&self) -> bool {
        self.lists.is_empty()
    }
}

/// Threshold & velocity flag configuration (RFC-0008 C3). Amounts are token **base units** as decimal
/// strings (i128 — no currency conversion in-core, per the RFC). Both flavours are opt-in.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Flags {
    /// Flag any single transfer whose value ≥ this many base units (travel-rule style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
    /// Flag an address whose outbound volume within a block-window reaches this many base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_amount: Option<String>,
    /// The velocity block-window size. Blocks, not wall-clock: an honest approximation of "~24h"
    /// (≈ 7200 blocks on 12s-block mainnet). Defaults to [`DEFAULT_VELOCITY_WINDOW`] when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_window: Option<u64>,
}

/// Default velocity window (~24h of 12s mainnet blocks). Documented as a block-count approximation.
pub const DEFAULT_VELOCITY_WINDOW: u64 = 7_200;

impl Flags {
    fn is_empty(&self) -> bool {
        self.threshold.is_none() && self.velocity_amount.is_none() && self.velocity_window.is_none()
    }

    /// The single-transfer threshold in base units, if configured and parseable.
    pub fn threshold_amount(&self) -> Option<i128> {
        self.threshold.as_deref().and_then(|s| s.parse().ok())
    }

    /// The velocity `(amount, window)` in `(base units, blocks)`, if an amount is configured.
    pub fn velocity(&self) -> Option<(i128, u64)> {
        let amount = self
            .velocity_amount
            .as_deref()
            .and_then(|s| s.parse::<i128>().ok())?;
        let window = self
            .velocity_window
            .unwrap_or(DEFAULT_VELOCITY_WINDOW)
            .max(1);
        Some((amount, window))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Nest {
    pub name: String,
    pub chain: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    /// Config schema version (see `CURRENT_SCHEMA_VERSION`). Absent in older nests → treated as 1.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Contract {
    pub alias: String,
    pub address: String,
    /// Deployment block (auto-detected at init); None → backfill from a tip offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_block: Option<u64>,
    /// ABI path relative to the nest dir, e.g. "abis/usdc.json".
    pub abi: String,
    /// Optional per-contract event allowlist (RFC-0011): only these events (by ABI name) are decoded
    /// and stored for this contract. Empty (the default) indexes every event the ABI defines. This is
    /// how a nest indexing e.g. GraphToken keeps only `Transfer` instead of millions of irrelevant
    /// rows. A name here that the ABI doesn't define is a config error, caught at registry build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

impl Config {
    pub fn load(dir: &Path) -> Result<Config> {
        let path = dir.join(CONFIG_FILE);
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no {CONFIG_FILE} in {} — run `nuthatch init` first",
                dir.display()
            )
        })?;
        // v2 first; fall back to migrating a v1 file.
        let cfg = match toml::from_str::<Config>(&raw) {
            Ok(cfg) => cfg,
            Err(v2_err) => Self::from_v1(&raw).map_err(|v1_err| {
                anyhow!("nuthatch.toml is neither v2 ({v2_err}) nor v1 ({v1_err})")
            })?,
        };
        if cfg.nest.schema_version > CURRENT_SCHEMA_VERSION {
            bail!(
                "this nest needs config schema v{} but this nuthatch supports up to v{} — upgrade nuthatch",
                cfg.nest.schema_version,
                CURRENT_SCHEMA_VERSION
            );
        }
        Ok(cfg)
    }

    fn from_v1(raw: &str) -> Result<Config> {
        #[derive(Deserialize)]
        struct V1 {
            chain: String,
            chain_id: u64,
            address: String,
            rpc_urls: Vec<String>,
        }
        let v1: V1 = toml::from_str(raw)?;
        Ok(Config {
            nest: Nest {
                name: "nest".to_string(),
                chain: v1.chain,
                chain_id: v1.chain_id,
                rpc_urls: v1.rpc_urls,
                schema_version: CURRENT_SCHEMA_VERSION,
            },
            contracts: vec![Contract {
                alias: "c0".to_string(),
                address: v1.address,
                start_block: None,
                abi: ABI_FILE.to_string(),
                events: Vec::new(),
            }],
            screening: Screening::default(),
            flags: Flags::default(),
            alerts: Vec::new(),
            templates: Vec::new(),
            factories: Vec::new(),
            webhooks: Vec::new(),
        })
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(self).context("failed to serialise config")?;
        std::fs::write(&path, raw)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// The first contract — the indexer's single-contract path uses this until step 3 generalises
    /// decode + storage to every contract in the nest.
    pub fn primary(&self) -> Result<&Contract> {
        self.contracts
            .first()
            .ok_or_else(|| anyhow!("nest has no contracts"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_a_v1_file() {
        let v1 = r#"
            chain = "mainnet"
            chain_id = 1
            address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            rpc_urls = ["https://rpc.example"]
            event = "Transfer"
        "#;
        let cfg = Config::from_v1(v1).unwrap();
        assert_eq!(cfg.nest.chain, "mainnet");
        assert_eq!(cfg.nest.chain_id, 1);
        assert_eq!(cfg.contracts.len(), 1);
        assert_eq!(cfg.contracts[0].alias, "c0");
        assert_eq!(cfg.contracts[0].abi, ABI_FILE);
        assert!(cfg.contracts[0].start_block.is_none());
    }

    #[test]
    fn roundtrips_a_v2_file() {
        let cfg = Config {
            nest: Nest {
                name: "my-nest".into(),
                chain: "mainnet".into(),
                chain_id: 1,
                rpc_urls: vec!["https://rpc.example".into()],
                schema_version: CURRENT_SCHEMA_VERSION,
            },
            contracts: vec![
                Contract {
                    alias: "usdc".into(),
                    address: "0xaaaa".into(),
                    start_block: Some(6_082_465),
                    abi: "abis/usdc.json".into(),
                    events: Vec::new(),
                },
                Contract {
                    alias: "weth".into(),
                    address: "0xbbbb".into(),
                    start_block: None,
                    abi: "abis/weth.json".into(),
                    events: vec!["Transfer".into()],
                },
            ],
            screening: Screening::default(),
            flags: Flags::default(),
            alerts: Vec::new(),
            templates: Vec::new(),
            factories: Vec::new(),
            webhooks: Vec::new(),
        };
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.contracts.len(), 2);
        assert_eq!(back.contracts[0].start_block, Some(6_082_465));
        assert_eq!(back.contracts[1].start_block, None);
        // The per-contract event allowlist (RFC-0011) round-trips; an empty one stays empty.
        assert!(back.contracts[0].events.is_empty());
        assert_eq!(back.contracts[1].events, vec!["Transfer".to_string()]);
        assert_eq!(back.primary().unwrap().alias, "usdc");
    }
}
