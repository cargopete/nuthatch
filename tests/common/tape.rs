//! `TapeSource` — a scripted, mutable chain the test drives, plus the fixtures/helpers around it.
//!
//! Why this exists: the crate's other test doubles all answer `block_hash -> None`, which makes
//! `detect_reorg` a no-op — so reorgs are untestable through them. `TapeSource` answers `block_hash`
//! with the *current canonical* hash (`Some`), supplies non-zero timestamps (a zero-timestamp window
//! is refused and retried forever), and exposes `finalized()` as the sealing trigger the test controls
//! precisely. That's the whole point: it lets an integration test drive the real `spawn_nest` /
//! `spawn_roost` loop through land → seal → reorg → halt without a network or a wall-clock race.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use nuthatch::config::Config;
use nuthatch::rpc::Log;
use nuthatch::source::Source;

/// The canonical ERC-20 `Transfer(address,address,uint256)` topic0.
pub const TRANSFER_TOPIC0: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// The example USDC address (Arbitrum One) — reused so tests exercise real checksummed addresses.
pub const USDC: &str = "0xaf88d065e77c8cC2239327C5EDb3A432268e5831";
/// The example ARB address (Arbitrum One).
pub const ARB: &str = "0x912CE59144191C1204E64559FE8253a0e49E6548";

/// A deterministic 20-byte account address from a small id, e.g. `account(1)` → `0x0000…0001`.
pub fn account(id: u64) -> String {
    format!("0x{id:040x}")
}

/// A deterministic 32-byte block hash for `(number, variant)`. `variant` lets a reorg mint a distinct
/// hash for the same height (`variant = 0` canonical, `1` the first replacement, …).
pub fn block_hash(number: u64, variant: u64) -> String {
    format!("0x{:032x}{:032x}", variant, number)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One block on the tape: its canonical hash, header timestamp (non-zero!), and emitted logs.
#[derive(Clone, Debug)]
pub struct BlockFixture {
    pub hash: String,
    pub timestamp: u64,
    pub logs: Vec<Log>,
}

/// Build an ERC-20 `Transfer` log. Topics are `[topic0, from(32B), to(32B)]`; `data` is the 32-byte
/// big-endian value — exactly the on-wire shape the decode registry parses.
pub fn transfer_log(
    address: &str,
    block: u64,
    log_index: u64,
    block_hash: &str,
    from: &str,
    to: &str,
    value: u128,
) -> Log {
    Log {
        address: address.to_string(),
        topics: vec![
            TRANSFER_TOPIC0.to_string(),
            topic_address(from),
            topic_address(to),
        ],
        data: format!("0x{value:064x}"),
        block_number: block,
        block_hash: block_hash.to_string(),
        // Deterministic from (block, log_index) — no wall clock.
        tx_hash: format!("0x{:064x}", (block << 20) | log_index),
        log_index,
    }
}

/// A 20-byte address left-padded to a 32-byte indexed-topic word.
fn topic_address(addr: &str) -> String {
    let h = addr.trim_start_matches("0x");
    format!("0x{h:0>64}")
}

/// A block carrying `transfers` (`(from, to, value)`), log-indexed `0..n`, for one emitting `address`.
pub fn transfers_block(
    number: u64,
    variant: u64,
    timestamp: u64,
    address: &str,
    transfers: &[(&str, &str, u128)],
) -> BlockFixture {
    let hash = block_hash(number, variant);
    let logs = transfers
        .iter()
        .enumerate()
        .map(|(i, (from, to, value))| {
            transfer_log(address, number, i as u64, &hash, from, to, *value)
        })
        .collect();
    BlockFixture {
        hash,
        timestamp,
        logs,
    }
}

/// A block that emits nothing (used to advance the tip past the sealing point without adding rows).
pub fn empty_block(number: u64, variant: u64, timestamp: u64) -> BlockFixture {
    BlockFixture {
        hash: block_hash(number, variant),
        timestamp,
        logs: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// TapeSource
// ---------------------------------------------------------------------------

struct Tape {
    blocks: BTreeMap<u64, BlockFixture>,
    tip: u64,
    finalized: u64,
}

/// A scripted chain with interior mutability. Test-facing mutators (`advance_tip_to`,
/// `advance_finalized_to`, `reorg`, `insert_block`) drive it; the [`Source`] impl is what the indexer
/// sees. A non-zero timestamp per block is a hard requirement — `process_window` refuses to commit a
/// zero-timestamp window and would retry forever.
pub struct TapeSource {
    inner: Mutex<Tape>,
}

impl Default for TapeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl TapeSource {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Tape {
                blocks: BTreeMap::new(),
                tip: 0,
                finalized: 0,
            }),
        }
    }

    /// Insert/overwrite a block. Raises the tip to `number` if it was behind (does not lower it).
    pub fn insert_block(&self, number: u64, fixture: BlockFixture) {
        let mut t = self.inner.lock().unwrap();
        t.blocks.insert(number, fixture);
        if number > t.tip {
            t.tip = number;
        }
    }

    /// Set the chain tip (the highest block the source will serve).
    pub fn advance_tip_to(&self, n: u64) {
        self.inner.lock().unwrap().tip = n;
    }

    /// Set the `finalized` block — the sealing trigger. `seal_ceiling` takes the FinalizedTag branch on
    /// an L2 chain (arbitrum-one/base), so this is exactly how far the test lets sealing advance.
    pub fn advance_finalized_to(&self, n: u64) {
        self.inner.lock().unwrap().finalized = n;
    }

    /// Rewrite the chain above `fork_block`: drop every block `> fork_block`, then install
    /// `replacement` as blocks `fork_block + 1, fork_block + 2, …` (new hashes + new logs). The tip
    /// becomes the highest replacement block. Blocks `<= fork_block` are untouched (the surviving
    /// ancestry `detect_reorg` walks back to).
    pub fn reorg(&self, fork_block: u64, replacement: Vec<BlockFixture>) {
        let mut t = self.inner.lock().unwrap();
        let above: Vec<u64> = t
            .blocks
            .range((fork_block + 1)..)
            .map(|(k, _)| *k)
            .collect();
        for k in above {
            t.blocks.remove(&k);
        }
        let mut tip = fork_block;
        for (offset, fixture) in replacement.into_iter().enumerate() {
            let number = fork_block + 1 + offset as u64;
            t.blocks.insert(number, fixture);
            tip = number;
        }
        t.tip = tip;
    }
}

