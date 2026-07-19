//! nuthatch — be your own indexer.
//!
//! Turn any contract into a local SQL database:
//!   `nuthatch init 0xADDR`                  -> detect the chain, resolve ABI (Sourcify -> Etherscan), scaffold a nest
//!   `nuthatch dev`                          -> backfill + follow the tip -> decode -> serve an API
//!   `nuthatch sql "SELECT …"`               -> query the live tip + sealed history, as a table
//!
//! Generalised event decode over many contracts, content-addressed Parquet sealing past finality with
//! DuckDB analytics (hot ∪ cold SQL), DBSP incremental views, factories, a compliance pack, webhooks,
//! a built-in admin UI, an MCP server, and multi-nest roosts — all from one static binary. This file is
//! just the CLI front door; the engine lives in the library crate.

use nuthatch::{
    analytics, audit, bench, blob, check, cli, config, indexer, labels, lists, mcp, pack, project,
    roost, screen, store, transform,
};

use anyhow::{Context, Result};
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuthatch=info".into()),
        )
        .with_target(false)
        .init();

    match cli::Cli::parse().command {
        cli::Command::Init(args) => project::init(args).await,
        cli::Command::Add(args) => project::add(args).await,
        cli::Command::Dev(args) => indexer::dev(args).await,
        cli::Command::Sql(args) => run_sql(args).await,
        cli::Command::Transform(args) => run_transform(args),
        cli::Command::Mcp(args) => {
            if args.print_config {
                mcp::print_client_config(&args.url);
                Ok(())
            } else {
                mcp::serve(args.url).await
            }
        }
        cli::Command::Check(args) => check::check(args),
        cli::Command::Bench(args) => match args.what {
            cli::BenchWhat::Backfill(a) => bench::backfill(a).await,
        },
        cli::Command::Labels(args) => run_labels(args),
        cli::Command::Lists(args) => run_lists(args).await,
        cli::Command::Screen(args) => screen::backfill(args),
        cli::Command::Pack(args) => pack::run(args, &now_stamp()),
        cli::Command::Audit(args) => audit::run(args),
        cli::Command::Nest(args) => match args.what {
            cli::NestWhat::Pack(a) => blob::pack(
                std::path::Path::new(&a.dir),
                a.out.as_deref().map(std::path::Path::new),
            ),
            cli::NestWhat::Mount(a) => blob::mount(
                std::path::Path::new(&a.blob),
                a.dir.as_deref().map(std::path::Path::new),
                a.expect.as_deref(),
            ),
        },
        cli::Command::SkillRefs => {
            nuthatch::skill::write_refs(std::path::Path::new("."))?;
            println!(
                "✓ regenerated {}/cli-reference.md",
                nuthatch::skill::SKILL_DIR
            );
            Ok(())
        }
        cli::Command::Roost(args) => match args.what {
            cli::RoostWhat::Dev(a) => {
                roost::dev(
                    std::path::PathBuf::from(&a.dir),
                    a.listen,
                    a.rpc,
                    a.backfill,
                    a.seal_direct,
                    a.concurrency,
                    a.window,
                    a.no_admin,
                )
                .await
            }
        },
    }
}

/// A coarse wall-clock stamp for the `created` field of a manifest (provenance, not a correctness path).
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// `nuthatch lists …` — manage sanctions/watch lists as content-addressed snapshots (RFC-0008 C2).
async fn run_lists(args: cli::ListsArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    match args.what {
        cli::ListsWhat::Fetch(a) => {
            let dir = PathBuf::from(&a.dir);
            let (hash, count) = lists::fetch(
                &dir,
                &a.list,
                a.url.as_deref(),
                a.file.as_deref().map(Path::new),
            )
            .await?;
            println!(
                "✓ fetched {count} sanctioned address(es) → lists/{}.json",
                &hash[..16]
            );
            println!(
                "  screen a range with:  nuthatch screen --list {hash} --from <block> --to <block>"
            );
            Ok(())
        }
        cli::ListsWhat::List(a) => {
            let dir = PathBuf::from(&a.dir);
            for (hash, count) in lists::snapshots(&dir) {
                println!("{hash}  {count} address(es)");
            }
            Ok(())
        }
    }
}

