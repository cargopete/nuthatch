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

/// The recipe library. Grows as more derivable reads are covered (reserves, …).
pub const RECIPES: &[Recipe] = &[
    Recipe {
        name: "total_supply",
        about: "ERC-20 totalSupply(), derived from Transfer mints/burns — no eth_call",
    },
    Recipe {
        name: "balances",
        about: "per-address token balance (current holders), derived from Transfers — no eth_call",
    },
    Recipe {
        name: "holder_count",
        about: "number of non-zero holders, derived from Transfers — no eth_call",
    },
    Recipe {
        name: "reserves",
        about: "Uniswap-V2 getReserves(): the latest Sync per pair (needs a `Sync` event) — no eth_call",
    },
];

/// The `SELECT` body of the `reserves` recipe: each pair's current reserves — the most recent `Sync`
/// event per contract address. This is exactly what Uniswap-V2 `getReserves()` returns, derived from
/// the `Sync(uint112,uint112)` events already indexed. Needs the nest to index the `Sync` event
/// (`{alias}__sync`).
pub fn reserves_select(alias: &str) -> String {
    format!(
        "SELECT address, \
           TRY_CAST(reserve0 AS HUGEINT) AS reserve0, \
           TRY_CAST(reserve1 AS HUGEINT) AS reserve1 \
         FROM (SELECT address, reserve0, reserve1, \
                 ROW_NUMBER() OVER (PARTITION BY address ORDER BY block_number DESC, log_index DESC) AS rn \
               FROM \"{alias}__sync\") \
         WHERE rn = 1"
    )
}

/// A per-address net-balance subquery over `{alias}__transfer`: +value to `to`, −value from `from`
/// (the same Σ(in) − Σ(out) the IVM balance view maintains). The building block for `balances` and
/// `holder_count`.
fn net_balance_subquery(alias: &str) -> String {
    let t = format!("{alias}__transfer");
    format!(
        "SELECT addr, SUM(d) AS balance FROM (\
           SELECT lower(\"to\") AS addr, TRY_CAST(\"value\" AS HUGEINT) AS d FROM \"{t}\" \
           UNION ALL \
           SELECT lower(\"from\") AS addr, -TRY_CAST(\"value\" AS HUGEINT) AS d FROM \"{t}\"\
         ) GROUP BY addr"
    )
}

/// The `SELECT` body of the `balances` recipe: each non-zero holder and its current balance.
pub fn balances_select(alias: &str) -> String {
    format!(
        "SELECT addr, balance FROM ({}) WHERE addr <> '{ZERO_ADDRESS}' AND balance <> 0",
        net_balance_subquery(alias)
    )
}

/// The `SELECT` body of the `holder_count` recipe: how many non-zero holders there are.
pub fn holder_count_select(alias: &str) -> String {
    format!(
        "SELECT COUNT(*) AS holders FROM ({}) WHERE addr <> '{ZERO_ADDRESS}' AND balance <> 0",
        net_balance_subquery(alias)
    )
}

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
        "balances" => Ok(format!(
            "-- Recipe: balances (RFC-0023 tier 1) — each address's current token balance, DERIVED from\n\
             -- the Transfer events already indexed as Σ(in) − Σ(out). No eth_call `balanceOf` per address.\n\
             -- Query it: SELECT * FROM {alias}_balances ORDER BY balance DESC\n\
             CREATE VIEW {alias}_balances AS\n{};\n",
            balances_select(alias)
        )),
        "holder_count" => Ok(format!(
            "-- Recipe: holder_count (RFC-0023 tier 1) — the number of non-zero holders, DERIVED from the\n\
             -- Transfer events already indexed. No eth_call. Query it: SELECT * FROM {alias}_holder_count\n\
             CREATE VIEW {alias}_holder_count AS\n{};\n",
            holder_count_select(alias)
        )),
        "reserves" => Ok(format!(
            "-- Recipe: reserves (RFC-0023 tier 1) — Uniswap-V2 `getReserves()`, DERIVED as the latest\n\
             -- `Sync(uint112,uint112)` event per pair. No eth_call, no archive node. Requires the nest to\n\
             -- index the `Sync` event (`{alias}__sync`). Query it: SELECT * FROM {alias}_reserves\n\
             CREATE VIEW {alias}_reserves AS\n{};\n",
            reserves_select(alias)
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
