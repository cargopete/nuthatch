//! Errors as prompts (RFC-0016 §3). A failed agent query is a teaching opportunity: instead of
//! relaying a raw `Binder Error: …` and costing a round-trip to rediscover what `schema` already
//! knows, we classify the failure against the registry and append a one-line, actionable hint so the
//! agent self-corrects in one shot. The raw engine message is always preserved (we never lie about
//! what the engine said); the hint is appended.
//!
//! This is pure text-in / text-out over the schema — nothing here touches the data path. It enriches
//! the `/sql` (and thus MCP `sql`) error surface and the `nuthatch sql` REPL alike.

use crate::registry::TableSchema;
use crate::semantic::derive_footguns;

/// Classify a DuckDB error for `query` against the nest `schema`, returning an actionable hint if the
/// failure matches a known class (`None` otherwise — an unrecognised error is relayed raw, unadorned).
/// The classes mirror the RFC-0016 §4 table; each is matched off DuckDB's real message text.
pub fn enrich(raw: &str, query: &str, schema: &[TableSchema]) -> Option<String> {
    // Unknown table: `Catalog Error: Table with name <X> does not exist!`
    if let Some(name) = between(raw, "Table with name ", " does not exist") {
        let name = name.trim();
        let tables: Vec<&str> = schema.iter().map(|t| t.table.as_str()).collect();
        return Some(match closest(name, &tables) {
            Some(c) => format!(
                "no table `{name}`; the closest is `{c}`. Call the `schema` tool for the full list."
            ),
            None => format!("no table `{name}`. Call the `schema` tool for the list of tables."),
        });
    }

    // Unknown column: `Binder Error: Referenced column "<X>" not found in FROM clause!`
    if let Some(col) = quoted_after(raw, "Referenced column ") {
        // A big-int helper the agent guessed wrong: they wrote `foo` but meant the `foo_dec` companion,
        // or vice-versa. Suggest the sibling if it exists before a generic fuzzy match.
        let all_cols: Vec<String> = schema
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| c.name.clone()))
            .collect();
        let big_ints: Vec<String> = schema
            .iter()
            .flat_map(|t| derive_footguns(t).big_ints)
            .collect();
        if big_ints.iter().any(|b| format!("{b}_dec") == col)
            && !all_cols.contains(&col.to_string())
        {
            return Some(format!(
                "`{col}` is derived on the fly — it isn't a stored column, but you *can* select it. If \
                 the binder rejected it, the base column exists as `{}` (exact text).",
                col.trim_end_matches("_dec")
            ));
        }
        let refs: Vec<&str> = all_cols.iter().map(String::as_str).collect();
        return Some(match closest(&col, &refs) {
            Some(c) => format!(
                "no column `{col}`; the closest is `{c}`. Call `schema` for this table's columns."
            ),
            None => format!("no column `{col}`. Call `schema` for the columns."),
        });
    }

    // Reserved word: `Parser Error: syntax error at or near …` when a reserved-word column appears
    // unquoted in the query. DuckDB reports the *next* token, not the column, so we detect via the
    // schema: a reserved-word column mentioned bare is the culprit.
    if raw.contains("syntax error") {
        for t in schema {
            for rc in derive_footguns(t).reserved_words {
                if mentions_unquoted(query, &rc) {
                    return Some(format!(
                        "`{rc}` is a SQL reserved word and a column of `{}` — double-quote it: SELECT \"{rc}\" …",
                        t.table
                    ));
                }
            }
        }
    }

    // Big-int arithmetic on the raw text column: `Binder Error: No function matches … 'sum(VARCHAR)'`.
    if raw.contains("No function matches") && raw.contains("VARCHAR") {
        for t in schema {
            for bc in derive_footguns(t).big_ints {
                if mentions_in_aggregate(query, &bc) {
                    return Some(format!(
                        "`{bc}` is an exact-text big integer (uint/int > 64-bit); use `{bc}_dec` for \
                         SUM/AVG/comparisons, not the raw column."
                    ));
                }
            }
        }
    }

    None
}

/// The substring strictly between the first occurrence of `a` and the next occurrence of `b` after it.
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let start = s.find(a)? + a.len();
    let rest = &s[start..];
    let end = rest.find(b)?;
    Some(&rest[..end])
}