/// `nuthatch labels …` — manage the compliance annotation substrate (RFC-0008 C1).
fn run_labels(args: cli::LabelsArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    match args.what {
        cli::LabelsWhat::Import(a) => {
            let dir = PathBuf::from(&a.dir);
            let (hash, count) = labels::import(&dir, Path::new(&a.file))?;
            println!(
                "✓ imported {count} labeled address(es) → labels/{}.json",
                &hash[..16]
            );
            println!("  (content-addressed: re-importing the same set is idempotent)");
            Ok(())
        }
        cli::LabelsWhat::List(a) => {
            let dir = PathBuf::from(&a.dir);
            let set = labels::load(&dir);
            println!(
                "{} labeled address(es) loaded from {}/labels/",
                set.len(),
                a.dir
            );
            Ok(())
        }
    }
}

/// `nuthatch sql [query]` — read-only SQL over the nest's data (live tip ∪ sealed history). With a
/// query, one-shot to a table (`--json` to pipe). Without, an interactive REPL. The terminal-native
/// front door to querying, so a user never needs curl to poke at their own data (RFC-0015).
async fn run_sql(args: cli::SqlArgs) -> Result<()> {
    let backend = SqlBackend::open(&args.dir, &args.url)?;
    match args.query.clone() {
        Some(query) => {
            let (rows, truncated) = backend.query(&query).await?;
            if args.json {
                for row in &rows {
                    println!("{row}");
                }
            } else {
                print_table(&rows);
            }
            if truncated {
                eprintln!("(result truncated at 50000 rows)");
            }
            Ok(())
        }
        None => repl(backend).await,
    }
}

/// Where `nuthatch sql` queries run: the local store (when `dev` is stopped) or the running instance's
/// HTTP API (when `dev` holds the single-writer redb). Opened once, so a REPL reuses one connection.
enum SqlBackend {
    Local {
        dir: std::path::PathBuf,
        store: store::Store,
    },
    Http {
        url: String,
        client: reqwest::Client,
    },
}

impl SqlBackend {
    fn open(dir: &str, url: &str) -> Result<Self> {
        let dir = std::path::PathBuf::from(dir);
        // Prefer local files; redb is single-writer, so if `dev` holds the store the open fails and we
        // fall back to the running instance's API — the same command works either way.
        match store::Store::open(&dir.join(config::DB_FILE)) {
            Ok(store) => Ok(SqlBackend::Local { dir, store }),
            Err(_) => Ok(SqlBackend::Http {
                url: url.trim_end_matches('/').to_string(),
                client: reqwest::Client::new(),
            }),
        }
    }

    fn describe(&self) -> String {
        match self {
            SqlBackend::Local { dir, .. } => format!("local nest at {}", dir.display()),
            SqlBackend::Http { url, .. } => format!("running nuthatch at {url}"),
        }
    }

    async fn query(&self, sql: &str) -> Result<(Vec<serde_json::Value>, bool)> {
        match self {
            SqlBackend::Local { dir, store } => {
                // Live tip ∪ sealed history, disjoint by the sealed watermark (COR-1).
                let hot = store.hot_rows_by_table().unwrap_or_default();
                let sealed_through = store.sealed_through();
                match analytics::query_hot_cold(
                    dir,
                    sql,
                    analytics::QueryGuard {
                        timeout: std::time::Duration::from_secs(30),
                        max_rows: 50_000,
                    },
                    &hot,
                    sealed_through,
                ) {
                    Ok(out) => Ok((out.rows, out.truncated)),
                    Err(e) => {
                        // Errors as prompts (RFC-0016 §3), same as the HTTP path: classify against the
                        // nest's schema and append a fix hint. Schema is loaded only on the error path.
                        let raw = format!("{e:#}");
                        let hint = config::Config::load(dir)
                            .ok()
                            .and_then(|cfg| {
                                nuthatch::registry::DecodeRegistry::from_nest(dir, &cfg).ok()
                            })
                            .and_then(|reg| nuthatch::sql_errors::enrich(&raw, sql, &reg.schema()));
                        match hint {
                            Some(h) => anyhow::bail!("{raw}\n\nhint: {h}"),
                            None => anyhow::bail!("{raw}"),
                        }
                    }
                }
            }
            SqlBackend::Http { url, client } => {
                let resp = client
                    .get(format!("{url}/sql"))
                    .query(&[("q", sql)])
                    .send()
                    .await
                    .with_context(|| format!("querying {url} — is `nuthatch dev` running?"))?;
                let status = resp.status();
                let body: serde_json::Value =
                    resp.json().await.context("reading the API response")?;
                if !status.is_success() {
                    anyhow::bail!(
                        "{}",
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("query failed")
                    );
                }
                let rows = body
                    .get("rows")
                    .and_then(|r| r.as_array())
                    .cloned()
                    .unwrap_or_default();
                let truncated = body
                    .get("truncated")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                Ok((rows, truncated))
            }
        }
    }
}

