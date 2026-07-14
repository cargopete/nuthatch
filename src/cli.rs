//! Command-line surface. The whole product is meant to be two commands: `init` and `dev`.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "nuthatch", version, about = "Be your own indexer — one binary, one command.")]
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
    /// Contract address to index, e.g. 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 (USDC).
    pub address: String,

    /// Chain to index. Currently: mainnet.
    #[arg(long, default_value = "mainnet")]
    pub chain: String,

    /// Directory to scaffold into (defaults to the current directory).
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
