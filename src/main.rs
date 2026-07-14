//! nuthatch — be your own indexer.
//!
//! This is the *walking skeleton*: the thinnest end-to-end path that actually runs.
//!   `nuthatch init 0xADDR --chain mainnet`  -> resolve ABI (Sourcify -> Etherscan) -> scaffold a project
//!   `nuthatch dev`                          -> poll logs over RPC -> decode -> redb tip store -> serve HTTP
//!
//! Deliberately minimal: one chain, ERC-20 `Transfer` decoding only, RPC polling (no ExEx yet),
//! redb-only storage (no DuckDB/Parquet yet), no IVM, no MCP. Those are the next layers to grow
//! onto this spine — see docs/ROADMAP as it lands. What matters here is that it's *alive*.

mod abi;
mod analytics;
mod chains;
mod cli;
mod config;
mod decode;
mod indexer;
mod project;
mod rpc;
mod seal;
mod serve;
mod store;
mod transform;
mod views;

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
    }
}

/// `nuthatch transform` — run a WASM transform component over a project's stored transfers.
fn run_transform(args: cli::TransformArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    let dir = PathBuf::from(&args.dir);
    let store = store::Store::open(&dir.join(config::DB_FILE))?;
    let entities = store.recent(args.limit)?;
    println!("→ running {} over {} transfers…", args.component, entities.len());

    let input = transform::transfers_to_ipc(&entities)?;
    let runtime = transform::TransformRuntime::load(Path::new(&args.component))?;
    let output = runtime.run(&input)?;
    let facts = transform::ipc_to_json(&output)?;

    println!("✓ {} facts out (pure, deterministic, sandboxed)", facts.len());
    for f in facts.iter().take(5) {
        println!("    {f}");
    }
    Ok(())
}