/// The interactive `nuthatch sql` REPL: readline with history, dot-commands, and a table per query.
async fn repl(backend: SqlBackend) -> Result<()> {
    use rustyline::error::ReadlineError;
    println!("nuthatch sql — querying {}.", backend.describe());
    println!("Type SQL, or .help for commands. .exit (or Ctrl-D) to quit.");
    let mut rl = rustyline::DefaultEditor::new().context("starting the REPL")?;
    loop {
        match rl.readline("nuthatch> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                if line.starts_with('.') {
                    if repl_meta(line, &backend).await {
                        break; // .exit / .quit
                    }
                    continue;
                }
                // A query error is printed, never fatal — the session stays open.
                match backend.query(line).await {
                    Ok((rows, truncated)) => {
                        print_table(&rows);
                        if truncated {
                            eprintln!("(result truncated at 50000 rows)");
                        }
                    }
                    Err(e) => eprintln!("error: {e:#}"),
                }
            }
            Err(ReadlineError::Interrupted) => continue, // Ctrl-C clears the line
            Err(ReadlineError::Eof) => break,            // Ctrl-D exits
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }
    Ok(())
}

/// Handle a REPL dot-command. Returns `true` when the session should exit.
async fn repl_meta(line: &str, backend: &SqlBackend) -> bool {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some(".exit") | Some(".quit") | Some(".q") => return true,
        Some(".help") => {
            println!(".tables            list the queryable tables");
            println!(".schema <table>    show a table's columns");
            println!(".exit / .quit      leave the REPL (or Ctrl-D)");
            println!("anything else is run as SQL (SELECT/WITH only).");
        }
        Some(".tables") => {
            run_meta_query(
                backend,
                "SELECT table_name FROM information_schema.tables \
                 WHERE NOT starts_with(table_name, '__hot_') ORDER BY table_name",
            )
            .await;
        }
        Some(".schema") => match parts.next() {
            Some(t) => {
                let q = format!(
                    "SELECT column_name, data_type FROM information_schema.columns \
                     WHERE table_name = '{}' ORDER BY ordinal_position",
                    t.replace('\'', "''")
                );
                run_meta_query(backend, &q).await;
            }
            None => eprintln!("usage: .schema <table>"),
        },
        _ => eprintln!("unknown command {line:?} — try .help"),
    }
    false
}

async fn run_meta_query(backend: &SqlBackend, sql: &str) {
    match backend.query(sql).await {
        Ok((rows, _)) => print_table(&rows),
        Err(e) => eprintln!("error: {e:#}"),
    }
}

/// Render query rows as a simple aligned ASCII table.
fn print_table(rows: &[serde_json::Value]) {
    use serde_json::Value;
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }
    // Column order: first-seen across rows (a query result's columns are consistent row to row).
    let mut cols: Vec<String> = Vec::new();
    for r in rows {
        if let Some(o) = r.as_object() {
            for k in o.keys() {
                if !cols.iter().any(|c| c == k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    let cell = |v: Option<&Value>| -> String {
        match v {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => String::new(),
            Some(other) => other.to_string(),
        }
    };
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| cols.iter().map(|c| cell(r.get(c))).collect())
        .collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for row in &table {
        for (i, s) in row.iter().enumerate() {
            widths[i] = widths[i].max(s.chars().count());
        }
    }
    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, s)| format!(" {:<w$} ", s, w = widths[i]))
            .collect::<Vec<_>>()
            .join("|")
    };
    println!("{}", line(&cols));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    for row in &table {
        println!("{}", line(row));
    }
    let n = rows.len();
    println!("({n} row{})", if n == 1 { "" } else { "s" });
}

/// `nuthatch transform` — run a WASM transform component over a project's stored transfers.
fn run_transform(args: cli::TransformArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    let dir = PathBuf::from(&args.dir);
    let store = store::Store::open(&dir.join(config::DB_FILE))?;
    let entities = store.recent(args.limit)?;
    println!(
        "→ running {} over {} transfers…",
        args.component,
        entities.len()
    );

    let input = transform::transfers_to_ipc(&entities)?;
    let runtime = transform::TransformRuntime::load(Path::new(&args.component))?;
    let output = runtime.run(&input)?;
    let facts = transform::ipc_to_json(&output)?;

    println!(
        "✓ {} facts out (pure, deterministic, sandboxed)",
        facts.len()
    );
    for f in facts.iter().take(5) {
        println!("    {f}");
    }
    Ok(())
}
