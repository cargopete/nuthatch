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
    /// Add another contract to an existing nest — resolve its ABI and grow the config, no re-init.
    Add(AddArgs),
    /// Run the indexer: poll logs, store entities, and serve the API.
    Dev(DevArgs),
    /// Query a nest's data with SQL — the live tip and sealed history, one surface. Prints a table.
    Sql(SqlArgs),
    /// Run a WASM transform component over a project's stored transfers.
    Transform(TransformArgs),
    /// Serve the Model Context Protocol over stdio (bridges to a running `nuthatch dev`).
    Mcp(McpArgs),
    /// Run a nest's invariant/parity checks (`checks/*.sql`) against recorded expected results.
    Check(CheckArgs),
    /// Regenerate the derived artifacts (`schema.json`, `llms.txt`, `semantic.toml` footguns) from
    /// `nuthatch.toml` — run after hand-editing the config (e.g. adding a factory `[[template]]`), so
    /// the schema and the derived `*_dec` columns match the tables the config now produces.
    Schema(SchemaArgs),
    /// Benchmark the indexing pipeline (measure first, optimise second — RFC-0004).
    Bench(BenchArgs),
    /// Manage labeled address sets — the compliance annotation substrate (RFC-0008 C1).
    Labels(LabelsArgs),
    /// Manage sanctions/watch lists as content-addressed snapshots (RFC-0008 C2).
    Lists(ListsArgs),
    /// Screen sealed transfers against a list snapshot, recording `sanction_hit` annotations
    /// (RFC-0008 C2). Replayable: same list hash + range + component → identical hits.
    Screen(ScreenArgs),
    /// Build, sign, and verify the signed compliance-pack manifest (RFC-0008 C6).
    Pack(PackArgs),
    /// Audit the compliance annotations: `replay` re-proves them, `report` summarises them (C6).
    Audit(AuditArgs),
    /// Package a nest as a content-addressed blob — the deploy unit (RFC-0012).
    Nest(NestArgs),
    /// Run a roost: many nests on one chain behind one API, each under `/<name>/…` (RFC-0012).
    Roost(RoostArgs),
    /// Regenerate the builder skill's machine-generated references from clap metadata (RFC-0017).
    /// Hidden: a dev/authoring tool, not part of the user-facing two-command story.
    #[command(hide = true)]
    SkillRefs,
}

#[derive(Args)]
pub struct RoostArgs {
    #[command(subcommand)]
    pub what: RoostWhat,
}

#[derive(Subcommand)]
pub enum RoostWhat {
    /// Bring up every nest a `roost.toml` mounts and serve them behind one listener: `/nests` roster
    /// plus each nest's full API under its `/<name>/…` prefix. Chain identity is shared; the stores are
    /// per-nest and isolated.
    Dev(RoostDevArgs),
}

#[derive(Args)]
pub struct RoostDevArgs {
    /// Roost directory (must contain a roost.toml and a nests/ dir).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// Override the roost's `rpc_urls` at runtime without editing the config (repeatable).
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Index only this many blocks back from the tip, for every mounted nest (recent-history mode).
    #[arg(long)]
    pub backfill: Option<u64>,

    /// Backfill finalized history straight to Parquet before tip-following, for every nest (RFC-0004).
    #[arg(long)]
    pub seal_direct: bool,

    /// Concurrent window fetches during each nest's seal-direct backfill.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,

    /// Override the `eth_getLogs` block-window (the chain default otherwise) for every nest's backfill.
    #[arg(long)]
    pub window: Option<u64>,

    /// Disable the built-in admin UI (`/<name>/_admin/`) entirely for every nest (RFC-0010 Part A).
    #[arg(long)]
    pub no_admin: bool,
}

#[derive(Args)]
pub struct NestArgs {
    #[command(subcommand)]
    pub what: NestWhat,
}

#[derive(Subcommand)]
pub enum NestWhat {
    /// Bundle a nest into one portable, content-addressed `.bundle` file — its authored inputs (config,
    /// ABIs, views, labels, skills) plus a `manifest.json` pinning the expected decode-registry hash.
    /// Share the `.bundle` anywhere (a URL, a file); anyone can `load` it to run your exact nest,
    /// verified by hash. Prints the bundle's content address.
    Bundle(NestBundleArgs),
    /// Load a bundle: verify a `.bundle` (or a URL to one, or an unpacked bundle dir) and install it as
    /// a runnable nest. Checks the manifest format, every file's hash, and that the decode registry
    /// regenerated from the inputs matches the manifest — so a loaded nest decodes exactly as authored.
    Load(NestLoadArgs),
}

