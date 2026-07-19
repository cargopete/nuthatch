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

/// After an endpoint fails, skip it for this long (unless every endpoint is unhealthy) — so one dead
/// provider doesn't cost a full request-timeout on every call that round-robins onto it. A partial
/// outage fails over fast instead of stalling the tip loop.
const ENDPOINT_COOLDOWN_MS: u64 = 30_000;

pub struct RpcClient {
    http: reqwest::Client,
    urls: Vec<String>,
    cursor: AtomicUsize,
    /// Per-endpoint health: the millis-since-epoch until which the endpoint is considered unhealthy
    /// (`0` = healthy). Set on a failed call, cleared on a successful one. Endpoints past their cooldown
    /// are tried first; still-unhealthy ones are the fallback of last resort (soonest-to-recover first).
    health: Vec<AtomicU64>,
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
        let health = urls.iter().map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            http,
            urls,
            cursor: AtomicUsize::new(0),
            health,
            requests: AtomicU64::new(0),
        })
    }

    /// Total HTTP requests attempted so far (including failover retries).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// The order to try endpoints for this call: healthy ones first (round-robin from the cursor for
    /// fairness), then any still in cooldown as a last resort (soonest-to-recover first). Advances the
    /// round-robin cursor once per call.
    fn endpoint_order(&self) -> Vec<usize> {
        let n = self.urls.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        let now = now_millis();
        let mut healthy = Vec::with_capacity(n);
        let mut cooling = Vec::with_capacity(n);
        for i in 0..n {
            let j = (start + i) % n;
            let until = self.health[j].load(Ordering::Relaxed);
            if until <= now {
                healthy.push(j);
            } else {
                cooling.push((until, j));
            }
        }
        cooling.sort_by_key(|(until, _)| *until);
        healthy
            .into_iter()
            .chain(cooling.into_iter().map(|(_, j)| j))
            .collect()
    }

    fn mark_healthy(&self, j: usize) {
        self.health[j].store(0, Ordering::Relaxed);
    }

    fn mark_unhealthy(&self, j: usize) {
        self.health[j].store(now_millis() + ENDPOINT_COOLDOWN_MS, Ordering::Relaxed);
    }

    /// Try endpoints in health order until one answers; a failed endpoint is put into cooldown, a
    /// successful one is cleared.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut last_err = anyhow!("all RPC endpoints failed");
        for j in self.endpoint_order() {
            let url = &self.urls[j];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
            match self.call_one(url, method, &params).await {
                Ok(v) => {
                    self.mark_healthy(j);
                    return Ok(v);
                }
                Err(e) => {
                    self.mark_unhealthy(j);
                    tracing::debug!("rpc {url} failed for {method}: {e:#}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// POST a raw JSON-RPC body (single object or a batch array) with the same health-ordered failover
    /// as `call`, returning the parsed response. Used for batch requests `call` can't express.
    async fn post_with_failover(&self, body: &Value) -> Result<Value> {
        let mut last_err = anyhow!("all RPC endpoints failed");
        for j in self.endpoint_order() {
            let url = &self.urls[j];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
            match self.post_one(url, body).await {
                Ok(v) => {
                    self.mark_healthy(j);
                    return Ok(v);
                }
                Err(e) => {
                    self.mark_unhealthy(j);
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
        // COR-3: a *partial* response (endpoint answered but a load-balanced/archive-vs-full split
        // returned `null` for some block) must be an error, not a partial map — else the caller defaults
        // the missing block's `block_timestamp` to 0 and *seals it permanently*, breaking determinism
        // (a re-run against a healthy endpoint yields a different timestamp → different content hash).
        // Erroring makes the seal path retry the whole window, exactly like a total failure.
        if out.len() != blocks.len() {
            let missing = blocks.iter().filter(|b| !out.contains_key(b)).count();
            bail!(
                "block_timestamps: {missing}/{} block(s) missing from the RPC response — refusing a \
                 partial map (would seal block_timestamp=0)",
                blocks.len()
            );
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

/// Wall-clock millis since the epoch — used only for endpoint-health cooldowns (a coarse "try again
/// after" timer), never for anything in the deterministic data path.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{merge_rpcs, RpcClient};

    fn v<const N: usize>(xs: [&str; N]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_failed_endpoint_is_tried_last_until_it_cools_down() {
        let c = RpcClient::new(v(["http://a", "http://b", "http://c"])).unwrap();
        // Endpoint 1 (b) just failed → it must sink to the back of the try order.
        c.mark_unhealthy(1);
        for _ in 0..5 {
            let order = c.endpoint_order();
            assert_eq!(order.len(), 3);
            assert_eq!(
                *order.last().unwrap(),
                1,
                "unhealthy endpoint is tried last"
            );
            // The two healthy endpoints lead, in some round-robin order.
            assert!(order[..2].contains(&0) && order[..2].contains(&2));
        }
        // A success clears it — back into normal rotation, no longer forced last.
        c.mark_healthy(1);
        let mut seen_first = false;
        for _ in 0..3 {
            if c.endpoint_order()[0] == 1 {
                seen_first = true;
            }
        }
        assert!(seen_first, "a recovered endpoint rejoins the round-robin");
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
