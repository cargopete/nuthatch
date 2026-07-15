//! `nuthatch init` — resolve each contract's ABI and scaffold a nest (RFC-0001: N contracts).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::abi;
use crate::chains;
use crate::cli::InitArgs;
use crate::config::{Config, Contract, Nest};
use crate::rpc::RpcClient;

pub async fn init(args: InitArgs) -> Result<()> {
    let chain = chains::lookup(&args.chain)
        .with_context(|| format!("unknown chain '{}' (try: mainnet)", args.chain))?;
    let dir = PathBuf::from(&args.dir);
    std::fs::create_dir_all(dir.join("abis"))
        .with_context(|| format!("cannot create {}", dir.display()))?;

    let addresses: Vec<String> = args
        .addresses
        .iter()
        .map(|a| normalise_address(a))
        .collect::<Result<_>>()?;
    let aliases = resolve_aliases(&args.alias, addresses.len())?;

    // One RPC client for best-effort deployment-block detection.
    let rpc = RpcClient::new(chain.rpc_urls.iter().map(|s| s.to_string()).collect())?;
    let tip = rpc.block_number().await.ok();

    let mut contracts = Vec::with_capacity(addresses.len());
    for (address, alias) in addresses.iter().zip(&aliases) {
        println!("→ resolving ABI for {alias} ({address}) on {}…", chain.name);
        let abi_json = resolve_abi(&rpc, chain.chain_id, address).await?;
        let abi_path = format!("abis/{alias}.json");
        std::fs::write(
            dir.join(&abi_path),
            serde_json::to_string_pretty(&abi_json).context("failed to serialise ABI")?,
        )
        .with_context(|| format!("failed to write {abi_path}"))?;

        let start_block = match tip {
            Some(tip) => match detect_deploy_block(&rpc, address, tip).await {
                Ok(b) => {
                    println!("  ✓ deployed at block {b}");
                    Some(b)
                }
                Err(e) => {
                    println!("  · deployment block undetected ({e:#}); backfill starts from a tip offset");
                    None
                }
            },
            None => None,
        };

        contracts.push(Contract {
            alias: alias.clone(),
            address: address.clone(),
            start_block,
            abi: abi_path,
        });
    }

    let config = Config {
        nest: Nest {
            name: nest_name(&dir),
            chain: chain.name.to_string(),
            chain_id: chain.chain_id,
            rpc_urls: chain.rpc_urls.iter().map(|s| s.to_string()).collect(),
        },
        contracts,
    };
    config.save(&dir)?;
    scaffold_ai_surface(&dir, chain.name, &config.contracts)?;

    println!(
        "✓ scaffolded nest '{}' ({} contract(s)) in {}",
        config.nest.name,
        config.contracts.len(),
        dir.display()
    );
    println!("    nuthatch.toml              config");
    println!("    abis/                      resolved ABIs");
    println!("    llms.txt                   how an AI agent queries this index");
    println!("    .claude/skills/nuthatch/   Claude Code skill (offline, no phone-home)");
    println!();
    println!("next:  nuthatch dev{}", dir_hint(&args.dir));
    println!("       nuthatch mcp   (expose this index to a coding agent over MCP)");
    Ok(())
}

/// Well-known proxy implementation storage slots, tried in order:
/// - EIP-1967: keccak256("eip1967.proxy.implementation") − 1
/// - legacy OpenZeppelin/zeppelinos: keccak256("org.zeppelinos.proxy.implementation") (e.g. USDC)
const PROXY_IMPL_SLOTS: &[&str] = &[
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
    "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3",
];

/// Resolve the ABI to index events with. For a proxy (e.g. USDC), events emit from the proxy address
/// but use the *implementation's* event definitions, so resolve the implementation's ABI. Falls back
/// to the address's own ABI if it isn't a proxy or the implementation can't resolve.
async fn resolve_abi(rpc: &RpcClient, chain_id: u64, address: &str) -> Result<serde_json::Value> {
    for slot in PROXY_IMPL_SLOTS {
        let Ok(word) = rpc.get_storage_at(address, slot).await else {
            continue;
        };
        let Some(implementation) = impl_from_slot(&word) else {
            continue;
        };
        println!("  · proxy → implementation {implementation}");
        if let Ok(abi) = abi::resolve(chain_id, &implementation).await {
            return Ok(abi);
        }
        println!("  · implementation ABI unresolved; using the proxy's own ABI");
        break;
    }
    abi::resolve(chain_id, address).await
}

/// Extract a non-zero implementation address from a 32-byte storage word.
fn impl_from_slot(slot: &str) -> Option<String> {
    let h = slot.trim_start_matches("0x");
    if h.len() < 40 {
        return None;
    }
    let addr = &h[h.len() - 40..];
    if addr.chars().all(|c| c == '0') {
        return None;
    }
    Some(format!("0x{addr}"))
}

/// Aliases from `--alias` (validated, one per address) or defaults c0, c1, ….
fn resolve_aliases(provided: &[String], n: usize) -> Result<Vec<String>> {
    if provided.is_empty() {
        return Ok((0..n).map(|i| format!("c{i}")).collect());
    }
    if provided.len() != n {
        bail!(
            "{} aliases for {n} address(es) — provide one alias per address or none",
            provided.len()
        );
    }
    for a in provided {
        if !is_valid_alias(a) {
            bail!("alias '{a}' must match [a-z][a-z0-9_]*");
        }
    }
    Ok(provided.to_vec())
}

