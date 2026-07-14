//! `nuthatch init` — the first 30 seconds a user judges us on. Resolve the ABI, write a project.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

use crate::abi;
use crate::chains;
use crate::cli::InitArgs;
use crate::config::{Config, ABI_FILE};

pub async fn init(args: InitArgs) -> Result<()> {
    let address = normalise_address(&args.address)?;
    let chain = chains::lookup(&args.chain)
        .with_context(|| format!("unknown chain '{}' (try: mainnet)", args.chain))?;
    let dir = PathBuf::from(&args.dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;

    println!("→ resolving ABI for {address} on {}…", chain.name);
    let abi = abi::resolve(chain.chain_id, &address).await?;

    if !abi::has_transfer_event(&abi) {
        // Not necessarily a problem: the skeleton decodes Transfer by topic0, not from the ABI,
        // so proxies (whose resolved ABI is the proxy's, not the token's) still index fine.
        println!(
            "· note: this ABI declares no `Transfer` event (often a proxy). The skeleton decodes \
             Transfer by topic0 regardless, so `nuthatch dev` will still index transfers at this address."
        );
    }

    std::fs::write(
        dir.join(ABI_FILE),
        serde_json::to_string_pretty(&abi).context("failed to serialise ABI")?,
    )
    .context("failed to write abi.json")?;

    let config = Config {
        chain: chain.name.to_string(),
        chain_id: chain.chain_id,
        address: address.clone(),
        rpc_urls: chain.rpc_urls.iter().map(|s| s.to_string()).collect(),
        event: "Transfer".to_string(),
    };
    config.save(&dir)?;

    println!("✓ scaffolded nuthatch project in {}", dir.display());
    println!("    nuthatch.toml   config");
    println!("    abi.json        resolved ABI");
    println!();
    println!("next:  nuthatch dev{}", dir_hint(&args.dir));
    Ok(())
}

fn dir_hint(dir: &str) -> String {
    if dir == "." {
        String::new()
    } else {
        format!(" --dir {dir}")
    }
}

/// Minimal sanity check + lowercasing. Full checksum validation is a later concern.
fn normalise_address(addr: &str) -> Result<String> {
    let a = addr.trim();
    let hex = a.strip_prefix("0x").unwrap_or(a);
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{addr}' is not a 20-byte hex address");
    }
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}
