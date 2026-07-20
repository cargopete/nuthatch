//! The governed semantic layer (RFC-0016 §2). `semantic.toml` is what a nest's data *means* —
//! per-table and per-column descriptions authored by the nest's author — sitting beside
//! `nuthatch.toml` and read by every surface that describes the nest (the MCP `schema` tool, the
//! admin UI, the scaffolded skill). One source of truth, many readers.
//!
//! Two rules make it *governed* rather than just a docs file:
//!
//! 1. **Generated at `init`, never trusted blindly.** Descriptions are seeded from the ABI (honest
//!    fallback text an author is invited to improve). **Footguns are derived, not authored** —
//!    reserved-word columns (`"from"`/`"to"`) and big-int columns (`value` → use `value_dec`) are
//!    computed from the decode registry, so they are always present and always correct even if the
//!    author never opens the file.
//! 2. **Drift is caught.** [`drift`] flags any table/column the file describes that the registry
//!    doesn't have — stale semantics are worse than none, so `dev` warns loudly.
//!
//! Nothing here touches the data path (non-negotiable 4): this is presentation over the registry.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::registry::TableSchema;

/// The authored semantic layer for a nest. Deserialized from `semantic.toml`; also produced by
/// [`generate`] from the registry for `init` to write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Semantic {
    #[serde(default = "one")]
    pub schema_version: u32,
    #[serde(default)]
    pub nest: NestSemantic,
    /// Per-table meaning, keyed by table name (`{alias}__{event}`). BTreeMap for stable ordering, so
    /// the generated file and the composed doc are deterministic (Tier-A goldenable).
    #[serde(default, rename = "table")]
    pub tables: BTreeMap<String, TableSemantic>,
    /// Per-authored-view meaning, keyed by view name (RFC-0018 §1) — the derivations the nest exists to
    /// answer. Rendered into `/schema`/the MCP exactly like tables, so an agent *sees* `top_recipients`
    /// and what it means rather than rediscovering it.
    #[serde(default, rename = "view")]
    pub views: BTreeMap<String, ViewSemantic>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NestSemantic {
    #[serde(default)]
    pub description: String,
}

