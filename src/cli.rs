//! Command-line surface. The whole product is meant to be two commands: `init` and `dev`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nuthatch",
    version,
    about = "Be your own indexer — one binary, one command."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Scaffold an indexer for a contract: resolve its ABI and write a project here.
    Init(InitArgs),
    /// Run the indexer: poll logs, store entities, and serve the API.
    Dev(DevArgs),
    /// Run a WASM transform component over a project's stored transfers.
    Transform(TransformArgs),
    /// Serve the Model Context Protocol over stdio (bridges to a running `nuthatch dev`).
    Mcp(McpArgs),
    /// Run a nest's invariant/parity checks (`checks/*.sql`) against recorded expected results.
    Check(CheckArgs),
    /// Benchmark the indexing pipeline (measure first, optimise second — RFC-0004).
    Bench(BenchArgs),
}

#[derive(Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    pub what: BenchWhat,
}

#[derive(Subcommand)]
pub enum BenchWhat {
    /// Measure backfill throughput (events/sec, wall-clock, peak RSS) over a pinned block range.
    Backfill(BackfillBenchArgs),
}

#[derive(Args)]
pub struct BackfillBenchArgs {
    /// Nest directory (must contain a `nuthatch.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// First block of the pinned range (inclusive).
    #[arg(long)]
    pub from: u64,

    /// Last block of the pinned range (inclusive).
    #[arg(long)]
    pub to: u64,

    /// How many runs to take (the report is the median). Public RPC is noisy — 3 is sensible.
    #[arg(long, default_value_t = 3)]
    pub runs: usize,

    /// Override the nest's `rpc_urls` (e.g. point at your own archive node for a T2 run).
    #[arg(long)]
    pub rpc: Option<String>,

    /// Write the bench-report JSON here (e.g. `docs/bench/w1.json`). Prints to stdout regardless.
    #[arg(long)]
    pub out: Option<String>,

    /// A label for the workload in the report (e.g. "W1: USDC 100k dense").
    #[arg(long)]
    pub label: Option<String>,

    /// Measure the seal-direct path (decode → Parquet, bypassing the hot store) instead of the
    /// default decode → redb hot-store path. Use to compare the two backfill storage paths.
    #[arg(long)]
    pub seal_direct: bool,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Optional check-name filter (substring). Omit to run every `checks/*.sql`. E.g. `parity`.
    pub name: Option<String>,

    /// Nest directory (must contain a `checks/` folder).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Record current query results as the expected fixtures (`checks/expected/*.json`) instead of
    /// comparing — the authoring mode, run once against known-good sealed data.
    #[arg(long)]
    pub update: bool,
}

#[derive(Args)]
pub struct McpArgs {
    /// Base URL of the running `nuthatch dev` HTTP API to bridge to.
    #[arg(long, default_value = "http://127.0.0.1:8288")]
    pub url: String,
}

#[derive(Args)]
pub struct TransformArgs {
    /// Path to the transform component (.wasm, wasm32-wasip2).
    pub component: String,

    /// Project directory (must contain a nuthatch.redb with indexed transfers).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// How many of the most-recent transfers to feed the transform.
    #[arg(long, default_value_t = 5_000)]
    pub limit: usize,
}

#[derive(Args)]
pub struct InitArgs {
    /// One or more contract addresses to index, e.g. 0xA0b8…eB48 (USDC). Omit when using `--from`.
    #[arg(num_args = 0..)]
    pub addresses: Vec<String>,

    /// Initialise from a published nest instead of addresses: a git URL or a local directory. The
    /// nest is self-contained (ABIs vendored), so nothing is resolved — just cloned/copied + validated.
    #[arg(long, conflicts_with = "addresses")]
    pub from: Option<String>,

    /// Optional aliases, one per address in order (comma-separated). Defaults to c0, c1, ….
    #[arg(long, value_delimiter = ',')]
    pub alias: Vec<String>,

    /// Chain to index. E.g. mainnet, arbitrum-one.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,

    /// Directory to scaffold into (defaults to the current directory; for `--from`, defaults to the
    /// nest's own name).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct DevArgs {
    /// Project directory (must contain a nuthatch.toml).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// How many blocks back from the tip to begin the (skeleton) backfill.
    #[arg(long, default_value_t = 5_000)]
    pub backfill: u64,
}
