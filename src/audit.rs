//! The audit surface (RFC-0008 C6): the "prove it" commands. `audit replay` re-runs the pure
//! screening stage over the sealed segments and diffs the result against the stored `sanction_hit`
//! annotations — if they match, the annotations are reproducible from (list snapshot, block range,
//! component), which is the whole point of the pure/deterministic design. `audit report` summarises
//! the hits and flags in a range with the list-snapshot hashes and block bounds, for a human record.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

/// The identity of a screening hit, used to diff re-computed vs stored annotations. Deterministic:
/// `(block, log_index, sanctioned address, side, list snapshot)`.
type HitKey = (u64, u64, String, String, String);

/// `nuthatch audit replay --from --to` — re-screen the sealed transfers in a range and confirm the
/// stored `sanction_hit` annotations are exactly reproduced. Returns the verdict + any differences.
pub fn replay(dir: &Path, from: u64, to: u64) -> Result<ReplayReport> {
    let config = crate::config::Config::load(dir)?;
    let registry = crate::registry::DecodeRegistry::from_nest(dir, &config)?;
    let tables = crate::screen::transfer_tables(&registry);

    // Re-compute the hits: for each configured list, screen the sealed transfers over the range.
    let mut recomputed: BTreeSet<HitKey> = BTreeSet::new();
    if !config.screening.lists.is_empty() {
        let rt = crate::screen::load_runtime(dir)?;
        let transfers = crate::screen::read_sealed_transfers(dir, &tables, from, to)?;
        for list_hash in &config.screening.lists {
            let addresses = crate::lists::load(dir, list_hash)?;
            for (_key, ann) in crate::screen::screen_batch(&rt, &transfers, list_hash, &addresses)?
            {
                recomputed.insert(hit_key(&ann));
            }
        }
    }

    // The stored hits over the same range (sealed sanction_hit annotations).
    let mut stored: BTreeSet<HitKey> = BTreeSet::new();
    let sql = format!(
        "SELECT block_number, log_index, address, side, list_snapshot FROM sanction_hit \
         WHERE block_number BETWEEN {from} AND {to}"
    );
    if let Ok(rows) = crate::analytics::query(dir, &sql) {
        for r in rows {
            if let Some(k) = row_hit_key(&r) {
                stored.insert(k);
            }
        }
    }

    let missing: Vec<HitKey> = stored.difference(&recomputed).cloned().collect();
    let extra: Vec<HitKey> = recomputed.difference(&stored).cloned().collect();
    Ok(ReplayReport {
        from,
        to,
        recomputed: recomputed.len(),
        stored: stored.len(),
        missing,
        extra,
    })
}

/// The result of a replay. `matches()` is the verdict: stored annotations reproduce exactly.
#[derive(Debug)]
pub struct ReplayReport {
    pub from: u64,
    pub to: u64,
    pub recomputed: usize,
    pub stored: usize,
    /// Stored hits the re-run did NOT reproduce (stored ∖ recomputed).
    pub missing: Vec<HitKey>,
    /// Hits the re-run produced that aren't stored (recomputed ∖ stored).
    pub extra: Vec<HitKey>,
}

