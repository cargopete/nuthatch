//! RFC-0020: nest lifecycle — the compatible-vs-breaking classifier (slice 1).
//!
//! An update `vN → vN+1` is **compatible** when every existing downstream query keeps working with
//! unchanged meaning, and **breaking** otherwise. Settled rule (RFC-0020): compatible = *additive
//! only* (a new table or column; nothing existing removed, renamed, retyped, or semantically
//! changed); breaking = anything a consumer can observe as removed/renamed/retyped/re-meant. A
//! conservative default falls to **breaking** when in doubt.
//!
//! This slice classifies over the **schema surface** — `schema.json`, the decoded event tables and
//! their columns, which is the concrete machine-readable contract a consumer queries (`SELECT … FROM
//! c0__transfer`). Later slices act on the verdict: compatible → hot-swap behind the same endpoint;
//! breaking → a new versioned endpoint run alongside the old. This slice only *decides*; it moves no
//! data and touches no serving path.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The classification of an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Additive-only (or no change): safe to hot-swap behind the same endpoint.
    Compatible,
    /// A consumer-observable break: needs a new versioned endpoint, run alongside the old.
    Breaking,
}

/// A single schema difference between two versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    TableAdded(String),
    TableRemoved(String),
    ColumnAdded {
        table: String,
        column: String,
    },
    ColumnRemoved {
        table: String,
        column: String,
    },
    ColumnRetyped {
        table: String,
        column: String,
        from: String,
        to: String,
    },
}

impl Change {
    /// Additive changes preserve every existing query; everything else is consumer-observable and
    /// therefore breaking.
    pub fn is_breaking(&self) -> bool {
        !matches!(self, Change::TableAdded(_) | Change::ColumnAdded { .. })
    }

    /// A one-line, human-legible description for CLI/logs.
    pub fn describe(&self) -> String {
        match self {
            Change::TableAdded(t) => format!("table `{t}` added"),
            Change::TableRemoved(t) => format!("table `{t}` removed"),
            Change::ColumnAdded { table, column } => format!("column `{table}.{column}` added"),
            Change::ColumnRemoved { table, column } => format!("column `{table}.{column}` removed"),
            Change::ColumnRetyped {
                table,
                column,
                from,
                to,
            } => format!("column `{table}.{column}` retyped {from} → {to}"),
        }
    }
}

/// The full result: a verdict plus the changes that produced it (deterministically ordered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub verdict: Verdict,
    pub changes: Vec<Change>,
}

impl Classification {
    /// The subset of changes that force a breaking verdict.
    pub fn breaking_changes(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| c.is_breaking())
    }

    /// The additive (compatible) changes.
    pub fn additive_changes(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| !c.is_breaking())
    }
}

/// A column's observable type — the pair a consumer sees. `indexed` is deliberately excluded: it
/// affects query cost, not shape or meaning, so flipping it is not a break.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ColType {
    sol_type: String,
    storage: String,
}

impl ColType {
    fn describe(&self) -> String {
        format!("{}({})", self.sol_type, self.storage)
    }
}

/// A normalized schema: `table → (column → type)`, ordered for deterministic diffs.
type Schema = BTreeMap<String, BTreeMap<String, ColType>>;

#[derive(Deserialize)]
struct SchemaDoc {
    #[serde(default)]
    tables: Vec<TableDoc>,
}

#[derive(Deserialize)]
struct TableDoc {
    table: String,
    #[serde(default)]
    columns: Vec<ColDoc>,
}

#[derive(Deserialize)]
struct ColDoc {
    name: String,
    #[serde(default)]
    sol_type: String,
    #[serde(default)]
    storage: String,
}

/// Parse a `schema.json` document into the normalized [`Schema`].
fn parse_schema(json: &str) -> Result<Schema> {
    let doc: SchemaDoc = serde_json::from_str(json).context("parsing schema.json")?;
    let mut schema = Schema::new();
    for t in doc.tables {
        let cols = t
            .columns
            .into_iter()
            .map(|c| {
                (
                    c.name,
                    ColType {
                        sol_type: c.sol_type,
                        storage: c.storage,
                    },
                )
            })
            .collect();
        schema.insert(t.table, cols);
    }
    Ok(schema)
}

