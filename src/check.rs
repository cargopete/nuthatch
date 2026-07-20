//! `nuthatch check` — run a nest's invariant/parity checks (RFC-0002 §5).
//!
//! Each `checks/<name>.sql` is a read-only query run over the nest's sealed data (the same DuckDB
//! surface as `/sql`, so it sees the per-event tables *and* the nest's derived views). Its result is
//! compared to a recorded expected fixture `checks/expected/<name>.json`. For the Horizon nest those
//! fixtures are the deployed subgraph's answers at a pinned block, so this is a parity check; the
//! framework itself is generic (any nest can ship invariant checks).
//!
//! Hermetic by design — it compares against committed fixtures, not a live endpoint, so it runs in
//! CI with no network. `--update` re-records the fixtures from current results (authoring, run once
//! against known-good sealed data). Refreshing fixtures from a live subgraph is a nest-side chore,
//! not this command's job.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::analytics;
use crate::cli::CheckArgs;

pub fn check(args: CheckArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let checks = collect_checks(&dir, args.name.as_deref())?;
    if checks.is_empty() {
        bail!(
            "no checks found in {} (expected checks/*.sql)",
            dir.join("checks").display()
        );
    }
    let expected_dir = dir.join("checks").join("expected");
    if args.update {
        std::fs::create_dir_all(&expected_dir)
            .with_context(|| format!("cannot create {}", expected_dir.display()))?;
    }

    let mut failures = 0usize;

    // RFC-0018 §1: authored views are validated as part of `check` — a broken view, or one that
    // references a table/column the registry no longer has (**drift**), fails loudly with a
    // fuzzy-matched fix hint instead of vanishing silently. This runs before the parity checks so a
    // drifted view is caught even if it's the reason a parity check would fail.
    if let Some(schema) = nest_schema(&dir) {
        for issue in analytics::validate_nest_views(&dir, &schema) {
            let hint = issue
                .hint
                .map(|h| format!("\n    hint: {h}"))
                .unwrap_or_default();
            let first = issue.error.lines().next().unwrap_or(&issue.error);
            println!("✗ view {}: {first}{hint}", issue.file);
            failures += 1;
        }
    }

    for (name, sql_path) in &checks {
        let sql = std::fs::read_to_string(sql_path)
            .with_context(|| format!("cannot read {}", sql_path.display()))?;
        let got = match analytics::query(&dir, &sql) {
            Ok(rows) => rows,
            Err(e) => {
                println!("✗ {name}: query failed — {e:#}");
                failures += 1;
                continue;
            }
        };
        let exp_path = expected_dir.join(format!("{name}.json"));

        if args.update {
            std::fs::write(&exp_path, serde_json::to_string_pretty(&got)?)
                .with_context(|| format!("cannot write {}", exp_path.display()))?;
            println!("● {name}: recorded {} row(s)", got.len());
            continue;
        }

        let expected: Vec<Value> = match std::fs::read_to_string(&exp_path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("corrupt fixture {}", exp_path.display()))?,
            Err(_) => {
                println!(
                    "✗ {name}: no expected fixture — run `nuthatch check --update` to record it"
                );
                failures += 1;
                continue;
            }
        };

        match diff(&expected, &got) {
            None => println!("✓ {name}: {} row(s) match", got.len()),
            Some(msg) => {
                println!("✗ {name}: {msg}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{failures}/{} check(s) failed", checks.len());
    }
    println!("✓ all {} check(s) passed", checks.len());
    Ok(())
}

/// The nest's decode-registry table schemas, for view drift-validation. `None` if the dir isn't a
/// nest (no config) — view validation is then skipped, not fatal.
fn nest_schema(dir: &Path) -> Option<Vec<crate::registry::TableSchema>> {
    let cfg = crate::config::Config::load(dir).ok()?;
    let reg = crate::registry::DecodeRegistry::from_nest(dir, &cfg).ok()?;
    Some(reg.schema())
}

/// Every `checks/<name>.sql` (sorted), optionally filtered to names containing `filter`.
fn collect_checks(dir: &Path, filter: Option<&str>) -> Result<Vec<(String, PathBuf)>> {
    let checks_dir = dir.join("checks");
    let entries = match std::fs::read_dir(&checks_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .filter_map(|p| {
            let name = p.file_stem()?.to_string_lossy().into_owned();
            match filter {
                Some(f) if !name.contains(f) => None,
                _ => Some((name, p)),
            }
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Compare expected vs actual result sets. Returns None if identical, else a human diff of the first
/// discrepancy. Row order is significant — checks should `ORDER BY` for a deterministic comparison.
fn diff(expected: &[Value], got: &[Value]) -> Option<String> {
    if expected.len() != got.len() {
        return Some(format!(
            "row count differs: expected {}, got {}",
            expected.len(),
            got.len()
        ));
    }
    for (i, (e, g)) in expected.iter().zip(got).enumerate() {
        if e != g {
            return Some(format!(
                "row {i} differs:\n    expected: {e}\n    got:      {g}"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_detects_count_and_value_mismatches() {
        let a = vec![json!({"x": 1}), json!({"x": 2})];
        assert!(diff(&a, &a).is_none());
        assert!(diff(&a, &a[..1]).unwrap().contains("row count"));
        let b = vec![json!({"x": 1}), json!({"x": 9})];
        assert!(diff(&a, &b).unwrap().contains("row 1 differs"));
    }

    #[test]
    fn collect_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let checks = dir.path().join("checks");
        std::fs::create_dir_all(&checks).unwrap();
        std::fs::write(checks.join("parity_rewards.sql"), "SELECT 1").unwrap();
        std::fs::write(checks.join("parity_allocs.sql"), "SELECT 1").unwrap();
        std::fs::write(checks.join("other.sql"), "SELECT 1").unwrap();

        let all = collect_checks(dir.path(), None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "other"); // sorted
        let parity = collect_checks(dir.path(), Some("parity")).unwrap();
        assert_eq!(parity.len(), 2);
        assert!(parity.iter().all(|(n, _)| n.contains("parity")));
    }
}