#[derive(Args)]
pub struct NestBundleArgs {
    /// Nest directory to bundle.
    #[arg(default_value = ".")]
    pub dir: String,

    /// Output path for the `.bundle` (default: `<nest-name>-<hash>.bundle` beside the nest). With
    /// `--as-dir`, an unpacked bundle *directory* is written here instead of a single file.
    #[arg(long)]
    pub out: Option<String>,

    /// Write an unpacked bundle directory instead of a single `.bundle` file (handy for inspecting a
    /// bundle's contents).
    #[arg(long)]
    pub as_dir: bool,
}

#[derive(Args)]
pub struct NestLoadArgs {
    /// The bundle to load: a `.bundle` file, an `http(s)://` URL to one, or an unpacked bundle directory.
    pub bundle: String,

    /// Target directory to install the nest into (default: the nest's name).
    #[arg(long)]
    pub dir: Option<String>,

    /// Assert the bundle's content-address hash equals this value before installing.
    #[arg(long)]
    pub expect: Option<String>,
}

#[derive(Args)]
pub struct PackArgs {
    #[command(subcommand)]
    pub what: PackWhat,
}

#[derive(Subcommand)]
pub enum PackWhat {
    /// Generate an ed25519 signing keypair into a local JSON file.
    Keygen(PackKeygenArgs),
    /// Assemble `compliance-pack.toml` from the nest's config + artifact hashes (optionally signed).
    Build(PackBuildArgs),
    /// Verify a pack: signature, artifact hashes, and grant conformance.
    Verify(PackVerifyArgs),
}

#[derive(Args)]
pub struct PackKeygenArgs {
    /// Where to write the keypair JSON.
    #[arg(long, default_value = "nuthatch-key.json")]
    pub out: String,
}

#[derive(Args)]
pub struct PackBuildArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// Sign the manifest with this keypair file (from `pack keygen`). Omit to write it unsigned.
    #[arg(long)]
    pub key: Option<String>,
}

#[derive(Args)]
pub struct PackVerifyArgs {
    /// Nest directory (must contain a `compliance-pack.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub what: AuditWhat,
}

#[derive(Subcommand)]
pub enum AuditWhat {
    /// Re-run screening over the sealed segments and confirm the stored hits reproduce exactly.
    Replay(AuditReplayArgs),
    /// Summarise the hits and flags in a block range (markdown or `--json`).
    Report(AuditReportArgs),
}

#[derive(Args)]
pub struct AuditReplayArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// First block of the range (inclusive).
    #[arg(long)]
    pub from: u64,
    /// Last block of the range (inclusive).
    #[arg(long)]
    pub to: u64,
}

#[derive(Args)]
pub struct AuditReportArgs {
    /// Nest directory.
    #[arg(long, default_value = ".")]
    pub dir: String,
    /// First block of the range (inclusive).
    #[arg(long)]
    pub from: u64,
    /// Last block of the range (inclusive).
    #[arg(long)]
    pub to: u64,
    /// Emit JSON instead of markdown.
    #[arg(long)]
    pub json: bool,
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
    /// Measure the read path: entity point-read latency (p50/p99) and the `/sql` hot∪cold scan cost
    /// (query latency + peak RSS). The regression guard for the perf refactors — run offline against
    /// an already-indexed nest.
    Query(QueryBenchArgs),
}

#[derive(Args)]
pub struct QueryBenchArgs {
    /// Nest directory (must contain an indexed `nuthatch.redb`). Stop `dev` first — the bench opens
    /// the store directly.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// The `/sql` query to time (over hot∪cold). Defaults to `SELECT count(*)` on the largest hot
    /// table — the full-tip-materialising scan whose cost is the #1 RAM risk on deep-finality L2s.
    #[arg(long)]
    pub sql: Option<String>,

    /// Entity point-reads to time (keys sampled evenly across the hot store).
    #[arg(long, default_value_t = 1000)]
    pub reads: usize,

    /// `/sql` query repetitions to time (the report is the p50/p99 across them).
    #[arg(long, default_value_t = 20)]
    pub iters: usize,

    /// Write the bench-report JSON here. Prints to stdout regardless.
    #[arg(long)]
    pub out: Option<String>,