/// Classify `old → new` over two normalized schemas. Pure and deterministic.
fn classify(old: &Schema, new: &Schema) -> Classification {
    let mut changes = Vec::new();

    let tables: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for table in tables {
        match (old.get(table), new.get(table)) {
            (None, Some(_)) => changes.push(Change::TableAdded(table.clone())),
            (Some(_), None) => changes.push(Change::TableRemoved(table.clone())),
            (Some(old_cols), Some(new_cols)) => {
                let cols: BTreeSet<&String> = old_cols.keys().chain(new_cols.keys()).collect();
                for column in cols {
                    match (old_cols.get(column), new_cols.get(column)) {
                        (None, Some(_)) => changes.push(Change::ColumnAdded {
                            table: table.clone(),
                            column: column.clone(),
                        }),
                        (Some(_), None) => changes.push(Change::ColumnRemoved {
                            table: table.clone(),
                            column: column.clone(),
                        }),
                        (Some(a), Some(b)) if a != b => changes.push(Change::ColumnRetyped {
                            table: table.clone(),
                            column: column.clone(),
                            from: a.describe(),
                            to: b.describe(),
                        }),
                        _ => {}
                    }
                }
            }
            (None, None) => unreachable!("table came from the union of both key sets"),
        }
    }

    let verdict = if changes.iter().any(Change::is_breaking) {
        Verdict::Breaking
    } else {
        Verdict::Compatible
    };
    Classification { verdict, changes }
}

/// Classify an update from two `schema.json` documents. Errors only on malformed JSON — a real fault,
/// distinct from a *breaking* verdict.
pub fn classify_schemas(old_json: &str, new_json: &str) -> Result<Classification> {
    let old = parse_schema(old_json)?;
    let new = parse_schema(new_json)?;
    Ok(classify(&old, &new))
}

/// Resolve a `schema.json` from a path that is either a nest directory or the file itself.
fn schema_path(p: &Path) -> PathBuf {
    if p.is_dir() {
        p.join("schema.json")
    } else {
        p.to_path_buf()
    }
}

