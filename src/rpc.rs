//! The thinnest JSON-RPC client that works: `eth_blockNumber` + `eth_getLogs`, with round-robin
//! failover across the configured endpoints. No ExEx yet — that's the sovereignty upgrade later.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct RpcClient {
    http: reqwest::Client,
    urls: Vec<String>,
    cursor: AtomicUsize,
}

#[derive(Debug, Clone)]
pub struct Log {
    /// Emitting contract. Unused while we filter by a single address in the query, but retained
    /// for multi-contract / ABI-priority decode in later slices.
    #[allow(dead_code)]
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub block_number: u64,
    pub tx_hash: String,
    pub log_index: u64,
}

impl RpcClient {
    pub fn new(urls: Vec<String>) -> Result<Self> {
        if urls.is_empty() {
            bail!("no RPC URLs configured");
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, urls, cursor: AtomicUsize::new(0) })
    }

    /// Try each endpoint once, starting from the round-robin cursor, until one answers.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let n = self.urls.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut last_err = anyhow!("all RPC endpoints failed");
        for i in 0..n {
            let url = &self.urls[(start + i) % n];
            match self.call_one(url, method, &params).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::debug!("rpc {url} failed for {method}: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn call_one(&self, url: &str, method: &str, params: &Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            bail!("rpc error: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc response had no result"))
    }

    pub async fn block_number(&self) -> Result<u64> {
        let result = self.call("eth_blockNumber", json!([])).await?;
        parse_hex_u64(result.as_str().unwrap_or_default())
    }

    pub async fn get_logs(&self, address: &str, topic0: &str, from: u64, to: u64) -> Result<Vec<Log>> {
        let params = json!([{
            "address": address,
            "topics": [topic0],
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{to:x}"),
        }]);
        let result = self.call("eth_getLogs", params).await?;
        let arr = result.as_array().ok_or_else(|| anyhow!("eth_getLogs did not return an array"))?;
        arr.iter().map(parse_log).collect()
    }
}

fn parse_log(v: &Value) -> Result<Log> {
    let topics = v
        .get("topics")
        .and_then(Value::as_array)
        .map(|t| t.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok(Log {
        address: field_str(v, "address")?,
        topics,
        data: field_str(v, "data").unwrap_or_default(),
        block_number: parse_hex_u64(&field_str(v, "blockNumber")?)?,
        tx_hash: field_str(v, "transactionHash")?,
        log_index: parse_hex_u64(&field_str(v, "logIndex")?)?,
    })
}

fn field_str(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("log missing field '{key}'"))
}

fn parse_hex_u64(s: &str) -> Result<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).with_context(|| format!("bad hex number '{s}'"))
}
