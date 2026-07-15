//! The ingestion `Source` — the seam between "where blocks come from" and everything downstream.
//!
//! Decode, hot store, sealing, IVM, and serving are all oblivious to whether a block arrived by
//! RPC polling or was handed to us in-process by a colocated reth node. That obliviousness is the
//! whole point of this trait: adding ExEx tip-ingestion is a new `Source` impl, never a fork of the
//! indexing logic (per the standing brief — no `#[cfg]` forks of business logic).
//!
//! - `RpcSource` (always available): polls a JSON-RPC endpoint. Works today, no node required.
//! - `ExExSource` (feature = "exex"): the "no third-party" sovereignty upgrade — native-block-time
//!   tip latency from a colocated reth node. Stubbed here with the design written up; see
//!   `docs/exex-design.md`.

use anyhow::Result;

use crate::rpc::{Log, RpcClient};

/// Everything the indexer needs from an ingestion source. Pull-shaped: the single-cursor loop asks
/// for the tip, verifies canonical hashes (reorg detection), and requests decoded logs for a range.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// Latest block the source can serve.
    async fn tip(&self) -> Result<u64>;

    /// Canonical block hash at `number`, or `None` if the source can't answer (retry later).
    async fn block_hash(&self, number: u64) -> Result<Option<String>>;

    /// Logs matching any of `addresses` + any of `topic0s` over the inclusive range `[from, to]`.
    async fn logs(
        &self,
        addresses: &[String],
        topic0s: &[String],
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>>;
}

#[async_trait::async_trait]
impl Source for RpcClient {
    async fn tip(&self) -> Result<u64> {
        self.block_number().await
    }

    async fn block_hash(&self, number: u64) -> Result<Option<String>> {
        // Disambiguate from this trait method — call the inherent one.
        RpcClient::block_hash(self, number).await
    }

    async fn logs(
        &self,
        addresses: &[String],
        topic0s: &[String],
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>> {
        self.get_logs(addresses, topic0s, from, to).await
    }
}

// ---------------------------------------------------------------------------
// ExEx source (feature-gated stub).
//
// A reth Execution Extension is compiled INTO the reth binary and runs in-process, consuming a
// `CanonStateNotification` stream over shared memory — no RPC, no serialization boundary — with
// `ChainCommitted` / `ChainReverted` variants and an `ExExEvent::FinishedHeight` back-channel so
// reth can prune. That gives native-block-time tip latency AND a first-class reorg signal.
//
// The design tension this stub captures: ExEx is PUSH (reth notifies us as blocks commit), while
// the indexer's `Source` is PULL (it asks for `[from,to]`). The bridge is a bounded in-memory
// buffer that a reth ExEx handler fills; `Source::logs` drains the requested range from it. The
// reth wiring itself (a `reth` dependency + node build) is deliberately NOT pulled in here — it's an
// enormous compile that needs a synced node to exercise, so it lands in a dedicated environment.
// This stub establishes the trait boundary and the push→pull contract; see docs/exex-design.md.
// ---------------------------------------------------------------------------
#[cfg(feature = "exex")]
pub mod exex {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// What a reth ExEx handler would push per committed block: its hash and decoded logs.
    #[derive(Default)]
    struct Committed {
        hash: String,
        logs: Vec<Log>,
    }

    /// Bridges reth's push notifications to the indexer's pull `Source`. A real ExEx handler calls
    /// `commit`/`revert` from `CanonStateNotification`; the indexer calls the `Source` methods.
    pub struct ExExSource {
        blocks: Mutex<BTreeMap<u64, Committed>>,
    }

    impl ExExSource {
        pub fn new() -> Self {
            Self {
                blocks: Mutex::new(BTreeMap::new()),
            }
        }

        /// Called by the reth ExEx handler on `ChainCommitted` (per block, pre-decoded).
        pub fn commit(&self, block: u64, hash: String, logs: Vec<Log>) {
            self.blocks
                .lock()
                .unwrap()
                .insert(block, Committed { hash, logs });
        }

        /// Called on `ChainReverted` — drop every buffered block above the revert point. (The hot
        /// store's own rollback still runs; this just keeps the buffer canonical.)
        pub fn revert(&self, from_block: u64) {
            self.blocks.lock().unwrap().retain(|&b, _| b < from_block);
        }
    }

    impl Default for ExExSource {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait::async_trait]
    impl Source for ExExSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0))
        }

        async fn block_hash(&self, number: u64) -> Result<Option<String>> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .get(&number)
                .map(|c| c.hash.clone()))
        }

        async fn logs(
            &self,
            addresses: &[String],
            topic0s: &[String],
            from: u64,
            to: u64,
        ) -> Result<Vec<Log>> {
            let blocks = self.blocks.lock().unwrap();
            let mut out = Vec::new();
            for (_, c) in blocks.range(from..=to) {
                for log in &c.logs {
                    let matches_addr = addresses
                        .iter()
                        .any(|a| log.address.eq_ignore_ascii_case(a));
                    let matches_topic = log
                        .topics
                        .first()
                        .map(|t| topic0s.iter().any(|s| s.eq_ignore_ascii_case(t)))
                        .unwrap_or(false);
                    if matches_addr && matches_topic {
                        out.push(log.clone());
                    }
                }
            }
            Ok(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn buffers_commits_and_serves_range() {
            let s = ExExSource::new();
            // ERC-20 Transfer topic0.
            let topic =
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string();
            let log = Log {
                address: "0xabc".into(),
                topics: vec![topic.clone()],
                data: "0x".into(),
                block_number: 10,
                block_hash: "0xhash10".into(),
                tx_hash: "0xt".into(),
                log_index: 0,
            };
            s.commit(10, "0xhash10".into(), vec![log]);
            assert_eq!(s.tip().await.unwrap(), 10);
            assert_eq!(s.block_hash(10).await.unwrap().as_deref(), Some("0xhash10"));
            assert_eq!(
                s.logs(&["0xABC".to_string()], &[topic.clone()], 0, 20)
                    .await
                    .unwrap()
                    .len(),
                1
            );

            s.revert(10); // reorg drops block 10
            assert_eq!(s.tip().await.unwrap(), 0);
        }
    }
}
