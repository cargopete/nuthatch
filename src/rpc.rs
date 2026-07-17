//! The thinnest JSON-RPC client that works: `eth_blockNumber` + `eth_getLogs`, with round-robin
//! failover across the configured endpoints. No ExEx yet — that's the sovereignty upgrade later.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// How many times a whole `block_timestamps` batch is retried before it's returned as an error rather
/// than silently yielding an all-zeros timestamp map into the sealed path.
const TIMESTAMP_ATTEMPTS: usize = 4;

/// Merge `preferred` RPC endpoints ahead of a `fallback` list, preserving order and dropping
/// duplicates. Used by `init --rpc` and `dev --rpc` to prefer a user's own node while keeping the
/// built-in / configured endpoints as fallback. An empty `preferred` leaves `fallback` untouched.
pub fn merge_rpcs(preferred: &[String], fallback: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in preferred.iter().cloned().chain(fallback) {
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

pub struct RpcClient {
    http: reqwest::Client,
    urls: Vec<String>,
    cursor: AtomicUsize,
    /// Total HTTP requests attempted (incl. failover retries) — a benchmark/observability metric.
    requests: AtomicU64,
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
    pub block_hash: String,
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
        Ok(Self {
            http,
            urls,
            cursor: AtomicUsize::new(0),
            requests: AtomicU64::new(0),
        })
    }

    /// Total HTTP requests attempted so far (including failover retries).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Try each endpoint once, starting from the round-robin cursor, until one answers.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let n = self.urls.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut last_err = anyhow!("all RPC endpoints failed");
        for i in 0..n {
            let url = &self.urls[(start + i) % n];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
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

    /// POST a raw JSON-RPC body (single object or a batch array) with the same round-robin failover
    /// as `call`, returning the parsed response. Used for batch requests `call` can't express.
    async fn post_with_failover(&self, body: &Value) -> Result<Value> {
        let n = self.urls.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut last_err = anyhow!("all RPC endpoints failed");
        for i in 0..n {
            let url = &self.urls[(start + i) % n];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
            match self.post_one(url, body).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::debug!("rpc {url} failed for batch: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn post_one(&self, url: &str, body: &Value) -> Result<Value> {
        Ok(self
            .http
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
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

    /// A storage slot's value at `address` (latest block) — used to read the EIP-1967 proxy slot.
    pub async fn get_storage_at(&self, address: &str, slot: &str) -> Result<String> {
        let result = self
            .call("eth_getStorageAt", json!([address, slot, "latest"]))
            .await?;
        Ok(result.as_str().unwrap_or("0x0").to_string())
    }

    /// Contract bytecode at `address` as of `block`. `"0x"` (empty) means not yet deployed.
    pub async fn get_code(&self, address: &str, block: u64) -> Result<String> {
        let result = self
            .call("eth_getCode", json!([address, format!("0x{block:x}")]))
            .await?;
        Ok(result.as_str().unwrap_or("0x").to_string())
    }

    /// Unix timestamps (seconds) for the given block numbers, fetched in a single JSON-RPC batch so
    /// even a dense window costs one round-trip.
    ///
    /// Two different "missing" cases, deliberately kept distinct because timestamps feed the sealed
    /// (immutable) path: a block the endpoint *answered but omitted* is simply absent from the returned
    /// map (best-effort; the caller stores 0 for it), but a *whole-batch request failure* is retried a
    /// few times and then returned as `Err` — never silently collapsed into an all-zeros map, which
    /// would bake `block_timestamp = 0` into a permanent segment from a transient blip.
    pub async fn block_timestamps(&self, blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        if blocks.is_empty() {
            return Ok(HashMap::new());
        }
        let batch: Vec<Value> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| {
                json!({ "jsonrpc": "2.0", "id": i, "method": "eth_getBlockByNumber",
                        "params": [format!("0x{b:x}"), false] })
            })
            .collect();
        let body = Value::Array(batch);
        let mut resp = None;
        let mut last_err = None;
        for attempt in 0..TIMESTAMP_ATTEMPTS {
            match self.post_with_failover(&body).await {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    tracing::debug!("block_timestamps attempt {} failed: {e:#}", attempt + 1);
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        200 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
        let resp = match resp {
            Some(r) => r,
            None => {
                return Err(last_err
                    .unwrap()
                    .context("block_timestamps batch failed after retries"))
            }
        };
        let mut out = HashMap::new();
        for item in resp.as_array().into_iter().flatten() {
            let Some(idx) = item.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let Some(&block) = blocks.get(idx as usize) else {
                continue;
            };
            if let Some(ts) = item
                .pointer("/result/timestamp")
                .and_then(Value::as_str)
                .and_then(|s| parse_hex_u64(s).ok())
            {
                out.insert(block, ts);
            }
        }
        Ok(out)
    }

    /// The node's `finalized` block number (L1-aware on an L2 like Arbitrum), or None if the
    /// endpoint doesn't serve the `finalized` tag. Used by the `FinalizedTag` finality policy.
    pub async fn finalized_block(&self) -> Result<Option<u64>> {
        let result = self
            .call("eth_getBlockByNumber", json!(["finalized", false]))
            .await?;
        Ok(result
            .get("number")
            .and_then(Value::as_str)
            .and_then(|s| parse_hex_u64(s).ok()))
    }

    /// Canonical block hash for a height, or None if the node doesn't have that block.
    pub async fn block_hash(&self, number: u64) -> Result<Option<String>> {
        let result = self
            .call(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        Ok(result.get("hash").and_then(Value::as_str).map(String::from))
    }

    /// One combined `eth_getLogs` across all `addresses`, matching any of `topic0s`.
    pub async fn get_logs(
        &self,
        addresses: &[String],
        topic0s: &[String],
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>> {
        let mut filter = serde_json::Map::new();
        // An empty address list means "no address filter" (topic0-only) — the factory tip regime
        // (RFC-0009 §3) fetches this way so a child created and active in the same block is already in
        // hand. Sending `"address": []` would instead match nothing, so omit the field when empty.
        if !addresses.is_empty() {
            filter.insert("address".into(), json!(addresses));
        }
        if !topic0s.is_empty() {
            filter.insert("topics".into(), json!([topic0s]));
        }
        filter.insert("fromBlock".into(), json!(format!("0x{from:x}")));
        filter.insert("toBlock".into(), json!(format!("0x{to:x}")));
        let result = self
            .call("eth_getLogs", json!([Value::Object(filter)]))
            .await?;
        let arr = result
            .as_array()
            .ok_or_else(|| anyhow!("eth_getLogs did not return an array"))?;
        arr.iter().map(parse_log).collect()
    }
}

fn parse_log(v: &Value) -> Result<Log> {
    let topics = v
        .get("topics")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(Log {
        address: field_str(v, "address")?,
        topics,
        data: field_str(v, "data").unwrap_or_default(),
        block_number: parse_hex_u64(&field_str(v, "blockNumber")?)?,
        block_hash: field_str(v, "blockHash").unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::merge_rpcs;

    fn v<const N: usize>(xs: [&str; N]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_preferred_leaves_fallback_untouched() {
        assert_eq!(merge_rpcs(&[], v(["a", "b"])), v(["a", "b"]));
    }

    #[test]
    fn preferred_go_first_then_fallback() {
        assert_eq!(
            merge_rpcs(&v(["mine"]), v(["a", "b"])),
            v(["mine", "a", "b"])
        );
    }

    #[test]
    fn duplicates_are_dropped_keeping_first_position() {
        // A preferred URL already present in the fallback should surface once, at the front.
        assert_eq!(merge_rpcs(&v(["a"]), v(["a", "b"])), v(["a", "b"]));
        // Repeated preferred entries collapse too.
        assert_eq!(merge_rpcs(&v(["m", "m", "n"]), v(["n"])), v(["m", "n"]));
    }
}