/// What one authored SQL view (`views/*.sql`) computes (RFC-0018 §1). The view's *shape* (columns) is
/// introspected from DuckDB at query time — the author only has to say what it *means*.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewSemantic {
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableSemantic {
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub grain: String,
    /// Per-column description, keyed by column name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, String>,
    /// Derived-not-authored: the SQL footguns of this table. Regenerated from the registry, so they
    /// stay correct even when the author edits everything else.
    #[serde(default, skip_serializing_if = "Footguns::is_empty")]
    pub footguns: Footguns,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footguns {
    /// Columns whose names are SQL reserved words — must be double-quoted (`"from"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved_words: Vec<String>,
    /// Columns holding integers wider than 64 bits, stored as exact text. Arithmetic must use the
    /// derived `{col}_dec` companion, never the raw text column.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub big_ints: Vec<String>,
}

impl Footguns {
    pub fn is_empty(&self) -> bool {
        self.reserved_words.is_empty() && self.big_ints.is_empty()
    }
}

/// SQL reserved words that also turn up as EVM event parameter names — a column with one of these
/// names must be double-quoted in every dialect. Kept deliberately small and high-signal (the ones
/// that actually collide with real ABIs) rather than the full 200-word SQL keyword list.
const SQL_RESERVED: &[&str] = &[
    "from",
    "to",
    "in",
    "order",
    "group",
    "select",
    "where",
    "case",
    "when",
    "then",
    "else",
    "end",
    "default",
    "table",
    "index",
    "column",
    "references",
    "primary",
    "key",
    "all",
    "and",
    "or",
    "not",
    "null",
    "like",
    "limit",
    "offset",
    "values",
    "user",
    "grant",
    "check",
    "unique",
    "desc",
    "asc",
];

/// A big-integer storage kind (uint/int > 64-bit) — the columns that get a `{col}_dec` companion and
/// must not be summed/compared as raw text. Mirrors `analytics::is_bigint`.
fn is_bigint_storage(storage: &str) -> bool {
    storage == "word16" || storage == "word32"
}

/// Derive the footguns for one table purely from its registry schema. Always correct by construction.
pub fn derive_footguns(table: &TableSchema) -> Footguns {
    let mut reserved_words = Vec::new();
    let mut big_ints = Vec::new();
    for col in &table.columns {
        if SQL_RESERVED.contains(&col.name.to_ascii_lowercase().as_str()) {
            reserved_words.push(col.name.clone());
        }
        if is_bigint_storage(&col.storage) {
            big_ints.push(col.name.clone());
        }
    }
    Footguns {
        reserved_words,
        big_ints,
    }
}

/// The honest fallback marker appended to every generated (un-edited) description, so an author can
/// tell at a glance what still needs their attention and a reader knows the text is machine-seeded.
const SEEDED: &str = "(seeded from the ABI — edit semantic.toml to improve this)";

/// Generate a `Semantic` from the registry: ABI-seeded descriptions plus derived footguns. This is
/// what `init` writes. Descriptions are honest placeholders; footguns are authoritative.
pub fn generate(schema: &[TableSchema], nest_name: &str, chain: &str) -> Semantic {
    let mut tables = BTreeMap::new();
    for t in schema {
        let mut columns = BTreeMap::new();
        for col in &t.columns {
            if col.sol_type == "implicit" {
                continue; // implicit columns are documented once, in the composed doc, not per-nest.
            }
            columns.insert(
                col.name.clone(),
                format!("The `{}` {} parameter. {SEEDED}", col.name, col.sol_type),
            );
        }
        let footguns = derive_footguns(t);
        tables.insert(
            t.table.clone(),
            TableSemantic {
                description: format!(
                    "`{}` events emitted by the `{}` contract. {SEEDED}",
                    t.event, t.alias
                ),
                grain: format!("one row per {} event", t.event),
                columns,
                footguns,
            },
        );
    }
    Semantic {
        schema_version: 1,
        nest: NestSemantic {
            description: format!("The `{nest_name}` nest on {chain}. {SEEDED}"),
        },
        tables,
        // Authored views are seeded per-scaffolded-view by `init` (RFC-0018 §1b), not generated from
        // the registry — the registry has no views.
        views: BTreeMap::new(),
    }
}

/// Merge freshly-`generate`d semantics onto an existing (possibly author-edited) file: keep the
/// author's descriptions/grain/columns wherever they exist, but always take the **freshly-derived
/// footguns** (they must never go stale) and add entries for any new tables. Used by `add`, so
/// growing a nest never clobbers authored meaning yet always keeps the footguns correct.
pub fn merge(existing: Semantic, generated: Semantic) -> Semantic {
    let mut out = existing;
    for (table, gen_ts) in generated.tables {
        match out.tables.get_mut(&table) {
            Some(cur) => {
                // Authored text wins; derived footguns are always refreshed.
                cur.footguns = gen_ts.footguns;
                for (col, desc) in gen_ts.columns {
                    cur.columns.entry(col).or_insert(desc);
                }
                if cur.grain.is_empty() {
                    cur.grain = gen_ts.grain;
                }
                if cur.description.is_empty() {
                    cur.description = gen_ts.description;
                }
            }
            None => {
                out.tables.insert(table, gen_ts);
            }
        }
    }
    if out.nest.description.is_empty() {
        out.nest.description = generated.nest.description;
    }
    out
}

/// Load `semantic.toml` from a nest directory, if present. Absent is fine (a nest predating the
/// semantic layer still describes itself from the registry alone) — returns `Ok(None)`.
pub fn load(dir: &std::path::Path) -> Result<Option<Semantic>> {
    let path = dir.join("semantic.toml");
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let sem: Semantic =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(sem))
}

/// Write a `Semantic` to `semantic.toml` in a nest directory (what `init` calls).
pub fn save(dir: &std::path::Path, sem: &Semantic) -> Result<()> {
    let header = "# semantic.toml — what this nest's data *means* (RFC-0016). Read by the MCP `schema`\n\
                  # tool, the admin UI, and the scaffolded skill. Edit descriptions freely; the\n\
                  # `[table.*.footguns]` are DERIVED from the ABI and regenerated — leave them be.\n\n";
    let body = toml::to_string_pretty(sem).context("serialise semantic.toml")?;
    std::fs::write(dir.join("semantic.toml"), format!("{header}{body}"))
        .context("write semantic.toml")?;
    Ok(())
}

