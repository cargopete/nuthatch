//! Embedded hot store: redb. Single writer (the indexer), many readers (the API). This is the
//! tip layer for entity point-reads; Parquet sealing + DuckDB analytics land in slice 2.

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::Path;
use std::sync::Arc;

const ENTITIES: TableDefinition<&str, &str> = TableDefinition::new("entities");
const META: TableDefinition<&str, &str> = TableDefinition::new("meta");
/// Block-hash checkpoints (block -> canonical hash we indexed against), for reorg detection.
const BLOCKS: TableDefinition<&str, &str> = TableDefinition::new("blocks");
/// Durable alert-delivery outbox (RFC-0008 C5): monotonic seq -> pending-delivery JSON. Survives
/// restart, so at-least-once delivery holds across a process bounce.
const OUTBOX: TableDefinition<&str, &str> = TableDefinition::new("outbox");
/// Meta key holding the next outbox sequence number.
const OUTBOX_SEQ: &str = "outbox_next_seq";

#[derive(Clone)]
pub struct Store {
    db: Arc<Database>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let db = Database::create(path)
            .with_context(|| format!("failed to open redb at {}", path.display()))?;
        // Materialise both tables up front so read txns never hit a missing table.
        let wtx = db.begin_write()?;
        {
            wtx.open_table(ENTITIES)?;
            wtx.open_table(META)?;
            wtx.open_table(BLOCKS)?;
            wtx.open_table(OUTBOX)?;
        }
        wtx.commit()?;
        Ok(Store { db: Arc::new(db) })
    }

    /// Push a pending alert delivery onto the durable outbox; returns its sequence number. A fast
    /// single redb write — enqueuing never blocks indexing on a slow/dead webhook (RFC-0008 C5).
    pub fn outbox_push(&self, payload: &str) -> Result<u64> {
        let wtx = self.db.begin_write()?;
        let seq;
        {
            let mut meta = wtx.open_table(META)?;
            seq = meta
                .get(OUTBOX_SEQ)?
                .and_then(|v| v.value().parse::<u64>().ok())
                .unwrap_or(0);
            meta.insert(OUTBOX_SEQ, (seq + 1).to_string().as_str())?;
            let mut ob = wtx.open_table(OUTBOX)?;
            ob.insert(Self::outbox_key(seq).as_str(), payload)?;
        }
        wtx.commit()?;
        Ok(seq)
    }

    fn outbox_key(seq: u64) -> String {
        format!("{seq:020}")
    }

    /// The oldest `limit` pending deliveries, as `(seq, payload)`, in enqueue order.
    pub fn outbox_pending(&self, limit: usize) -> Result<Vec<(u64, String)>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(OUTBOX)?;
        let mut out = Vec::with_capacity(limit.min(1024));
        for row in t.iter()? {
            let (k, v) = row?;
            let seq: u64 = k.value().parse().context("corrupt outbox key")?;
            out.push((seq, v.value().to_string()));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Remove a delivered entry (call only after a successful POST — at-least-once semantics).
    pub fn outbox_remove(&self, seq: u64) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(OUTBOX)?;
            t.remove(Self::outbox_key(seq).as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Number of pending deliveries — the `/status` outbox gauge.
    pub fn outbox_len(&self) -> u64 {
        let count = || -> Result<u64> {
            let rtx = self.db.begin_read()?;
            let t = rtx.open_table(OUTBOX)?;
            Ok(t.len()?)
        };
        count().unwrap_or(0)
    }

    /// Bound the outbox: if it exceeds `max`, drop the oldest entries down to `max`. Returns how many
    /// were dropped. This is the "never block the indexer" backstop — a dead webhook can't grow the
    /// outbox without limit; the oldest undelivered alerts are shed (loudly, by the caller).
    pub fn outbox_trim(&self, max: u64) -> Result<u64> {
        let len = self.outbox_len();
        if len <= max {
            return Ok(0);
        }
        let drop = len - max;
        let wtx = self.db.begin_write()?;
        let mut dropped = 0u64;
        {
            let mut t = wtx.open_table(OUTBOX)?;
            let doomed: Vec<String> = t
                .iter()?
                .filter_map(|r| r.ok())
                .take(drop as usize)
                .map(|(k, _)| k.value().to_string())
                .collect();
            for k in doomed {
                t.remove(k.as_str())?;
                dropped += 1;
            }
        }
        wtx.commit()?;
        Ok(dropped)
    }

    /// Key entities as `{block:012}-{log_index:06}` so iteration is chain-ordered.
    pub fn entity_key(block: u64, log_index: u64) -> String {
        format!("{block:012}-{log_index:06}")
    }

    fn block_key(block: u64) -> String {
        format!("{block:012}")
    }

    pub fn put_entity(&self, key: &str, json: &str) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(ENTITIES)?;
            t.insert(key, json)?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Commit a whole window's writes in ONE transaction (PERF-2): every decoded row + annotation, the
    /// window-boundary block-hash checkpoint, and the `last_block` watermark. The tip loop previously
    /// did a separate `begin_write`/`commit` (an fsync) *per row* — 2,000 logs meant 2,000 fsyncs, which
    /// capped tip-follow throughput far below the decode rate. One txn per window is also *more*
    /// crash-consistent: the window is the atomic unit (its watermark already advances once), so a crash
    /// leaves the store at a clean window boundary, never mid-window.
    pub fn commit_window(
        &self,
        entities: &[(String, String)],
        checkpoint: Option<(u64, &str)>,
        last_block: u64,
    ) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(ENTITIES)?;
            for (k, v) in entities {
                t.insert(k.as_str(), v.as_str())?;
            }
            if let Some((block, hash)) = checkpoint {
                let mut b = wtx.open_table(BLOCKS)?;
                b.insert(Self::block_key(block).as_str(), hash)?;
            }
            let mut m = wtx.open_table(META)?;
            m.insert("last_block", last_block.to_string().as_str())?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_entity(&self, key: &str) -> Result<Option<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        Ok(t.get(key)?.map(|v| v.value().to_string()))
    }

    pub fn count(&self) -> Result<u64> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        Ok(t.len()?)
    }

    /// The `limit` most-recent entities (highest keys first).
    pub fn recent(&self, limit: usize) -> Result<Vec<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        let mut out = Vec::with_capacity(limit.min(1024));
        for row in t.iter()?.rev() {
            let (_k, v) = row?;
            out.push(v.value().to_string());
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// The `limit` most-recent hot rows belonging to `table` (highest keys first).
    pub fn recent_by_table(&self, table: &str, limit: usize) -> Result<Vec<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        // Cap the pre-allocation: `limit` may be usize::MAX (rebuild wants "all rows"); the Vec
        // still grows as needed, we just don't reserve an absurd capacity up front.
        let mut out = Vec::with_capacity(limit.min(1024));
        for row in t.iter()?.rev() {
            let (_k, v) = row?;
            let s = v.value();
            let matches = serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|j| j.get("table").and_then(|t| t.as_str()).map(|t| t == table))
                .unwrap_or(false);
            if matches {
                out.push(s.to_string());
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The sealed watermark: the highest block whose rows have been sealed to Parquet and pruned from
    /// hot. Rows `> sealed_through` live in the hot store; rows `<= sealed_through` live in cold
    /// segments. `/sql` reads this to keep the hot∪cold union disjoint (COR-1). 0 if nothing sealed.
    pub fn sealed_through(&self) -> u64 {
        self.get_meta("sealed_through")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Every hot (unsealed) row, parsed and grouped by its logical `table` (RFC-0013). One full scan of
    /// the hot store — bounded, since sealed rows are pruned from hot, so this holds only the tip past
    /// the sealed watermark. Feeds the analytical `/sql` surface so the live tip is queryable alongside
    /// the sealed segments (hot and cold are disjoint by block range, so a plain `UNION ALL` is exact).
    pub fn hot_rows_by_table(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<serde_json::Value>>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        let mut out: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for row in t.iter()? {
            let (_k, v) = row?;
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(v.value()) {
                if let Some(table) = json.get("table").and_then(|t| t.as_str()) {
                    out.entry(table.to_string()).or_default().push(json);
                }
            }
        }
        Ok(out)
    }

    /// Entity JSON values whose block falls in `[from, to]`, chain-ordered. Used by sealing to
    /// gather a finalized block range for a Parquet segment.
    pub fn entities_in_range(&self, from: u64, to: u64) -> Result<Vec<String>> {
        let lo = format!("{from:012}-000000");
        let hi = format!("{to:012}-999999");
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        let mut out = Vec::new();
        for row in t.range(lo.as_str()..=hi.as_str())? {
            let (_k, v) = row?;
            out.push(v.value().to_string());
        }
        Ok(out)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(META)?;
        Ok(t.get(key)?.map(|v| v.value().to_string()))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(META)?;
            t.insert(key, value)?;
        }
        wtx.commit()?;
        Ok(())
    }

    /// Record the canonical hash we indexed a block against (a reorg checkpoint).
    pub fn set_block_hash(&self, block: u64, hash: &str) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut t = wtx.open_table(BLOCKS)?;
            t.insert(Self::block_key(block).as_str(), hash)?;
        }
        wtx.commit()?;
        Ok(())
    }

    pub fn get_block_hash(&self, block: u64) -> Result<Option<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(BLOCKS)?;
        Ok(t.get(Self::block_key(block).as_str())?
            .map(|v| v.value().to_string()))
    }

    /// All recorded checkpoints, highest block first — for walking back to a common ancestor.
    pub fn checkpoints_desc(&self) -> Result<Vec<(u64, String)>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(BLOCKS)?;
        let mut out = Vec::new();
        for row in t.iter()?.rev() {
            let (k, v) = row?;
            let block: u64 = k.value().parse().context("corrupt block key")?;
            out.push((block, v.value().to_string()));
        }
        Ok(out)
    }

    /// Reorg handling: drop every entity and checkpoint strictly above `block`. Returns the number
    /// of entities removed. The mutable hot store is the *only* place a reorg ever lands.
    pub fn rollback_to(&self, block: u64) -> Result<u64> {
        let wtx = self.db.begin_write()?;
        let mut removed = 0u64;
        {
            let mut entities = wtx.open_table(ENTITIES)?;
            let doomed: Vec<String> = entities
                .iter()?
                .filter_map(|row| row.ok())
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    let b: u64 = key.split('-').next()?.parse().ok()?;
                    (b > block).then_some(key)
                })
                .collect();
            for k in doomed {
                entities.remove(k.as_str())?;
                removed += 1;
            }

            let mut blocks = wtx.open_table(BLOCKS)?;
            let doomed: Vec<String> = blocks
                .iter()?
                .filter_map(|row| row.ok())
                .filter_map(|(k, _)| {
                    let key = k.value().to_string();
                    let b: u64 = key.parse().ok()?;
                    (b > block).then_some(key)
                })
                .collect();
            for k in doomed {
                blocks.remove(k.as_str())?;
            }
        }
        wtx.commit()?;
        Ok(removed)
    }

    /// Prune sealed entities from the hot store: remove entity rows whose block is in `[from, to]`.
    /// Returns the number of rows removed. Called once every table in the range has been sealed to
    /// its own Parquet segment (the whole range is safe to drop; the data survives in Parquet and is
    /// reachable via the DuckDB point-read fallback).
    pub fn prune_range(&self, from: u64, to: u64) -> Result<u64> {
        let lo = format!("{from:012}-000000");
        let hi = format!("{to:012}-999999");
        let wtx = self.db.begin_write()?;
        let mut removed = 0u64;
        {
            let mut t = wtx.open_table(ENTITIES)?;
            let doomed: Vec<String> = t
                .range(lo.as_str()..=hi.as_str())?
                .filter_map(|row| row.ok())
                .map(|(k, _)| k.value().to_string())
                .collect();
            for k in doomed {
                t.remove(k.as_str())?;
                removed += 1;
            }
        }
        wtx.commit()?;
        Ok(removed)
    }

    /// Prune entities in `[from, to]` **and** set a meta key, in ONE write transaction (hardening
    /// COR-1). Sealing uses this to advance the `sealed_through` watermark and drop the just-sealed rows
    /// from hot *atomically* — a `kill -9` can never leave a range committed to both the hot store and a
    /// sealed segment, which would permanently double-count it in `/sql` and on every balance rebuild.
    /// The seal itself is content-addressed (idempotent), so a crash *before* this txn simply re-seals
    /// the same range on restart; the watermark only advances once the prune is durable.
    pub fn prune_and_set_meta(
        &self,
        from: u64,
        to: u64,
        meta_key: &str,
        meta_val: &str,
    ) -> Result<u64> {
        let lo = format!("{from:012}-000000");
        let hi = format!("{to:012}-999999");
        let wtx = self.db.begin_write()?;
        let mut removed = 0u64;
        {
            let mut t = wtx.open_table(ENTITIES)?;
            let doomed: Vec<String> = t
                .range(lo.as_str()..=hi.as_str())?
                .filter_map(|row| row.ok())
                .map(|(k, _)| k.value().to_string())
                .collect();
            for k in doomed {
                t.remove(k.as_str())?;
                removed += 1;
            }
            let mut m = wtx.open_table(META)?;
            m.insert(meta_key, meta_val)?;
        }
        wtx.commit()?;
        Ok(removed)
    }

    /// Test/consistency helper: the set of entity keys currently stored (chain-ordered).
    #[cfg(test)]
    pub fn entity_keys(&self) -> Result<Vec<String>> {
        let rtx = self.db.begin_read()?;
        let t = rtx.open_table(ENTITIES)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, _) = row?;
            out.push(k.value().to_string());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        (store, dir)
    }

    /// A block: its number and how many transfers (log indices) it carries.
    fn apply_block(store: &Store, block: u64, n_logs: u64, hash: &str) {
        for li in 0..n_logs {
            let key = Store::entity_key(block, li);
            store.put_entity(&key, "{}").unwrap();
        }
        store.set_block_hash(block, hash).unwrap();
    }

    #[test]
    fn prune_range_removes_only_blocks_in_range() {
        let (store, _d) = temp_store();
        apply_block(&store, 10, 2, "h10");
        apply_block(&store, 11, 3, "h11");
        apply_block(&store, 12, 1, "h12");
        let removed = store.prune_range(10, 11).unwrap();
        assert_eq!(removed, 5); // blocks 10 (2) + 11 (3)
        assert_eq!(store.count().unwrap(), 1); // only block 12 remains
    }

    #[test]
    fn rollback_is_multi_table_correct() {
        // Rows from two logical tables interleaved by block; a reorg must drop them uniformly by
        // block regardless of table (storage is block-keyed, so this is multi-table convergence).
        let (store, _d) = temp_store();
        store
            .put_entity(&Store::entity_key(10, 0), r#"{"table":"a__x"}"#)
            .unwrap();
        store
            .put_entity(&Store::entity_key(10, 1), r#"{"table":"b__y"}"#)
            .unwrap();
        store
            .put_entity(&Store::entity_key(12, 0), r#"{"table":"a__x"}"#)
            .unwrap();
        store
            .put_entity(&Store::entity_key(12, 1), r#"{"table":"b__y"}"#)
            .unwrap();
        store.rollback_to(11).unwrap();
        let keys = store.entity_keys().unwrap();
        assert_eq!(
            keys.len(),
            2,
            "both block-12 rows (both tables) rolled back"
        );
        assert!(keys.iter().all(|k| k.starts_with("000000000010")));
    }

    #[test]
    fn rollback_removes_only_blocks_above_threshold() {
        let (store, _d) = temp_store();
        apply_block(&store, 10, 3, "h10");
        apply_block(&store, 11, 2, "h11");
        apply_block(&store, 12, 4, "h12");
        assert_eq!(store.count().unwrap(), 9);

        let removed = store.rollback_to(11).unwrap();
        assert_eq!(removed, 4); // block 12's four entities
        assert_eq!(store.count().unwrap(), 5); // blocks 10 + 11
        assert!(store.get_block_hash(12).unwrap().is_none());
        assert_eq!(store.get_block_hash(11).unwrap().as_deref(), Some("h11"));
    }

    proptest! {
        // Each case opens a real redb file, so keep the count modest for CI wall-clock.
        #![proptest_config(ProptestConfig::with_cases(48))]
        /// Reorg convergence: indexing a chain then reorging at a fork point and applying an
        /// alternate branch must yield exactly the same state as indexing the winning branch
        /// directly. Random fork depths, random block populations.
        #[test]
        fn reorg_converges_to_canonical(
            prefix in prop::collection::vec(1u64..5, 1..8),   // logs-per-block, blocks 0..len
            branch in prop::collection::vec(1u64..5, 0..6),   // alternate branch after the fork
            fork_back in 0usize..8,
        ) {
            // Build the "reorged" store: apply prefix, roll back, apply the alternate branch.
            let (reorged, _d1) = temp_store();
            for (i, &n) in prefix.iter().enumerate() {
                apply_block(&reorged, i as u64, n, &format!("a{i}"));
            }
            let fork = (prefix.len().saturating_sub(fork_back)).saturating_sub(1) as u64;
            reorged.rollback_to(fork).unwrap();
            for (j, &n) in branch.iter().enumerate() {
                let b = fork + 1 + j as u64;
                apply_block(&reorged, b, n, &format!("b{j}"));
            }

            // Build the "canonical" store fresh: prefix up to the fork, then the same branch.
            let (canonical, _d2) = temp_store();
            for (i, &n) in prefix.iter().enumerate() {
                if (i as u64) <= fork {
                    apply_block(&canonical, i as u64, n, &format!("a{i}"));
                }
            }
            for (j, &n) in branch.iter().enumerate() {
                let b = fork + 1 + j as u64;
                apply_block(&canonical, b, n, &format!("b{j}"));
            }

            prop_assert_eq!(reorged.entity_keys().unwrap(), canonical.entity_keys().unwrap());
        }
    }
}