impl ReplayReport {
    pub fn matches(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// `nuthatch audit report --from --to` — a summary of the hits and flags in a range, with the list
/// snapshots involved and the block bounds. Returned as JSON; the CLI can render markdown from it.
pub fn report(dir: &Path, from: u64, to: u64) -> Result<Value> {
    let hits = crate::analytics::query(
        dir,
        &format!(
            "SELECT count(*) AS n, count(DISTINCT list_snapshot) AS lists, \
                    min(block_number) AS lo, max(block_number) AS hi \
             FROM sanction_hit WHERE block_number BETWEEN {from} AND {to}"
        ),
    )
    .unwrap_or_default();
    let snapshots = crate::analytics::query(
        dir,
        &format!(
            "SELECT DISTINCT list_snapshot FROM sanction_hit WHERE block_number BETWEEN {from} AND {to}"
        ),
    )
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r["list_snapshot"].as_str().map(str::to_string))
    .collect::<Vec<_>>();
    let flags = crate::analytics::query(
        dir,
        &format!(
            "SELECT count(*) AS n, min(block_number) AS lo, max(block_number) AS hi \
             FROM threshold_flag WHERE block_number BETWEEN {from} AND {to}"
        ),
    )
    .unwrap_or_default();

    let hit_row = hits.first().cloned().unwrap_or(json!({}));
    let flag_row = flags.first().cloned().unwrap_or(json!({}));
    Ok(json!({
        "range": { "from": from, "to": to },
        "sanction_hits": {
            "count": hit_row.get("n").cloned().unwrap_or(json!(0)),
            "distinct_lists": hit_row.get("lists").cloned().unwrap_or(json!(0)),
            "list_snapshots": snapshots,
            "block_bounds": [hit_row.get("lo").cloned().unwrap_or(json!(null)), hit_row.get("hi").cloned().unwrap_or(json!(null))],
        },
        "threshold_flags": {
            "count": flag_row.get("n").cloned().unwrap_or(json!(0)),
            "block_bounds": [flag_row.get("lo").cloned().unwrap_or(json!(null)), flag_row.get("hi").cloned().unwrap_or(json!(null))],
        },
    }))
}

/// Render a report JSON as a compact markdown record for a human audit file.
pub fn report_markdown(r: &Value) -> String {
    let sh = &r["sanction_hits"];
    let tf = &r["threshold_flags"];
    format!(
        "# Compliance audit report\n\n\
         - Range: blocks {}–{}\n\
         - Sanction hits: {} across {} list snapshot(s)\n\
         - List snapshots: {}\n\
         - Threshold flags: {}\n",
        r["range"]["from"],
        r["range"]["to"],
        sh["count"],
        sh["distinct_lists"],
        sh["list_snapshots"],
        tf["count"],
    )
}

fn hit_key(ann: &Value) -> HitKey {
    (
        ann["block_number"].as_u64().unwrap_or(0),
        ann["log_index"].as_u64().unwrap_or(0),
        ann["address"].as_str().unwrap_or("").to_string(),
        ann["side"].as_str().unwrap_or("").to_string(),
        ann["list_snapshot"].as_str().unwrap_or("").to_string(),
    )
}

fn row_hit_key(r: &Value) -> Option<HitKey> {
    Some((
        r["block_number"].as_u64()?,
        r["log_index"].as_u64()?,
        r["address"].as_str()?.to_string(),
        r["side"].as_str()?.to_string(),
        r["list_snapshot"].as_str()?.to_string(),
    ))
}

/// CLI entry: replay/report dispatch.
pub fn run(args: crate::cli::AuditArgs) -> Result<()> {
    use std::path::PathBuf;
    match args.what {
        crate::cli::AuditWhat::Replay(a) => {
            let dir = PathBuf::from(&a.dir);
            let r = replay(&dir, a.from, a.to).context("replay failed")?;
            println!(
                "audit replay {}..={}: {} stored, {} recomputed",
                r.from, r.to, r.stored, r.recomputed
            );
            if r.matches() {
                println!("PASS — stored sanction_hit annotations reproduce exactly");
                Ok(())
            } else {
                println!(
                    "FAIL — {} stored hit(s) not reproduced, {} recomputed hit(s) not stored",
                    r.missing.len(),
                    r.extra.len()
                );
                for k in r.missing.iter().take(5) {
                    println!("  missing: block {} log {} {} side {}", k.0, k.1, k.2, k.3);
                }
                anyhow::bail!("audit replay did not reproduce stored annotations")
            }
        }
        crate::cli::AuditWhat::Report(a) => {
            let dir = PathBuf::from(&a.dir);
            let r = report(&dir, a.from, a.to)?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", report_markdown(&r));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The C6 gate: a full compliance pipeline on a fixture — seal transfers, screen them to sealed
    /// `sanction_hit` annotations, then `audit replay` reproduces those hits exactly (PASS) and
    /// `audit report` summarises them. This is the end-to-end "prove it": stored annotations are
    /// reproducible from (list snapshot, block range, component).
    #[test]
    fn replay_reproduces_sealed_hits_and_report_summarises() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let bad = "0x1111111111111111111111111111111111111111";
        let clean = "0x00000000000000000000000000000000000000aa";

        // Nest config with screening enabled, and a vendored ABI so the registry builds.
        std::fs::create_dir_all(d.join("abis")).unwrap();
        std::fs::write(
            d.join("abis/usdc.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        let lf = d.join("l.csv");
        std::fs::write(&lf, format!("{bad},ofac\n")).unwrap();
        let (hash, _) = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async { crate::lists::fetch(d, "ofac-sdn", None, Some(&lf)).await })
            .unwrap();
        std::fs::write(
            d.join(crate::config::CONFIG_FILE),
            format!(
                r#"
[nest]
name = "audit-nest"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"

[screening]
lists = ["{hash}"]
"#
            ),
        )
        .unwrap();

        // Seal transfers: clean→bad (a hit) and clean→clean (no hit), in blocks 10–11.
        let transfers = vec![
            format!(
                r#"{{"table":"usdc__transfer","from":"{clean}","to":"{bad}","value":"500","block_number":10,"log_index":0,"tx_hash":"0xt1"}}"#
            ),
            format!(
                r#"{{"table":"usdc__transfer","from":"{clean}","to":"{clean}","value":"7","block_number":11,"log_index":0,"tx_hash":"0xt2"}}"#
            ),
        ];
        crate::seal::seal_range(d, &transfers, 10, 11).unwrap();

        // Screen the sealed range → seals `sanction_hit` annotations (the C2 backfill command).
        crate::screen::backfill(crate::cli::ScreenArgs {
            list: hash.clone(),
            from: 10,
            to: 11,
            dir: d.display().to_string(),
        })
        .unwrap();

        // audit replay reproduces the stored hit exactly.
        let r = replay(d, 10, 11).unwrap();
        assert_eq!(r.stored, 1, "one sealed sanction_hit");
        assert_eq!(r.recomputed, 1);
        assert!(
            r.matches(),
            "replay must reproduce: missing {:?} extra {:?}",
            r.missing,
            r.extra
        );

        // audit report summarises it.
        let rep = report(d, 10, 11).unwrap();
        assert_eq!(rep["sanction_hits"]["count"], Value::from(1u64));
        assert_eq!(
            rep["sanction_hits"]["list_snapshots"][0],
            Value::from(hash.as_str())
        );
    }
}
