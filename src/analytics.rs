//! Read-only analytical SQL over the sealed Parquet segments, via an embedded DuckDB. DuckDB is
//! single-writer/OLAP: we only ever ATTACH the segments read-only here; the ingestion path never
//! writes DuckDB. Queries see *finalized* data (what's been sealed); the hot tip lives in redb.
//!
//! The binary stays single-file: DuckDB is statically bundled. Memory is capped so an analytical
//! query can't blow the embedded-mode RAM budget.

use anyhow::{bail, Context, Result};
use duckdb::types::ValueRef;
use duckdb::Connection;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Cap DuckDB's working memory so `/sql` can't breach the embedded footprint budget.
const MEM_LIMIT: &str = "512MB";
const MAX_THREADS: u32 = 2;

/// A resource guard for the untrusted `/sql` surface: a hard wall-clock deadline (enforced by
/// interrupting the running DuckDB query) and a cap on materialised rows. Trusted internal callers
/// (`net_balances`, `get_row`) run *unguarded* — their SQL is registry-built, never user text, and
/// they must run to completion. Access control (who may query, per-caller quotas) is deliberately
/// *not* here: that needs caller identity a sovereign single-tenant node doesn't have — it's a
/// gateway's job. This guard is only about the node protecting itself from any single query.
#[derive(Clone, Copy)]
pub struct QueryGuard {
    pub timeout: Duration,
    pub max_rows: usize,
}

/// The result of a query: the rows, plus whether a guard's row cap truncated them.
#[derive(Debug)]
pub struct QueryOutput {
    pub rows: Vec<Value>,
    pub truncated: bool,
}

/// Run a read-only query to completion. Only SELECT/WITH statements are accepted — this is a query
/// surface, not a mutation surface. Unguarded: for trusted, registry-built SQL that must finish.
pub fn query(dir: &Path, sql: &str) -> Result<Vec<Value>> {
    Ok(run(dir, sql, None)?.rows)
}

/// Run a read-only query under a resource guard — the entry point for the public `/sql` surface. A
/// query that outlives `guard.timeout` is interrupted and surfaced as a timeout; a result larger
/// than `guard.max_rows` is truncated and flagged. See [`QueryGuard`].
pub fn query_guarded(dir: &Path, sql: &str, guard: QueryGuard) -> Result<QueryOutput> {
    run(dir, sql, Some(guard))
}

fn run(dir: &Path, sql: &str, guard: Option<QueryGuard>) -> Result<QueryOutput> {
    // Check the first *statement keyword*, past any leading whitespace and SQL comments — a query
    // that opens with `-- note` or `/* … */` is still a SELECT. DuckDB gets the original text.
    let head = strip_leading_sql_comments(sql).to_ascii_lowercase();
    if !(head.starts_with("select") || head.starts_with("with")) {
        bail!("only SELECT/WITH queries are allowed on the read-only SQL surface");
    }

    let conn = Connection::open_in_memory().context("failed to open DuckDB")?;
    conn.execute_batch(&format!(
        "SET memory_limit='{MEM_LIMIT}'; SET threads={MAX_THREADS};"
    ))
    .context("failed to configure DuckDB")?;
    define_views(&conn, dir)?;
    // A nest can ship derived-entity views (`views/*.sql`) that build on the per-event tables; the
    // analytical `/sql` surface sees them. Point-reads (`net_balances`, `get_row`) deliberately skip
    // this — they only touch the raw per-event tables.
    define_nest_views(&conn, dir);

    // Hard wall-clock deadline for the untrusted surface: a watchdog thread interrupts the in-flight
    // query once it outlives the guard's timeout (a cartesian blow-up can't be stopped by the memory
    // cap alone). `interrupt()` makes the running query fail; we translate that into a clear timeout
    // error below. On normal completion we signal the watchdog so it never fires. Unguarded (trusted)
    // queries skip all of this and run to completion.
    let interrupted = Arc::new(AtomicBool::new(false));
    let watchdog = guard.map(|g| {
        let handle = conn.interrupt_handle();
        let flag = interrupted.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            // Only a genuine timeout interrupts; a value (normal completion) or a dropped sender
            // (panic) leaves the query alone.
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(g.timeout) {
                flag.store(true, Ordering::SeqCst);
                handle.interrupt();
            }
        });
        (tx, join)
    });

    let cap = guard.map(|g| g.max_rows);
    let outcome = collect(&conn, sql, cap);

    // Stop the watchdog before interpreting the result: a value arriving before the deadline makes
    // `recv_timeout` return `Ok`, so it won't interrupt; then join so it can't fire late.
    if let Some((tx, join)) = watchdog {
        let _ = tx.send(());
        let _ = join.join();
    }

    let (mut rows, over_cap) = match outcome {
        Ok(v) => v,
        Err(e) => {
            if interrupted.load(Ordering::SeqCst) {
                let secs = guard.map(|g| g.timeout.as_secs()).unwrap_or(0);
                bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
            }
            return Err(e);
        }
    };

    let truncated = match cap {
        Some(max) if over_cap => {
            rows.truncate(max);
            true
        }
        _ => false,
    };
    Ok(QueryOutput { rows, truncated })
}