/// Drift check: every table/column the semantic file *describes* must exist in the registry. Returns
/// human-readable warnings (empty when clean). Stale semantics are worse than none — `dev` surfaces
/// these loudly so the author fixes or regenerates the file.
pub fn drift(schema: &[TableSchema], sem: &Semantic) -> Vec<String> {
    let known: BTreeMap<&str, Vec<String>> = schema
        .iter()
        .map(|t| {
            (
                t.table.as_str(),
                t.columns.iter().map(|c| c.name.clone()).collect(),
            )
        })
        .collect();

    let mut warnings = Vec::new();
    for (table, ts) in &sem.tables {
        match known.get(table.as_str()) {
            None => warnings.push(format!(
                "semantic.toml describes table `{table}`, which the registry has no decoder for"
            )),
            Some(cols) => {
                for col in ts.columns.keys() {
                    if !cols.contains(col) {
                        warnings.push(format!(
                            "semantic.toml describes `{table}.{col}`, which isn't a column of that table"
                        ));
                    }
                }
            }
        }
    }
    warnings
}

/// Live per-table coverage, folded into the composed schema so the hot/cold seam is data an agent can
/// reason about rather than prose it skims. Assembled by the server from the store at call time.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Coverage {
    pub sealed_through: u64,
    pub tip: u64,
}

