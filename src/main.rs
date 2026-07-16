//! nuthatch — be your own indexer.
//!
//! This is the *walking skeleton*: the thinnest end-to-end path that actually runs.
//!   `nuthatch init 0xADDR --chain mainnet`  -> resolve ABI (Sourcify -> Etherscan) -> scaffold a project
//!   `nuthatch dev`                          -> poll logs over RPC -> decode -> redb tip store -> serve HTTP
//!
//! Deliberately minimal: one chain, ERC-20 `Transfer` decoding only, RPC polling (no ExEx yet),
//! redb-only storage (no DuckDB/Parquet yet), no IVM, no MCP. Those are the next layers to grow
//! onto this spine — see docs/ROADMAP as it lands. What matters here is that it's *alive*.

use nuthatch::{
    audit, bench, check, cli, config, indexer, labels, lists, mcp, pack, project, screen, store,
    transform,
};

use anyhow::Result;
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
        cli::Command::Dev(args) => indexer::dev(args).await,
        cli::Command::Transform(args) => run_transform(args),
        cli::Command::Mcp(args) => mcp::serve(args.url).await,
        cli::Command::Check(args) => check::check(args),
        cli::Command::Bench(args) => match args.what {
            cli::BenchWhat::Backfill(a) => bench::backfill(a).await,
        },
        cli::Command::Labels(args) => run_labels(args),
        cli::Command::Lists(args) => run_lists(args).await,
        cli::Command::Screen(args) => screen::backfill(args),
        cli::Command::Pack(args) => pack::run(args, &now_stamp()),
        cli::Command::Audit(args) => audit::run(args),
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