/// `nuthatch nest diff <old> <new>`: classify an update between two nests (each a nest dir or a
/// `schema.json` path) and print the verdict with its reasons. Slice 1 is decision-only — it prints
/// what a later slice will *act* on (compatible → same endpoint; breaking → a new one).
pub fn diff_cli(old: &Path, new: &Path) -> Result<()> {
    let old_json = std::fs::read_to_string(schema_path(old))
        .with_context(|| format!("reading old schema from {}", old.display()))?;
    let new_json = std::fs::read_to_string(schema_path(new))
        .with_context(|| format!("reading new schema from {}", new.display()))?;
    let c = classify_schemas(&old_json, &new_json)?;

    let additive = c.additive_changes().count();
    match c.verdict {
        Verdict::Compatible => {
            println!(
                "✓ compatible — {additive} additive change(s), nothing removed/retyped. Safe to \
                 hot-swap behind the same endpoint."
            );
            for ch in c.additive_changes() {
                println!("  + {}", ch.describe());
            }
        }
        Verdict::Breaking => {
            println!(
                "✗ breaking — a consumer-observable change. Serve on a NEW versioned endpoint, run \
                 it alongside the old, and let downstream migrate on their clock."
            );
            for ch in c.breaking_changes() {
                println!("  - {}", ch.describe());
            }
            if additive > 0 {
                println!("  ({additive} additive change(s) also present)");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny two-table schema: `c0__transfer(from,to,value)` and `c0__approval(owner,spender)`.
    fn base() -> String {
        r#"{
          "registry_hash": "0xabc",
          "tables": [
            {"table":"c0__transfer","alias":"c0","event":"Transfer","topic0":"0x0","columns":[
              {"name":"from","sol_type":"address","storage":"address","indexed":true},
              {"name":"to","sol_type":"address","storage":"address","indexed":true},
              {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}
            ]},
            {"table":"c0__approval","alias":"c0","event":"Approval","topic0":"0x1","columns":[
              {"name":"owner","sol_type":"address","storage":"address","indexed":true},
              {"name":"spender","sol_type":"address","storage":"address","indexed":true}
            ]}
          ]
        }"#
        .to_string()
    }

    fn verdict(old: &str, new: &str) -> Classification {
        classify_schemas(old, new).unwrap()
    }

    #[test]
    fn identical_is_compatible_with_no_changes() {
        let c = verdict(&base(), &base());
        assert_eq!(c.verdict, Verdict::Compatible);
        assert!(c.changes.is_empty());
    }

    #[test]
    fn added_table_and_column_are_compatible() {
        // Same two tables, but `approval` gains a `note` column and a whole new `c0__mint` table
        // appears — both additive, nothing existing touched.
        let new = r#"{
          "tables": [
            {"table":"c0__transfer","columns":[
              {"name":"from","sol_type":"address","storage":"address","indexed":true},
              {"name":"to","sol_type":"address","storage":"address","indexed":true},
              {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}
            ]},
            {"table":"c0__approval","columns":[
              {"name":"owner","sol_type":"address","storage":"address","indexed":true},
              {"name":"spender","sol_type":"address","storage":"address","indexed":true},
              {"name":"note","sol_type":"string","storage":"string","indexed":false}
            ]},
            {"table":"c0__mint","columns":[
              {"name":"amount","sol_type":"uint256","storage":"word32","indexed":false}
            ]}
          ]
        }"#;
        let c = verdict(&base(), new);
        assert_eq!(c.verdict, Verdict::Compatible, "changes: {:?}", c.changes);
        assert_eq!(c.additive_changes().count(), 2);
        assert_eq!(c.breaking_changes().count(), 0);
    }

    #[test]
    fn removed_table_is_breaking() {
        // Drop the approval table entirely.
        let new = r#"{"tables":[
            {"table":"c0__transfer","columns":[
              {"name":"from","sol_type":"address","storage":"address"},
              {"name":"to","sol_type":"address","storage":"address"},
              {"name":"value","sol_type":"uint256","storage":"word32"}
            ]}
          ]}"#;
        let c = verdict(&base(), new);
        assert_eq!(c.verdict, Verdict::Breaking);
        assert!(c
            .changes
            .contains(&Change::TableRemoved("c0__approval".to_string())));
    }

    #[test]
    fn removed_column_is_breaking() {
        let new = r#"{"tables":[
            {"table":"c0__transfer","columns":[
              {"name":"from","sol_type":"address","storage":"address"},
              {"name":"value","sol_type":"uint256","storage":"word32"}
            ]},
            {"table":"c0__approval","columns":[
              {"name":"owner","sol_type":"address","storage":"address"},
              {"name":"spender","sol_type":"address","storage":"address"}
            ]}
          ]}"#;
        let c = verdict(&base(), new);
        assert_eq!(c.verdict, Verdict::Breaking);
        assert!(c.changes.contains(&Change::ColumnRemoved {
            table: "c0__transfer".to_string(),
            column: "to".to_string(),
        }));
    }

    #[test]
    fn retyped_column_is_breaking() {
        // `value` goes uint256(word32) → uint128(word16): a consumer-observable retype.
        let new = base().replace(
            r#"{"name":"value","sol_type":"uint256","storage":"word32","indexed":false}"#,
            r#"{"name":"value","sol_type":"uint128","storage":"word16","indexed":false}"#,
        );
        let c = verdict(&base(), &new);
        assert_eq!(c.verdict, Verdict::Breaking);
        assert!(c.changes.iter().any(|ch| matches!(
            ch,
            Change::ColumnRetyped { table, column, .. }
            if table == "c0__transfer" && column == "value"
        )));
    }

    #[test]
    fn indexed_flag_change_alone_is_compatible() {
        // Flipping `indexed` changes query cost, not shape/meaning → not a break.
        let new = base().replace(
            r#"{"name":"from","sol_type":"address","storage":"address","indexed":true}"#,
            r#"{"name":"from","sol_type":"address","storage":"address","indexed":false}"#,
        );
        let c = verdict(&base(), &new);
        assert_eq!(c.verdict, Verdict::Compatible, "changes: {:?}", c.changes);
        assert!(c.changes.is_empty());
    }

    #[test]
    fn mixed_additive_and_breaking_is_breaking() {
        // Add a column (additive) AND remove a table (breaking) → the whole update is breaking.
        let new = r#"{"tables":[
            {"table":"c0__transfer","columns":[
              {"name":"from","sol_type":"address","storage":"address"},
              {"name":"to","sol_type":"address","storage":"address"},
              {"name":"value","sol_type":"uint256","storage":"word32"},
              {"name":"note","sol_type":"string","storage":"string"}
            ]}
          ]}"#;
        let c = verdict(&base(), new);
        assert_eq!(c.verdict, Verdict::Breaking);
        assert_eq!(c.additive_changes().count(), 1); // the added `note`
        assert!(c.breaking_changes().count() >= 1); // the removed approval table
    }

    #[test]
    fn malformed_json_errors_rather_than_misclassifies() {
        assert!(classify_schemas("not json", &base()).is_err());
        assert!(classify_schemas(&base(), "{").is_err());
    }
}