/// The text inside the first pair of double-quotes that appears after `marker`.
fn quoted_after(s: &str, marker: &str) -> Option<String> {
    let after = &s[s.find(marker)? + marker.len()..];
    let open = after.find('"')? + 1;
    let rest = &after[open..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// Does `col` appear in `query` as a bare (un-double-quoted) identifier? If it were quoted the query
/// would have parsed, so a reserved-word column that shows up in the text unquoted is the culprit.
fn mentions_unquoted(query: &str, col: &str) -> bool {
    let q = query.to_ascii_lowercase();
    let c = col.to_ascii_lowercase();
    contains_word(&q, &c) && !q.contains(&format!("\"{c}\""))
}

/// Does `col` (raw) appear inside an aggregate call in `query`? Matches `sum(col)`, `avg(col,` etc.
/// after stripping whitespace/quotes, with a closing `)`/`,` so `col` isn't a prefix of `col_dec`.
fn mentions_in_aggregate(query: &str, col: &str) -> bool {
    let stripped: String = query
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect::<String>()
        .to_ascii_lowercase();
    let c = col.to_ascii_lowercase();
    for op in ["sum(", "avg(", "min(", "max(", "total("] {
        for close in [")", ","] {
            if stripped.contains(&format!("{op}{c}{close}")) {
                return true;
            }
        }
    }
    false
}

/// Whole-word containment: `col` bounded by non-alphanumeric/underscore (so `to` doesn't match
/// `token`). Cheap and dependency-free.
fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(word) {
        let i = from + pos;
        let before_ok = i == 0 || !is_ident(bytes[i - 1]);
        let after = i + word.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        from = i + word.len();
    }
    false
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The closest real candidate to `name`. Tries, in order: an exact (case-insensitive) hit; then
/// **containment** — the most common agent slip is dropping the `{alias}__` prefix (`transfers` for
/// `usdc__transfer`) or pluralising, so a candidate that contains the de-pluralised guess wins; then
/// **Levenshtein** within a sane budget (≤ 3, or half the name) for genuine typos (`valu` → `value`).
/// `None` if nothing is close enough — suggestions therefore always come from the real schema, never
/// hallucinated.
fn closest<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let n = name.to_ascii_lowercase();
    let n_sing = n.strip_suffix('s').unwrap_or(&n);

    if let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(name)) {
        return Some(c);
    }
    // Containment on the de-pluralised guess (`transfer` ⊆ `usdc__transfer`). Prefer the shortest
    // matching candidate — the most specific.
    if let Some(c) = candidates
        .iter()
        .filter(|c| {
            let cl = c.to_ascii_lowercase();
            (n.len() >= 3 && cl.contains(&n)) || (n_sing.len() >= 3 && cl.contains(n_sing))
        })
        .min_by_key(|c| c.len())
    {
        return Some(c);
    }
    // Genuine typo: nearest by edit distance, within budget.
    let budget = (name.len() / 2).max(3);
    candidates
        .iter()
        .map(|c| (*c, levenshtein(&n, &c.to_ascii_lowercase())))
        .filter(|(_, d)| *d <= budget)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Classic Levenshtein edit distance (two-row DP). Small strings, so O(n·m) is fine.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ColumnSchema, TableSchema};

    fn schema() -> Vec<TableSchema> {
        vec![TableSchema {
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
        }]
    }

    #[test]
    fn unknown_table_suggests_the_closest_real_table() {
        let raw = "Catalog Error: Table with name transfers does not exist!";
        let hint = enrich(raw, "SELECT count(*) FROM transfers", &schema()).unwrap();
        assert!(hint.contains("no table `transfers`"));
        assert!(hint.contains("usdc__transfer"), "suggests the real table");
    }

    #[test]
    fn unknown_column_suggests_the_closest_real_column() {
        let raw = r#"Binder Error: Referenced column "valu" not found in FROM clause!"#;
        let hint = enrich(raw, "SELECT valu FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("no column `valu`"));
        assert!(hint.contains("value"), "suggests value");
    }

    #[test]
    fn reserved_word_column_is_told_to_double_quote() {
        let raw = r#"Parser Error: syntax error at or near "FROM""#;
        let hint = enrich(raw, "SELECT from FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("reserved word"));
        assert!(hint.contains("\"from\""), "shows the quoted form");
    }

    #[test]
    fn bigint_aggregate_is_pointed_at_the_dec_companion() {
        let raw =
            "Binder Error: No function matches the given name and argument types 'sum(VARCHAR)'.";
        let hint = enrich(raw, "SELECT sum(value) FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("value_dec"), "points at value_dec");
    }

    #[test]
    fn a_quoted_reserved_word_query_is_not_flagged() {
        // If the agent already quoted "from", a *different* syntax error must not be misread as the
        // reserved-word case.
        let raw = r#"Parser Error: syntax error at or near ")""#;
        let hint = enrich(
            raw,
            r#"SELECT "from" FROM usdc__transfer WHERE )"#,
            &schema(),
        );
        assert!(hint.is_none(), "quoted from is not the culprit");
    }

    #[test]
    fn an_unrecognised_error_gets_no_hint() {
        assert!(enrich("Some internal error", "SELECT 1", &schema()).is_none());
    }

    #[test]
    fn suggestions_only_ever_come_from_the_schema() {
        // A wild table name gets no suggestion rather than a hallucinated one.
        let raw = "Catalog Error: Table with name zzzzzzzzzz does not exist!";
        let hint = enrich(raw, "SELECT * FROM zzzzzzzzzz", &schema()).unwrap();
        assert!(hint.contains("no table"));
        assert!(!hint.contains("usdc__transfer"), "too far to suggest");
    }
}