/// Prepare, execute and materialise the result. With `cap = Some(n)` it stops after `n + 1` rows so
/// the caller can report truncation precisely (the returned bool is true when that extra row existed,
/// i.e. more than `n` rows were available); the caller then truncates back to `n`. `cap = None`
/// materialises every row. Row materialisation is Rust-side and escapes DuckDB's own memory limit,
/// so the cap is what actually bounds a `SELECT *` result buffer.
fn collect(conn: &Connection, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool)> {
    let mut stmt = conn.prepare(sql).context("failed to prepare query")?;
    let mut rows = stmt.query([]).context("query failed")?;
    // Column metadata is only materialised once the statement has executed — read it off the
    // executed result, not the prepared statement.
    let column_names: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names().iter().map(|c| c.to_string()).collect())
        .unwrap_or_default();

    let hard = cap.map(|c| c + 1);
    let mut out = Vec::new();
    while let Some(row) = rows.next().context("row read failed")? {
        let mut obj = Map::new();
        for (i, name) in column_names.iter().enumerate() {
            obj.insert(name.clone(), value_to_json(row.get_ref(i)?));
        }
        out.push(Value::Object(obj));
        if hard.is_some_and(|h| out.len() >= h) {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// Skip leading whitespace and SQL comments (`-- line` and `/* block */`) so the read-only guard
/// sees the first real keyword. Returns the remainder starting at that keyword.
fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = match rest.find('\n') {
                Some(i) => rest[i + 1..].trim_start(),
                None => "",
            };
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = match rest.find("*/") {
                Some(i) => rest[i + 2..].trim_start(),
                None => "",
            };
        } else {
            return s;
        }
    }
}

/// Net balance per address for one sealed transfer table, summed as i128 (DuckDB HUGEINT). This is
/// how the IVM view is re-seeded on restart: instead of replaying every sealed transfer through the
/// circuit, we let DuckDB fold each immutable segment down to one (address, net) row. Addresses
/// whose net is exactly zero are omitted (matching the view's drop-at-zero behaviour). `table` and
/// the column names come from the registry (`{alias}__transfer`; from/to/value column names vary by
/// token — USDC from/to/value, WETH src/dst/wad), never user text, so there is no injection surface.
pub fn net_balances(
    dir: &Path,
    table: &str,
    from_col: &str,
    to_col: &str,
    value_col: &str,
) -> Result<Vec<(String, i128)>> {
    // `to` receives (+value), `from` sends (−value); TRY_CAST yields NULL (skipped) for the rare
    // value that overflows i128, mirroring the caller's i128 parse-or-skip.
    let sql = format!(
        "SELECT addr, SUM(d)::VARCHAR AS net FROM (\
           SELECT \"{to_col}\" AS addr, TRY_CAST(\"{value_col}\" AS HUGEINT) AS d FROM \"{table}\" \
           UNION ALL \
           SELECT \"{from_col}\" AS addr, -TRY_CAST(\"{value_col}\" AS HUGEINT) AS d FROM \"{table}\"\
         ) GROUP BY addr HAVING SUM(d) <> 0"
    );
    let mut out = Vec::new();
    for r in query(dir, &sql)? {
        if let (Some(addr), Some(net)) = (r["addr"].as_str(), r["net"].as_str()) {
            if let Ok(n) = net.parse::<i128>() {
                out.push((addr.to_string(), n));
            }
        }
    }
    Ok(out)
}