fn is_valid_alias(a: &str) -> bool {
    let mut chars = a.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn nest_name(dir: &Path) -> String {
    dir.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty() && n != ".")
        .unwrap_or_else(|| "nest".to_string())
}

/// Binary-search the deployment block: smallest block where the contract has code.
/// ~log2(tip) ≈ 25 `eth_getCode` calls. Best-effort — the caller tolerates failure.
async fn detect_deploy_block(rpc: &RpcClient, address: &str, tip: u64) -> Result<u64> {
    if is_empty_code(&rpc.get_code(address, tip).await?) {
        bail!("no code at tip");
    }
    let (mut lo, mut hi) = (0u64, tip);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if is_empty_code(&rpc.get_code(address, mid).await?) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn is_empty_code(code: &str) -> bool {
    code.trim_start_matches("0x").is_empty()
}

fn scaffold_ai_surface(dir: &Path, chain: &str, contracts: &[Contract]) -> Result<()> {
    let list: String = contracts
        .iter()
        .map(|c| format!("- `{}` = {}\n", c.alias, c.address))
        .collect();
    let llms = format!(
        "# nuthatch nest on {chain}\n\
         \n\
         A self-hosted blockchain index. Query it locally; there is no third-party API.\n\
         \n\
         ## Contracts\n{list}\n\
         ## Live HTTP API (run `nuthatch dev`)\n\
         - `GET /`                    index status\n\
         - `GET /entities?limit=N`    recent rows\n\
         - `GET /entity/{{id}}`         one row by id (`{{block:012}}-{{logindex:06}}`)\n\
         - `GET /sql?q=SELECT...`     read-only SQL over sealed (finalized) rows\n\
         - `GET /balances?limit=N`    top holder balances (when an ERC-20 Transfer table is present)\n\
         - `GET /balance/{{address}}`   one address's derived balance\n\
         \n\
         ## MCP (for coding agents)\n\
         Run `nuthatch mcp` (stdio) to expose tools: status, schema, sql, entity, balance,\n\
         top_balances. Fully offline against the local instance; nothing phones home.\n"
    );
    std::fs::write(dir.join("llms.txt"), llms).context("failed to write llms.txt")?;

    let skill_dir = dir.join(".claude/skills/nuthatch");
    std::fs::create_dir_all(&skill_dir).context("failed to create skill dir")?;
    let skill = format!(
        "---\n\
         name: nuthatch\n\
         description: Query this self-hosted nuthatch nest on {chain} — decoded events, balances, \
         and read-only SQL. Use when asked about on-chain activity for these contracts.\n\
         ---\n\
         \n\
         # Querying the nuthatch nest\n\
         \n\
         Contracts indexed on {chain}:\n{list}\n\
         Data is local — never call an external API for it.\n\
         \n\
         ## Preferred: MCP\n\
         If a `nuthatch` MCP server is configured, use its tools. Call `schema` first to learn the\n\
         data model, then `sql` / `entity` / `balance` / `top_balances`.\n\
         \n\
         ## Fallback: HTTP (a `nuthatch dev` must be running)\n\
         - Recent rows:  `curl localhost:8288/entities?limit=20`\n\
         - Read-only SQL: `curl -G localhost:8288/sql --data-urlencode 'q=SELECT count(*) FROM transfers'`\n\
         \n\
         `sql` sees finalized data only; balances/entity cover the live tip.\n"
    );
    std::fs::write(skill_dir.join("SKILL.md"), skill).context("failed to write SKILL.md")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure mirror of the deployment binary search, for algorithm confidence without RPC.
    fn find_deploy_block(tip: u64, deployed_from: u64) -> Option<u64> {
        let is_deployed = |b: u64| b >= deployed_from;
        if !is_deployed(tip) {
            return None;
        }
        let (mut lo, mut hi) = (0u64, tip);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if is_deployed(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Some(lo)
    }

    #[test]
    fn deploy_block_binary_search() {
        assert_eq!(find_deploy_block(1000, 137), Some(137));
        assert_eq!(find_deploy_block(1000, 0), Some(0));
        assert_eq!(find_deploy_block(1000, 1000), Some(1000));
        assert_eq!(find_deploy_block(100, 500), None); // not deployed by tip
    }

    #[test]
    fn aliases_default_and_validate() {
        assert_eq!(resolve_aliases(&[], 2).unwrap(), vec!["c0", "c1"]);
        assert_eq!(
            resolve_aliases(&["usdc".into(), "weth".into()], 2).unwrap(),
            vec!["usdc", "weth"]
        );
        assert!(resolve_aliases(&["usdc".into()], 2).is_err()); // count mismatch
        assert!(resolve_aliases(&["USDC".into()], 1).is_err()); // uppercase invalid
        assert!(resolve_aliases(&["1bad".into()], 1).is_err()); // leading digit invalid
    }

    #[test]
    fn eip1967_impl_extraction() {
        assert!(impl_from_slot(
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        )
        .is_none());
        assert_eq!(
            impl_from_slot("0x00000000000000000000000043506849d7c04f9138d1a2050bbf3a0c054402dd"),
            Some("0x43506849d7c04f9138d1a2050bbf3a0c054402dd".to_string())
        );
    }

    #[test]
    fn address_normalisation() {
        assert_eq!(
            normalise_address("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert!(normalise_address("0x123").is_err());
    }
}
