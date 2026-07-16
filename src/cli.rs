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
    /// Manage labeled address sets — the compliance annotation substrate (RFC-0008 C1).
    Labels(LabelsArgs),
    /// Manage sanctions/watch lists as content-addressed snapshots (RFC-0008 C2).
    Lists(ListsArgs),
    /// Screen sealed transfers against a list snapshot, recording `sanction_hit` annotations
    /// (RFC-0008 C2). Replayable: same list hash + range + component → identical hits.
    Screen(ScreenArgs),
}

#[derive(Args)]
pub struct ListsArgs {
    #[command(subcommand)]
    pub what: ListsWhat,
}

#[derive(Subcommand)]
pub enum ListsWhat {
    /// Fetch a sanctions/watch list (host-side, out-of-band) into a content-addressed snapshot.
    Fetch(ListsFetchArgs),
    /// List the fetched list snapshots and how many addresses each carries.
    List(ListsListArgs),
}

#[derive(Args)]
pub struct ListsFetchArgs {
    /// List name. Known: `ofac-sdn`, `eu-consolidated` (have default URLs). Any other name needs
    /// `--url` or `--file`.
    pub list: String,

    /// Read the list from a local file instead of downloading (any text: XML/CSV/JSON/plain — the
    /// fetcher extracts every `0x…40hex` address).
    #[arg(long)]
    pub file: Option<String>,

    /// Override the download URL for this list.
    #[arg(long)]
    pub url: Option<String>,

    /// Nest directory to fetch into (writes `lists/<hash>.json`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct ListsListArgs {
    /// Nest directory to read list snapshots from.
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct ScreenArgs {
    /// The list snapshot hash to screen against (from `nuthatch lists fetch`).
    #[arg(long)]
    pub list: String,

    /// First block of the range to screen (inclusive).
    #[arg(long)]
    pub from: u64,

    /// Last block of the range to screen (inclusive).
    #[arg(long)]
    pub to: u64,

    /// Nest directory (must contain a `nuthatch.toml` and sealed segments over the range).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct LabelsArgs {
    #[command(subcommand)]
    pub what: LabelsWhat,
}

#[derive(Subcommand)]
pub enum LabelsWhat {
    /// Import a labeled address set (CSV `address,label` or JSON) as a content-addressed snapshot.
    Import(LabelsImportArgs),
    /// List the imported label snapshots and how many addresses each carries.
    List(LabelsListArgs),
}

#[derive(Args)]
pub struct LabelsImportArgs {
    /// Path to the label file: CSV (`address,label` per line, optional header) or JSON (an array of
    /// `{address,label}` objects, or an `{address: label}` map).
    pub file: String,

    /// Nest directory to import into (writes `labels/<hash>.json`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct LabelsListArgs {
    /// Nest directory to read labels from.
    #[arg(long, default_value = ".")]
    pub dir: String,
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

    /// Concurrent window fetches (seal-direct only). >1 overlaps RPC round-trip latency; results are
    /// still consumed in block order so segments are identical. Try 8–16 against your own node.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
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

    /// Index only this many blocks back from the tip (recent-history mode). Explicitly overrides a
    /// nest's vendored `start_block`s. Omit to backfill from deployment when the nest declares start
    /// blocks, else from a default recent window.
    #[arg(long)]
    pub backfill: Option<u64>,

    /// Backfill finalized history straight to Parquet (skip the hot store) before tip-following —
    /// much faster for a from-deployment backfill (RFC-0004). The near-tip window still uses the hot
    /// path; the IVM view is rebuilt from the sealed segments.
    #[arg(long)]
    pub seal_direct: bool,

    /// Concurrent window fetches during the seal-direct history backfill (overlaps RPC latency).
    /// Try 8–16 against your own node; keep low on rate-limited public RPC.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}