/// Point-read fallback: fetch a single sealed transfer by (block, log_index). Used when the hot
/// store has already pruned it. Integers are interpolated (not user text), so no injection surface.
pub fn get_row(dir: &Path, block: u64, log_index: u64) -> Result<Option<Value>> {
    let manifest = crate::seal::load_manifest(dir)?;
    for table in manifest.tables.keys() {
        let sql = format!(
            "SELECT * FROM \"{table}\" WHERE block_number = {block} AND log_index = {log_index} LIMIT 1"
        );
        if let Some(row) = query(dir, &sql)?.into_iter().next() {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// Expose each table's sealed segments as a read-only DuckDB view named after the table. Tables with
/// no sealed segments yet simply have no view (they hold only unsealed tip data, served from hot).
///
/// Big-integer columns (uint/int > 64 bits) are stored as exact text (canonical form). For ergonomic
/// SQL (RFC-0001 §2) each such column `c` gets two derived view columns: `c_dec` — the value as
/// `DECIMAL(38,0)` when it fits, else NULL — and `c_overflow` — true when the exact value exceeds
/// 38 digits (so `c_dec` is NULL but `c` isn't). Analytics can `SUM(c_dec)` without hand-casting.
fn define_views(conn: &Connection, dir: &Path) -> Result<()> {
    let manifest = crate::seal::load_manifest(dir)?;
    let schema = schema_columns(dir);
    let cols_of = |table: &str| -> &[(String, String)] {
        schema
            .iter()
            .find(|(t, _)| t == table)
            .map(|(_, c)| c.as_slice())
            .unwrap_or(&[])
    };
    let seg_dir = dir.join(crate::seal::SEGMENTS_DIR);

    // Real views over the sealed segments.
    for (table, segments) in &manifest.tables {
        if segments.is_empty() {
            continue;
        }
        let files: Vec<String> = segments
            .iter()
            .map(|s| format!("'{}'", seg_dir.join(&s.file).display()))
            .collect();
        let ddl = format!(
            "CREATE VIEW \"{table}\" AS SELECT *{} FROM read_parquet([{}])",
            derived_bigint_cols(cols_of(table)),
            files.join(", ")
        );
        conn.execute_batch(&ddl)
            .with_context(|| format!("failed to define view {table}"))?;
    }

    // Empty typed views for every *declared* table that hasn't sealed yet, so nest views over sparse
    // data resolve to zero rows instead of cascade-failing (a table with no events is common early on
    // and on a low-traffic contract). Best-effort — a bad schema entry just leaves that view absent.
    let sealed: std::collections::HashSet<&str> = manifest
        .tables
        .iter()
        .filter(|(_, s)| !s.is_empty())
        .map(|(t, _)| t.as_str())
        .collect();
    for (table, cols) in &schema {
        if sealed.contains(table.as_str()) || cols.is_empty() {
            continue;
        }
        if let Err(e) = conn.execute_batch(&empty_view_ddl(table, cols)) {
            tracing::debug!("empty view {table} skipped: {e}");
        }
    }
    Ok(())
}

/// Load a nest's derived-entity views from `{dir}/views/*.sql` into the connection, in sorted
/// filename order (so `10-foo.sql` can build on nothing and `20-bar.sql` can build on foo). Run
/// after the per-event table views (§4 of RFC-0002), so views may reference `{alias}__{event}`
/// tables. Best-effort: a view over a table with no sealed segment yet — or a bad statement — is
/// skipped with a debug log rather than failing the whole query. Nest SQL is authored by the nest
/// you chose to consume; it runs read-only in this ephemeral in-memory DuckDB, same trust as `/sql`.
fn define_nest_views(conn: &Connection, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir.join("views")) else {
        return; // no views/ dir — nothing to load
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    for f in files {
        let Ok(sql) = std::fs::read_to_string(&f) else {
            continue;
        };
        if let Err(e) = conn.execute_batch(&sql) {
            tracing::debug!("nest view {} skipped: {e}", f.display());
        }
    }
}

/// (table, [(column, storage)]) for every declared table, from the nest's `schema.json`. Empty if
/// the file is absent/unparseable. Drives both the derived `*_dec` columns and the empty typed views.
fn schema_columns(dir: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    let Ok(raw) = std::fs::read_to_string(dir.join("schema.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    for t in v
        .get("tables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = t.get("table").and_then(Value::as_str) else {
            continue;
        };
        let cols: Vec<(String, String)> = t
            .get("columns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some((
                    c.get("name")?.as_str()?.to_string(),
                    c.get("storage")?.as_str()?.to_string(),
                ))
            })
            .collect();
        out.push((name.to_string(), cols));
    }
    out
}

/// True for a big-integer (uint/int > 64-bit) storage kind — the columns that get `*_dec`/`*_overflow`.
fn is_bigint(storage: &str) -> bool {
    storage == "word16" || storage == "word32"
}

/// The extra `SELECT` items projecting the derived `{c}_dec` / `{c}_overflow` columns for a table's
/// big-integer columns (empty string if none), shared by the sealed and empty view builders.
fn derived_bigint_cols(cols: &[(String, String)]) -> String {
    let mut s = String::new();
    for (c, _) in cols.iter().filter(|(_, s)| is_bigint(s)) {
        s.push_str(&format!(
            ", TRY_CAST(\"{c}\" AS DECIMAL(38,0)) AS \"{c}_dec\", \
               (\"{c}\" IS NOT NULL AND TRY_CAST(\"{c}\" AS DECIMAL(38,0)) IS NULL) AS \"{c}_overflow\""
        ));
    }
    s
}

/// An empty but correctly-typed view for a declared table that has no sealed segment yet, so a nest
/// view that references it (or UNIONs it with a table that *does* have data) resolves instead of
/// silently vanishing. Columns and their `*_dec`/`*_overflow` siblings match the sealed view's shape;
/// `WHERE false` yields zero rows.
fn empty_view_ddl(table: &str, cols: &[(String, String)]) -> String {
    let mut sel: Vec<String> = Vec::new();
    for (name, storage) in cols {
        // u64 implicit columns are UBIGINT in parquet; everything else is stored as text (VARCHAR).
        let ty = if storage == "u64" {
            "UBIGINT"
        } else {
            "VARCHAR"
        };
        sel.push(format!("CAST(NULL AS {ty}) AS \"{name}\""));
        if is_bigint(storage) {
            sel.push(format!("CAST(NULL AS DECIMAL(38,0)) AS \"{name}_dec\""));
            sel.push(format!("CAST(NULL AS BOOLEAN) AS \"{name}_overflow\""));
        }
    }
    format!(
        "CREATE VIEW \"{table}\" AS SELECT {} WHERE false",
        sel.join(", ")
    )
}

fn value_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::from(i),
        ValueRef::SmallInt(i) => Value::from(i),
        ValueRef::Int(i) => Value::from(i),
        ValueRef::BigInt(i) => Value::from(i),
        ValueRef::UTinyInt(i) => Value::from(i),
        ValueRef::USmallInt(i) => Value::from(i),
        ValueRef::UInt(i) => Value::from(i),
        ValueRef::UBigInt(i) => Value::from(i),
        ValueRef::Float(f) => Value::from(f),
        ValueRef::Double(f) => Value::from(f),
        ValueRef::HugeInt(i) => Value::String(i.to_string()),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        // Timestamps, decimals, nested types etc. — stringify for the skeleton surface.
        other => Value::String(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_select() {
        let dir = tempfile::tempdir().unwrap();
        assert!(query(dir.path(), "DROP TABLE x").is_err());
    }

    /// The `/sql` row cap bounds the Rust-side result buffer and flags truncation precisely.
    #[test]
    fn guarded_query_caps_rows_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"t__transfer","from":"0xa","to":"0xb","value":"1","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"t__transfer","from":"0xa","to":"0xc","value":"2","block_number":1,"tx_hash":"0xt","log_index":1}"#.to_string(),
            r#"{"table":"t__transfer","from":"0xa","to":"0xd","value":"3","block_number":1,"tx_hash":"0xt","log_index":2}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        // Cap below the row count: truncated to max_rows and flagged.
        let guard = QueryGuard {
            timeout: Duration::from_secs(30),
            max_rows: 2,
        };
        let out = query_guarded(dir.path(), r#"SELECT * FROM "t__transfer""#, guard).unwrap();
        assert_eq!(out.rows.len(), 2, "capped at max_rows");
        assert!(out.truncated, "flagged when more rows existed");

        // Cap at the exact row count: everything returned, not flagged (the +1 sentinel finds no more).
        let guard = QueryGuard {
            timeout: Duration::from_secs(30),
            max_rows: 3,
        };
        let out = query_guarded(dir.path(), r#"SELECT * FROM "t__transfer""#, guard).unwrap();
        assert_eq!(out.rows.len(), 3);
        assert!(!out.truncated);
    }

    /// A runaway query is interrupted by the watchdog and surfaced as a timeout, not left to hang.
    #[test]
    fn guarded_query_times_out_on_a_runaway() {
        let dir = tempfile::tempdir().unwrap();
        // A recursive CTE that would iterate ~a billion times: it cannot finish inside the budget, so
        // the watchdog interrupts it. Needs no sealed data — it never touches a table.
        let runaway = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 1000000000) SELECT count(*) FROM t";
        let guard = QueryGuard {
            timeout: Duration::from_millis(250),
            max_rows: 1000,
        };
        let err = query_guarded(dir.path(), runaway, guard).unwrap_err();
        assert!(
            format!("{err:#}").contains("time budget"),
            "expected a timeout error, got: {err:#}"
        );
    }

    #[test]
    fn queries_a_sealed_per_table_segment() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":10,"tx_hash":"0xt","log_index":1}"#.to_string(),
            r#"{"table":"usdc__approval","owner":"0xa","spender":"0xd","value":"9","block_number":10,"tx_hash":"0xt","log_index":2}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();

        // Each table is its own view.
        let t = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__transfer""#).unwrap();
        assert_eq!(t[0]["n"], Value::from(2u64));
        let a = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__approval""#).unwrap();
        assert_eq!(a[0]["n"], Value::from(1u64));

        // Point-read searches all tables by (block, log_index).
        let one = get_row(dir.path(), 10, 1).unwrap().unwrap();
        assert_eq!(one["to"], Value::from("0xc"));
        let appr = get_row(dir.path(), 10, 2).unwrap().unwrap();
        assert_eq!(appr["spender"], Value::from("0xd"));
    }

    #[test]
    fn net_balances_sum_per_address_as_i128() {
        let dir = tempfile::tempdir().unwrap();
        // 1e20 base units > i64::MAX (~9.2e18): the value that an i64 accumulator would have dropped.
        let big = "100000000000000000000";
        let entities = vec![
            format!(
                r#"{{"table":"t__transfer","from":"0x0","to":"0xa","value":"{big}","block_number":1,"tx_hash":"0xt","log_index":0}}"#
            ),
            r#"{"table":"t__transfer","from":"0xa","to":"0xb","value":"30","block_number":1,"tx_hash":"0xt","log_index":1}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        let map: std::collections::HashMap<String, i128> =
            net_balances(dir.path(), "t__transfer", "from", "to", "value")
                .unwrap()
                .into_iter()
                .collect();
        let big: i128 = big.parse().unwrap();
        assert_eq!(map["0x0"], -big); // minted out
        assert_eq!(map["0xa"], big - 30); // received big, sent 30
        assert_eq!(map["0xb"], 30);
        assert!(!map.contains_key("nobody"));
    }

    /// RFC-0001 §2: a uint256 column gets a derived `_dec` DECIMAL(38) view column (value when it
    /// fits in 38 digits, else NULL) and an `_overflow` flag — so ad-hoc SQL can aggregate big ints
    /// without hand-casting.
    #[test]
    fn bigint_columns_get_decimal_and_overflow_views() {
        let dir = tempfile::tempdir().unwrap();
        // schema.json marks `value` as a word32 (uint256) column, driving the derived columns.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[{"table":"t__transfer","alias":"t","event":"Transfer","topic0":"0x","columns":[{"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        // One value that fits DECIMAL(38) (37 digits) and one that overflows it (a 39-digit u128).
        let fits = "1000000000000000000000000000000000000"; // 1e36, 37 digits
        let overflows = "340282366920938463463374607431768211455"; // u128::MAX, 39 digits > DECIMAL(38)
        let entities = vec![
            format!(
                r#"{{"table":"t__transfer","from":"0xa","to":"0xb","value":"{fits}","block_number":1,"tx_hash":"0xt","log_index":0}}"#
            ),
            format!(
                r#"{{"table":"t__transfer","from":"0xa","to":"0xb","value":"{overflows}","block_number":1,"tx_hash":"0xt","log_index":1}}"#
            ),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        let rows = query(
            dir.path(),
            r#"SELECT value_dec, value_overflow FROM "t__transfer" ORDER BY log_index"#,
        )
        .unwrap();
        // Row 0 fits: value_dec present (HUGEINT/DECIMAL stringified), not overflow.
        assert_eq!(rows[0]["value_dec"], Value::from(fits));
        assert_eq!(rows[0]["value_overflow"], Value::from(false));
        // Row 1 overflows DECIMAL(38): value_dec NULL, overflow flagged.
        assert_eq!(rows[1]["value_dec"], Value::Null);
        assert_eq!(rows[1]["value_overflow"], Value::from(true));

        // And SUM(value_dec) works over the fitting rows without a manual cast.
        let s = query(
            dir.path(),
            r#"SELECT SUM(value_dec)::VARCHAR AS s FROM "t__transfer""#,
        )
        .unwrap();
        assert_eq!(s[0]["s"], Value::from(fits));
    }

    #[test]
    fn query_guard_sees_past_leading_comments() {
        assert_eq!(
            strip_leading_sql_comments("  \n-- hi\nSELECT 1").trim_start(),
            "SELECT 1"
        );
        assert_eq!(
            strip_leading_sql_comments("/* a */ WITH x AS (SELECT 1) SELECT 1")
                .trim_start()
                .split(' ')
                .next(),
            Some("WITH")
        );
        let dir = tempfile::tempdir().unwrap();
        // A comment-prefixed SELECT must be accepted (not rejected as non-SELECT); a DROP still fails.
        assert!(query(dir.path(), "-- a note\nSELECT 42 AS n").is_ok());
        assert!(query(dir.path(), "/* x */ DROP TABLE t").is_err());
    }

    /// A declared-but-unsealed table still resolves as an empty typed view, so a nest view that
    /// UNIONs it with a table that *does* have data doesn't cascade-fail (RFC-0002 dogfood fix).
    #[test]
    fn unsealed_tables_get_empty_typed_views() {
        let dir = tempfile::tempdir().unwrap();
        // schema declares two transfer-ish tables; only `a__ev` will have sealed data.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[
                {"table":"a__ev","alias":"a","event":"E","topic0":"0x","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"amount","sol_type":"uint256","storage":"word32","indexed":false}]},
                {"table":"b__ev","alias":"b","event":"E","topic0":"0x","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"amount","sol_type":"uint256","storage":"word32","indexed":false}]}
            ]}"#,
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"a__ev","amount":"100","block_number":1,"log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();

        // b__ev has no segment, but a UNION of both (incl. the derived `_dec` column) must still work.
        let rows = query(
            dir.path(),
            r#"SELECT SUM(amount_dec)::VARCHAR AS total FROM (
                 SELECT amount_dec FROM "a__ev" UNION ALL SELECT amount_dec FROM "b__ev")"#,
        )
        .unwrap();
        assert_eq!(rows[0]["total"], Value::from("100"));
    }

    /// RFC-0002 §4: a nest's `views/*.sql` derived views are loaded and queryable via `/sql`, and
    /// can build on both the per-event tables and earlier (sorted) view files.
    #[test]
    fn nest_defined_views_are_loaded_and_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"7","block_number":11,"tx_hash":"0xu","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"3","block_number":12,"tx_hash":"0xv","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 12).unwrap();

        // Two view files: the second builds on the first — proves sorted load order.
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-recipients.sql"),
            r#"CREATE VIEW recipients AS SELECT "to" AS addr, count(*) AS n FROM "usdc__transfer" GROUP BY "to";"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("views/20-top_recipient.sql"),
            "CREATE VIEW top_recipient AS SELECT addr, n FROM recipients ORDER BY n DESC LIMIT 1;",
        )
        .unwrap();

        let rows = query(dir.path(), "SELECT addr, n FROM top_recipient").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["addr"], Value::from("0xb")); // 0xb received 2, 0xc received 1
        assert_eq!(rows[0]["n"], Value::from(2u64));

        // A broken view file doesn't blow up the surface — the good views still resolve.
        std::fs::write(
            dir.path().join("views/30-broken.sql"),
            "CREATE VIEW broken AS SELECT * FROM nonexistent_table;",
        )
        .unwrap();
        let again = query(dir.path(), "SELECT n FROM recipients WHERE addr = '0xb'").unwrap();
        assert_eq!(again[0]["n"], Value::from(2u64));
    }

    /// RFC-0001 acceptance: `/sql` can JOIN across two per-event tables.
    #[test]
    fn sql_joins_across_two_tables() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":11,"tx_hash":"0xu","log_index":0}"#.to_string(),
            r#"{"table":"usdc__approval","owner":"0xa","spender":"0xd","value":"9","block_number":10,"tx_hash":"0xt","log_index":1}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 11).unwrap();

        // Transfers that occurred in a block where an approval also happened (join on block_number).
        let rows = query(
            dir.path(),
            r#"SELECT t.block_number AS b, t."to" AS recip, a.spender AS appr
               FROM "usdc__transfer" t JOIN "usdc__approval" a USING (block_number)"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 1); // only block 10 has both
        assert_eq!(rows[0]["b"], Value::from(10u64));
        assert_eq!(rows[0]["recip"], Value::from("0xb"));
        assert_eq!(rows[0]["appr"], Value::from("0xd"));
    }
}
