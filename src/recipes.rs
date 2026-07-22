//! RFC-0023 tier 1 — **derive-first recipes.** The Foundation says >70% of subgraphs use `eth_call`,
//! but most of those reads are *derivable* from the events a nest already indexes — subgraphs fetch
//! them only because they have no incremental-view engine. Nuthatch does (the DBSP/IVM core, and
//! authored SQL views over the tip ∪ sealed surface), so a recipe here **replaces an eth_call with a
//! derivation**: deterministic, free, no archive node, and a capability subgraphs structurally lack.
//!
//! A recipe is an authored `CREATE VIEW` (RFC-0018 §1) over a nest's decoded tables. `nuthatch recipe
//! add <name>` drops one into the nest's `views/`. The flagship is `total_supply` — the ERC-20
//! `totalSupply()` computed as Σ minted − Σ burned (Transfers from / to the zero address).

use crate::config::Config;
use anyhow::{bail, Result};
use std::path::Path;

/// The zero address — an ERC-20 mint's `from` and a burn's `to`.
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// One derive-first recipe: a name and what eth_call it replaces.
pub struct Recipe {
    pub name: &'static str,
    pub about: &'static str,
}

/// The recipe library. Grows as more derivable reads are covered (reserves, holder counts, …).
pub const RECIPES: &[Recipe] = &[Recipe {
    name: "total_supply",
    about: "ERC-20 totalSupply(), derived from Transfer mints/burns — no eth_call",
}];

/// The `SELECT` body of the `total_supply` recipe over a contract's `{alias}__transfer` table:
/// Σ(value where `from` = 0x0) − Σ(value where `to` = 0x0). Exposed so it can be queried directly (and
/// tested) as well as wrapped in a `CREATE VIEW`.
pub fn total_supply_select(alias: &str) -> String {
    format!(
        "SELECT \
           COALESCE(SUM(CASE WHEN lower(\"from\") = '{ZERO_ADDRESS}' \
             THEN TRY_CAST(\"value\" AS HUGEINT) ELSE 0 END), 0) \
         - COALESCE(SUM(CASE WHEN lower(\"to\") = '{ZERO_ADDRESS}' \
             THEN TRY_CAST(\"value\" AS HUGEINT) ELSE 0 END), 0) \
         AS total_supply FROM \"{alias}__transfer\""
    )
}

/// The authored `CREATE VIEW` SQL for a recipe over the given contract alias.
pub fn view_sql(name: &str, alias: &str) -> Result<String> {
    match name {
        "total_supply" => Ok(format!(
            "-- Recipe: total_supply (RFC-0023 tier 1) — the ERC-20 `totalSupply()` a subgraph fetches\n\
             -- via eth_call, DERIVED here from the Transfer events already indexed: Σ minted − Σ burned\n\
             -- (transfers from / to the zero address). Deterministic and free — no eth_call, no archive\n\
             -- node. Query it: SELECT total_supply FROM {alias}_total_supply\n\
             CREATE VIEW {alias}_total_supply AS\n{};\n",
            total_supply_select(alias)
        )),
        _ => bail!(
            "unknown recipe '{name}' — available: {}",
            RECIPES
                .iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `nuthatch recipe list` — the available derive-first recipes.
pub fn list_cli() {
    println!("Derive-first recipes (RFC-0023) — each replaces an eth_call with a derivation:");
    for r in RECIPES {
        println!("  {:<14} {}", r.name, r.about);
    }
    println!("\nadd one with:  nuthatch recipe add <name> [--alias <contract>]");
}

/// `nuthatch recipe add <name> [--alias <a>] [--dir <d>]` — write a recipe's view into the nest's
/// `views/`. `alias` defaults to the nest's first contract. Never clobbers an existing file.
pub fn add_cli(dir: &Path, name: &str, alias: Option<&str>) -> Result<()> {
    let alias = match alias {
        Some(a) => a.to_string(),
        None => Config::load(dir)?.primary()?.alias.clone(),
    };
    let sql = view_sql(name, &alias)?;
    let views = dir.join("views");
    std::fs::create_dir_all(&views)?;
    let path = views.join(format!("{name}.sql"));
    if path.exists() {
        bail!(
            "{} already exists — remove it first, or edit it in place",
            path.display()
        );
    }
    std::fs::write(&path, sql)?;
    println!(
        "✓ wrote {} — a derived `{name}` view (no eth_call). Query it: \
         nuthatch sql \"SELECT * FROM {alias}_{name}\"",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_supply_select_targets_the_transfer_table_and_zero_address() {
        let sql = total_supply_select("usdc");
        assert!(sql.contains("\"usdc__transfer\""));
        assert!(sql.contains(ZERO_ADDRESS));
        assert!(sql.contains("total_supply"));
    }

    #[test]
    fn view_sql_wraps_a_create_view_and_rejects_unknown() {
        let v = view_sql("total_supply", "weth").unwrap();
        assert!(v.contains("CREATE VIEW weth_total_supply AS"));
        let err = view_sql("nope", "weth").unwrap_err().to_string();
        assert!(err.contains("unknown recipe"), "got: {err}");
    }
}