#[async_trait]
impl Source for TapeSource {
    async fn tip(&self) -> Result<u64> {
        Ok(self.inner.lock().unwrap().tip)
    }

    async fn block_hash(&self, number: u64) -> Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .blocks
            .get(&number)
            .map(|b| b.hash.clone()))
    }

    async fn finalized(&self) -> Result<Option<u64>> {
        Ok(Some(self.inner.lock().unwrap().finalized))
    }

    async fn block_timestamps(&self, blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        let t = self.inner.lock().unwrap();
        Ok(blocks
            .iter()
            .filter_map(|b| t.blocks.get(b).map(|f| (*b, f.timestamp)))
            .collect())
    }

    async fn logs(
        &self,
        addresses: &[String],
        topic0s: &[String],
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>> {
        let t = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for (_, fixture) in t.blocks.range(from..=to) {
            for log in &fixture.logs {
                // Empty address list = "any address" (a real provider's factory / topic0-only fetch).
                let addr_ok = addresses.is_empty()
                    || addresses
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(&log.address));
                // Empty topic list = "any topic" (mirror a provider; the indexer always sends topics).
                let topic_ok = topic0s.is_empty()
                    || log
                        .topics
                        .first()
                        .map(|t0| topic0s.iter().any(|s| s.eq_ignore_ascii_case(t0)))
                        .unwrap_or(false);
                if addr_ok && topic_ok {
                    out.push(log.clone());
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Nest scaffolding — `init`'s OUTPUT without `init`'s network dependency.
// ---------------------------------------------------------------------------

/// Write a real nest into `dir`: a `nuthatch.toml` (arbitrum-one → FinalizedTag finality, so
/// `finalized()` controls sealing) plus `abis/erc20.json` copied from the vendored example ABI. Returns
/// the loaded [`Config`]. One `Transfer`-only ERC-20 contract at `address`, backfilling from block 1.
pub fn scaffold_nest(dir: &Path, name: &str, address: &str) -> Config {
    let abi_dir = dir.join("abis");
    std::fs::create_dir_all(&abi_dir).expect("create abis dir");
    let example_abi = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/roost/nests/usdc/abis/erc20.json"
    );
    let abi = std::fs::read(example_abi).expect("read example erc20 abi");
    std::fs::write(abi_dir.join("erc20.json"), abi).expect("write erc20 abi");

    let toml = format!(
        r#"[nest]
name = "{name}"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = []

[[contracts]]
alias = "{name}"
address = "{address}"
start_block = 1
abi = "abis/erc20.json"
events = ["Transfer"]
"#
    );
    std::fs::write(dir.join("nuthatch.toml"), toml).expect("write nuthatch.toml");
    Config::load(dir).expect("load scaffolded config")
}

/// The event table a nest named `name` decodes its transfers into (`{alias}__transfer`).
pub fn transfer_table(name: &str) -> String {
    format!("{name}__transfer")
}

// ---------------------------------------------------------------------------
// Polling — bounded waits on observable state (never a fixed sleep driving the loop).
// ---------------------------------------------------------------------------

/// Poll `cond` every 25 ms until it holds or `timeout` elapses. Returns whether it held. Used to
/// observe pipeline progress deterministically without racing on a fixed sleep.
pub async fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A generous ceiling for a bounded poll. The loop's own idle re-poll is ~2 s, so this comfortably
/// covers reorg detection + reconvergence while still failing fast on a genuine hang.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(20);