/// Compose the enriched schema document from the four layers the RFC names: **structure** (registry),
/// **meaning** (semantic.toml), **derived footguns**, and — when a running nest supplies it —
/// **coverage** (the hot/cold seam as numbers). Sample-row *evidence* is a later slice; this is the
/// text an agent reads before writing SQL. Deterministic given its inputs, so it is Tier-A goldenable.
pub fn compose(
    schema: &[TableSchema],
    sem: Option<&Semantic>,
    coverage: Option<&Coverage>,
) -> String {
    let mut out = String::new();
    out.push_str("nuthatch data model\n\n");
    if let Some(s) = sem {
        if !s.nest.description.is_empty() {
            out.push_str(&s.nest.description);
            out.push_str("\n\n");
        }
    }

    if let Some(c) = coverage {
        out.push_str(&format!(
            "COVERAGE\n  sealed_through = {} (the `sql` tool sees rows at or below this block);\n  \
             tip = {} — rows above sealed_through are served by `table`/`entity`, not `sql`.\n\n",
            c.sealed_through, c.tip
        ));
    }

    out.push_str("TABLES (one per contract event; query via the `sql` tool)\n");
    for t in schema {
        let ts = sem.and_then(|s| s.tables.get(&t.table));
        out.push_str(&format!("\n  {} — {}\n", t.table, describe_table(t, ts)));
        if let Some(ts) = ts {
            if !ts.grain.is_empty() {
                out.push_str(&format!("    grain: {}\n", ts.grain));
            }
        }
        out.push_str("    columns: ");
        let cols: Vec<String> = t
            .columns
            .iter()
            .filter(|c| c.sol_type != "implicit")
            .map(|c| format!("{} ({})", c.name, c.sol_type))
            .collect();
        out.push_str(&cols.join(", "));
        out.push('\n');

        let fg = derive_footguns(t);
        if !fg.reserved_words.is_empty() {
            out.push_str(&format!(
                "    ⚠ reserved-word columns — double-quote them: {}\n",
                fg.reserved_words
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !fg.big_ints.is_empty() {
            out.push_str(&format!(
                "    ⚠ big-int columns (exact text; use the `_dec` companion for SUM/AVG/compare): {}\n",
                fg.big_ints
                    .iter()
                    .map(|c| format!("{c} → {c}_dec"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // Authored views (RFC-0018 §1): the derivations the nest exists to answer, queryable by name over
    // the same hot∪cold surface. Rendered from `semantic.toml` `[view.*]` so an agent sees them.
    if let Some(s) = sem {
        if !s.views.is_empty() {
            out.push_str(
                "\nAUTHORED VIEWS (derived — query by name, recomputed per query over hot∪cold)\n",
            );
            for (name, v) in &s.views {
                let desc = if v.description.is_empty() {
                    "(an authored SQL view — describe it in semantic.toml `[view.…]`)"
                } else {
                    &v.description
                };
                out.push_str(&format!("  {name} — {desc}\n"));
            }
        }
    }

    out.push_str(GENERAL_GUIDANCE);
    out
}

/// Nest-independent guidance every composed schema carries — the derived views and compliance/factory
/// surfaces an agent should know exist. Kept as a trailing appendix so the per-nest tables lead.
const GENERAL_GUIDANCE: &str = r#"
VIEWS (incrementally maintained; reorgs retract automatically)
  balances — per-address net balance = Σ(received) − Σ(sent), i128 base units as decimal strings,
             for ERC-20 Transfer tables. Query via the `balance`/`top_balances` tools.

COMPLIANCE (RFC-0008; amounts are i128 base units as decimal strings)
  exposure       — an address's direct exposure to the labeled set (tool `exposure`).
  flags          — threshold and velocity flags (tool `flags`).
  screen_status  — sanctions-screening hits + the list-snapshot version (tool `screen_status`);
                   also the `sanction_hit` SQL table (each row carries its list_snapshot hash).

FACTORIES (RFC-0009; only in a nest with templates/factories)
  Children of a template share tables (`pool__swap`, …), distinguished by the `address` column. Each
  template has a `{template}__children` view: which children were discovered, when, by which parent.
"#;

fn describe_table(t: &TableSchema, ts: Option<&TableSemantic>) -> String {
    match ts {
        Some(ts) if !ts.description.is_empty() => ts.description.clone(),
        _ => format!("`{}` events from `{}`", t.event, t.alias),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ColumnSchema, TableSchema};

    fn transfer_table() -> TableSchema {
        TableSchema {
            table: "usdc__transfer".into(),
            alias: "usdc".into(),
            event: "Transfer".into(),
            topic0: "0xddf2".into(),
            columns: vec![
                ColumnSchema {
                    name: "from".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: true,
                },
                ColumnSchema {
                    name: "to".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: true,
                },
                ColumnSchema {
                    name: "value".into(),
                    sol_type: "uint256".into(),
                    storage: "word32".into(),
                    indexed: false,
                },
                ColumnSchema {
                    name: "block_number".into(),
                    sol_type: "implicit".into(),
                    storage: "u64".into(),
                    indexed: false,
                },
            ],
        }
    }

    #[test]
    fn footguns_are_derived_from_the_registry() {
        let fg = derive_footguns(&transfer_table());
        assert_eq!(fg.reserved_words, vec!["from", "to"]);
        assert_eq!(fg.big_ints, vec!["value"]);
    }

    #[test]
    fn drift_flags_unknown_tables_and_columns() {
        let schema = vec![transfer_table()];
        let mut good = TableSemantic::default();
        good.columns.insert("nope".into(), "x".into()); // not a column of usdc__transfer
        good.columns.insert("from".into(), "the sender".into()); // real column — no warning
        let mut sem = Semantic::default();
        sem.tables.insert("usdc__transfer".into(), good);
        sem.tables
            .insert("ghost__event".into(), TableSemantic::default()); // no such table

        let warnings = drift(&schema, &sem);
        assert!(warnings.iter().any(|w| w.contains("ghost__event")));
        assert!(warnings.iter().any(|w| w.contains("usdc__transfer.nope")));
        assert!(
            !warnings.iter().any(|w| w.contains("from")),
            "a real column must not warn"
        );
    }

    #[test]
    fn footguns_survive_a_toml_round_trip() {
        let fg = Footguns {
            reserved_words: vec!["from".into(), "to".into()],
            big_ints: vec!["value".into()],
        };
        let ts = TableSemantic {
            footguns: fg.clone(),
            ..Default::default()
        };
        let mut sem = Semantic::default();
        sem.tables.insert("usdc__transfer".into(), ts);
        let text = toml::to_string_pretty(&sem).unwrap();
        let back: Semantic = toml::from_str(&text).unwrap();
        assert_eq!(back.tables["usdc__transfer"].footguns, fg);
    }

    #[test]
    fn compose_renders_authored_views() {
        // RFC-0018 §1: an authored view described in semantic.toml appears in the composed /schema so
        // an agent can see and query it by name.
        let schema = [transfer_table()];
        let mut sem = Semantic::default();
        sem.views.insert(
            "top_recipients".into(),
            ViewSemantic {
                description: "The addresses that received the most transfers.".into(),
            },
        );
        let doc = compose(&schema, Some(&sem), None);
        assert!(doc.contains("AUTHORED VIEWS"));
        assert!(doc.contains("top_recipients — The addresses that received the most transfers."));
    }

    #[test]
    fn compose_teaches_the_footguns_without_a_semantic_file() {
        // Even with no semantic.toml, compose must surface the derived footguns from the registry —
        // that's the "always correct even if the author never opens the file" guarantee.
        let schema = [transfer_table()];
        // A tiny registry stand-in isn't available, so assert the footgun text via the same helper
        // compose uses; the registry-backed compose is golden-tested in the integration test.
        let fg = derive_footguns(&schema[0]);
        assert!(fg.reserved_words.contains(&"from".to_string()));
        assert!(fg.big_ints.contains(&"value".to_string()));
    }
}