    /// A label for the report (e.g. "R1: horizon tip 12k rows").
    #[arg(long)]
    pub label: Option<String>,
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
pub struct SchemaArgs {
    /// Nest directory (must contain a `nuthatch.toml`).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct McpArgs {
    /// Base URL of the running `nuthatch dev` HTTP API to bridge to.
    #[arg(long, default_value = "http://127.0.0.1:8288")]
    pub url: String,

    /// Print a copy-paste MCP client config (Claude Code `.mcp.json` + the `claude mcp add` one-liner)
    /// and exit, instead of running the stdio server. This is the "wire it up in one step" helper.
    #[arg(long)]
    pub print_config: bool,
}

#[derive(Args)]
pub struct SqlArgs {
    /// The SQL query (SELECT/WITH). Tables are `{alias}__{event}`, e.g. `usdc__transfer`. Omit to open
    /// an interactive REPL (`.tables`, `.schema <t>`, history; `.exit` to quit).
    pub query: Option<String>,

    /// Nest directory (queried directly when no `nuthatch dev` holds the store).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// The running instance's API, used when the local store is locked by `nuthatch dev`.
    #[arg(long, default_value = "http://127.0.0.1:8288")]
    pub url: String,

    /// Emit newline-delimited JSON instead of a table (for piping to jq etc.).
    #[arg(long)]
    pub json: bool,
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

    /// Chain to index, e.g. mainnet, arbitrum-one, base. Omit it and nuthatch probes each known
    /// chain for the contract's bytecode and picks the one it lives on — you rarely need to say.
    #[arg(long)]
    pub chain: Option<String>,

    /// Prefer these RPC URL(s) over the chain defaults (repeatable). They're written first in the
    /// nest's `rpc_urls` and also used for ABI/deploy-block resolution during init, with the
    /// built-in chain endpoints kept as fallback. Point at your own node to dodge public-RPC limits.
    #[arg(long)]
    pub rpc: Vec<String>,

    /// Directory to scaffold into (defaults to the current directory; for `--from`, defaults to the
    /// nest's own name).
    #[arg(long, default_value = ".")]
    pub dir: String,
}

#[derive(Args)]
pub struct AddArgs {
    /// One or more contract addresses to add to the nest, e.g. 0xC02a…6Cc2 (WETH).
    #[arg(num_args = 1..)]
    pub addresses: Vec<String>,

    /// Optional aliases, one per address in order (comma-separated). Defaults to the next free
    /// c<N> slots after the nest's existing contracts.
    #[arg(long, value_delimiter = ',')]
    pub alias: Vec<String>,

    /// The nest directory to grow (must contain a nuthatch.toml). Defaults to the current directory.
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Prefer these RPC URL(s) over the nest's configured endpoints for ABI/deploy-block resolution
    /// (repeatable). Point at your own node to dodge public-RPC limits.
    #[arg(long)]
    pub rpc: Vec<String>,
}

#[derive(Args)]
pub struct DevArgs {
    /// Project directory (must contain a nuthatch.toml).
    #[arg(long, default_value = ".")]
    pub dir: String,

    /// Address to bind the HTTP API to.
    #[arg(long, default_value = "127.0.0.1:8288")]
    pub listen: String,

    /// Override the nest's `rpc_urls` at runtime without editing the config (repeatable). These are
    /// tried first; the nest's configured endpoints remain as fallback. Point at your own node.
    #[arg(long)]
    pub rpc: Vec<String>,

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

    /// Override the `eth_getLogs` block-window (the chain default otherwise). For a *sparse* contract
    /// over a long backfill — few events across many blocks — a large window (e.g. 50000) turns tens
    /// of thousands of near-empty requests into a few, so a from-history backfill finishes in minutes.
    /// Keep it under your provider's max block-range for `getLogs` (many allow 100k+ when the result
    /// set is small); the concurrent backfill fails the range rather than auto-shrinking it.
    #[arg(long)]
    pub window: Option<u64>,

    /// Disable the built-in admin UI (`/_admin/`) entirely — no routes, for hosted deployments that
    /// front their own dashboard (RFC-0010 Part A). Off-localhost the UI requires `NUTHATCH_ADMIN_TOKEN`
    /// to be set AND each request to present it as `?token=…` (or it self-disables with a log line).
    #[arg(long)]
    pub no_admin: bool,
}
