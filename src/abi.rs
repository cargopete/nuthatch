//! ABI resolution: Sourcify first (open, no key), Etherscan v2 as a keyed fallback.
//! Correctness-critical decoding lives elsewhere; this is just acquisition.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

/// Resolve a contract ABI. Returns the ABI as a JSON array on success.
pub async fn resolve(chain_id: u64, address: &str) -> Result<Value> {
    match sourcify(chain_id, address).await {
        Ok(abi) => {
            tracing::info!("ABI resolved via Sourcify");
            Ok(abi)
        }
        Err(e) => {
            tracing::warn!("Sourcify miss ({e:#}); trying Etherscan");
            etherscan(chain_id, address).await
        }
    }
}

async fn sourcify(chain_id: u64, address: &str) -> Result<Value> {
    // Sourcify server API v2. The legacy /server/files endpoint is retired.
    let url = format!("https://sourcify.dev/server/v2/contract/{chain_id}/{address}?fields=abi");
    let resp = reqwest::get(&url).await.context("Sourcify request failed")?;
    if !resp.status().is_success() {
        bail!("Sourcify returned HTTP {}", resp.status());
    }
    let body: Value = resp.json().await.context("Sourcify response was not JSON")?;
    body.get("abi")
        .filter(|a| a.is_array())
        .cloned()
        .ok_or_else(|| anyhow!("Sourcify had no ABI for this contract"))
}

async fn etherscan(chain_id: u64, address: &str) -> Result<Value> {
    let key = std::env::var("ETHERSCAN_API_KEY").map_err(|_| {
        anyhow!("Sourcify had no verified ABI and ETHERSCAN_API_KEY is not set — \
                 set it, or use a Sourcify-verified contract")
    })?;
    let url = format!(
        "https://api.etherscan.io/v2/api?chainid={chain_id}&module=contract&action=getabi&address={address}&apikey={key}"
    );
    let body: Value = reqwest::get(&url)
        .await
        .context("Etherscan request failed")?
        .json()
        .await
        .context("Etherscan response was not JSON")?;
    if body.get("status").and_then(Value::as_str) != Some("1") {
        let msg = body.get("result").and_then(Value::as_str).unwrap_or("unknown error");
        bail!("Etherscan could not return an ABI: {msg}");
    }
    let result = body
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Etherscan result missing"))?;
    let abi: Value = serde_json::from_str(result).context("Etherscan ABI was not valid JSON")?;
    tracing::info!("ABI resolved via Etherscan");
    Ok(abi)
}

/// Does this ABI declare a `Transfer` event? (The only event the skeleton can decode.)
pub fn has_transfer_event(abi: &Value) -> bool {
    abi.as_array()
        .map(|items| {
            items.iter().any(|it| {
                it.get("type").and_then(Value::as_str) == Some("event")
                    && it.get("name").and_then(Value::as_str) == Some("Transfer")
            })
        })
        .unwrap_or(false)
}
