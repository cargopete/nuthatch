//! Per-table sealing (RFC-0001 step 4): once a block range is final, each table's rows in that
//! range are written to their own content-addressed Parquet segment, catalogued per table in the
//! manifest. The columnar cold layer is append-only — it never sees a reorg, because reorgs only
//! ever touch the mutable hot store (see store::rollback_to).
//!
//! All tables in a nest ingest from the same block stream and seal together per finalized range, so
//! `sealed_through` stays a single global watermark and the whole range is pruned from hot once every
//! table's segment is durable (the indexer does the prune).

use anyhow::{Context, Result};
use arrow::array::{ArrayRef, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SEGMENTS_DIR: &str = "segments";
pub const MANIFEST_FILE: &str = "manifest.json";

/// One sealed Parquet file. `hash` is the content address (sha256 of the file bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub hash: String,
    pub from_block: u64,
    pub to_block: u64,
    pub rows: usize,
    pub file: String,
    /// The discovered-child registry's content hash at seal time (RFC-0009): records exactly which
    /// factory-discovered set produced this segment, so its child rows are reproducible. `None` for a
    /// static (non-factory) nest. Absent in pre-RFC-0009 manifests (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_snapshot: Option<String>,
}

/// The segment catalogue: per-table lists of sealed segments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub tables: BTreeMap<String, Vec<Segment>>,
}

/// What a `seal_range` call sealed.
#[derive(Debug, Default)]
pub struct SealSummary {
    pub tables: usize,
    pub rows: usize,
}

/// Seal every table's rows in a finalized `[from, to]` range. Rows are grouped by their `table`
/// field; each group becomes one content-addressed Parquet segment catalogued under its table.
/// Returns None if the range held no rows.
pub fn seal_range(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
) -> Result<Option<SealSummary>> {
    seal_range_with_snapshot(dir, entity_json, from, to, None)
}

/// Like [`seal_range`], but records `registry_snapshot` (the discovered-child registry's content hash)
/// on each segment's manifest entry — the factory paths (RFC-0009) pass it so a segment records which
/// discovered set produced it. A static nest passes `None` (via `seal_range`).
pub fn seal_range_with_snapshot(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
    registry_snapshot: Option<&str>,
) -> Result<Option<SealSummary>> {
    let mut by_table: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<Value>(j) else {
            continue;
        };
        let table = v
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or("rows")
            .to_string();
        by_table.entry(table).or_default().push(v);
    }
    if by_table.is_empty() {
        return Ok(None);
    }

    let seg_dir = dir.join(SEGMENTS_DIR);
    std::fs::create_dir_all(&seg_dir)
        .with_context(|| format!("cannot create {}", seg_dir.display()))?;
    let mut manifest = load_manifest(dir)?;
    let mut summary = SealSummary::default();

    for (table, rows) in by_table {
        let batch = rows_to_batch(&rows)?;
        let bytes = write_parquet(&batch)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        let file = format!("{table}-{hash}.parquet");
        let segments = manifest.tables.entry(table).or_default();
        // Content-addressed idempotency: an identical segment (same table + hash) is already
        // catalogued, so re-sealing the same rows — e.g. re-running `nuthatch screen` over a range to
        // re-audit — is a no-op rather than a double-listed (double-counted) segment.
        if segments.iter().any(|s| s.hash == hash) {
            continue;
        }
        std::fs::write(seg_dir.join(&file), &bytes).context("failed to write segment")?;
        summary.tables += 1;
        summary.rows += rows.len();
        segments.push(Segment {
            hash,
            from_block: from,
            to_block: to,
            rows: rows.len(),
            file,
            registry_snapshot: registry_snapshot.map(str::to_string),
        });
    }

    save_manifest(dir, &manifest)?;
    Ok(Some(summary))
}

/// Build an Arrow batch from a table's JSON rows. `block_number`/`log_index` are UInt64; every other
/// column is Utf8 (values already carry their canonical text form — hex, decimal, or string).
fn rows_to_batch(rows: &[Value]) -> Result<RecordBatch> {
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for r in rows {
        if let Some(obj) = r.as_object() {
            columns.extend(obj.keys().cloned());
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();

    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for col in &columns {
        if col == "block_number" || col == "log_index" || col == "_seq" || col == "block_timestamp"
        {
            let vals: Vec<u64> = rows
                .iter()
                .map(|r| r.get(col).and_then(Value::as_u64).unwrap_or(0))
                .collect();
            fields.push(Field::new(col, DataType::UInt64, false));
            arrays.push(Arc::new(UInt64Array::from(vals)));
        } else {
            let vals: Vec<Option<String>> = rows
                .iter()
                .map(|r| match r.get(col) {
                    Some(Value::String(s)) => Some(s.clone()),
                    None | Some(Value::Null) => None,
                    Some(other) => Some(other.to_string()),
                })
                .collect();
            fields.push(Field::new(col, DataType::Utf8, true));
            arrays.push(Arc::new(StringArray::from(vals)));
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .context("failed to build record batch")
}

fn write_parquet(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
        .context("failed to create parquet writer")?;
    writer.write(batch).context("failed to write batch")?;
    writer.close().context("failed to finalise parquet")?;
    Ok(buf)
}

/// Load the segment catalogue (empty if none yet).
pub fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = manifest_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("corrupt segments manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e).context("failed to read manifest"),
    }
}

/// Startup integrity pass: verify every manifest segment's file exists and its bytes hash to the
/// recorded content address. A file that's missing, unreadable, or hash-mismatched is corrupt or
/// tampered with — quarantine it (move to a sibling `quarantine/` dir so `define_views` skips it) and
/// log loudly, then continue. A corrupt segment must *reduce* a table's cold data, never crash-loop the
/// node. Sealed data is immutable and content-addressed, so a hash mismatch is unambiguous corruption.
/// Returns the number of segments quarantined. Best-effort — never fatal (an IO error just logs).
pub fn verify_and_quarantine(dir: &Path) -> Result<usize> {
    let manifest = load_manifest(dir)?;
    let seg_dir = dir.join(SEGMENTS_DIR);
    // Sibling of `segments/`, deliberately *outside* it — nothing globs or SQL-reads the quarantine.
    let quarantine = dir.join("quarantine");
    let mut quarantined = 0usize;

    for (table, segs) in &manifest.tables {
        for s in segs {
            let path = seg_dir.join(&s.file);
            let reason = match std::fs::read(&path) {
                Ok(bytes) if hex::encode(Sha256::digest(&bytes)) == s.hash => continue, // intact
                Ok(_) => "hash mismatch (corrupt or tampered)",
                // Already gone from disk — nothing to move; `define_views` skips it. Not counted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => "unreadable",
            };
            std::fs::create_dir_all(&quarantine).ok();
            let dest = quarantine.join(&s.file);
            match std::fs::rename(&path, &dest) {
                Ok(()) => {
                    tracing::error!(
                        "quarantined segment {} for table {table} ({reason}) → {} — cold data for this \
                         table is reduced; re-seal the range to restore it",
                        s.file,
                        dest.display()
                    );
                    quarantined += 1;
                }
                Err(e) => tracing::error!(
                    "segment {} for {table} is {reason} but could not be quarantined: {e}",
                    s.file
                ),
            }
        }
    }
    if quarantined > 0 {
        tracing::error!(
            "startup integrity: quarantined {quarantined} corrupt segment(s) — data is reduced, node \
             continues; investigate disk health and re-seal the affected ranges"
        );
    }
    Ok(quarantined)
}

fn save_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let raw = serde_json::to_string_pretty(manifest)?;
    // The manifest is the segment catalogue — the crown jewels of a `kill -9`-survivable single binary
    // (a half-written `manifest.json` orphans every otherwise-fine `.parquet` and fails all cold reads,
    // deadlock-review finding M8). Write a sibling temp file then rename it over the target: `rename`
    // is atomic on the same filesystem, so a reader/crash sees either the old manifest or the new one,
    // never a torn one.
    let path = manifest_path(dir);
    let tmp = path.with_extension("json.tmp");
    // COR-9: fsync the temp file's *bytes* before the rename, and the directory *entry* after — so the
    // atomic rename survives power loss, not just process death. Without the fsyncs a `rename` can be
    // reordered before the data hits disk, exposing a torn/empty manifest that orphans the segments.
    {
        let f = std::fs::File::create(&tmp).context("failed to create manifest temp")?;
        use std::io::Write;
        (&f).write_all(raw.as_bytes())
            .context("failed to write manifest temp")?;
        f.sync_all().context("failed to fsync manifest temp")?;
    }
    std::fs::rename(&tmp, &path).context("failed to install manifest")?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all(); // best-effort dir fsync (unsupported on some platforms)
    }
    Ok(())
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(SEGMENTS_DIR).join(MANIFEST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::fs::File;

    fn transfer(block: u64, li: u64, value: &str) -> String {
        format!(
            r#"{{"table":"usdc__transfer","from":"0xaaaa","to":"0xbbbb","value":"{value}","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }
    fn approval(block: u64, li: u64) -> String {
        format!(
            r#"{{"table":"usdc__approval","owner":"0xaaaa","spender":"0xdddd","value":"1","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }

    #[test]
    fn verify_quarantines_a_corrupt_segment_and_leaves_intact_ones() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[transfer(101, 0, "7")], 101, 101).unwrap();
        let manifest = load_manifest(dir.path()).unwrap();
        let segs = &manifest.tables["usdc__transfer"];
        assert_eq!(segs.len(), 2);

        // A clean tree quarantines nothing.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);

        // Corrupt the first segment's bytes (simulate disk rot / tampering).
        let bad = dir.path().join(SEGMENTS_DIR).join(&segs[0].file);
        std::fs::write(&bad, b"not a parquet file anymore").unwrap();

        // Verify quarantines exactly the corrupt one; the intact one stays put.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 1);
        assert!(!bad.exists(), "corrupt file moved out of segments/");
        assert!(
            dir.path().join("quarantine").join(&segs[0].file).exists(),
            "corrupt file is in quarantine/"
        );
        assert!(
            dir.path().join(SEGMENTS_DIR).join(&segs[1].file).exists(),
            "intact segment untouched"
        );
        // Idempotent: the already-quarantined (now-missing) segment isn't re-counted.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);
    }

    #[test]
    fn seals_each_table_to_its_own_segment() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
        ];
        let summary = seal_range(dir.path(), &rows, 100, 101).unwrap().unwrap();
        assert_eq!(summary.tables, 2); // transfer + approval
        assert_eq!(summary.rows, 3);

        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.tables["usdc__transfer"].len(), 1);
        assert_eq!(manifest.tables["usdc__transfer"][0].rows, 2);
        assert_eq!(manifest.tables["usdc__approval"][0].rows, 1);

        // The transfer segment reads back with the right rows.
        let seg = &manifest.tables["usdc__transfer"][0];
        let file = File::open(dir.path().join(SEGMENTS_DIR).join(&seg.file)).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let total: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn empty_range_seals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seal_range(dir.path(), &[], 1, 2).unwrap().is_none());
    }

    #[test]
    fn content_address_is_deterministic() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let rows = vec![transfer(1, 0, "1")];
        seal_range(dir1.path(), &rows, 1, 1).unwrap();
        seal_range(dir2.path(), &rows, 1, 1).unwrap();
        let a = &load_manifest(dir1.path()).unwrap().tables["usdc__transfer"][0].hash;
        let b = &load_manifest(dir2.path()).unwrap().tables["usdc__transfer"][0].hash;
        assert_eq!(a, b); // same rows in → same content address
    }

    /// RFC-0004 §1 path-equivalence: rows sealed *directly* (the seal-direct backfill path) and the
    /// same rows sealed *after a redb round-trip* (the hot-then-seal path) yield byte-identical
    /// segments. `seal_range` is the one shared writer, so the two backfill paths are provably the
    /// same bytes — the determinism claim the optimisation rests on.
    #[test]
    fn seal_direct_matches_seal_via_hot_store() {
        use crate::store::Store;
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
            transfer(102, 0, "9"),
        ];

        // Path A — direct: seal the decoded rows as-is.
        let da = tempfile::tempdir().unwrap();
        seal_range(da.path(), &rows, 100, 102).unwrap();

        // Path B — via hot store: write to redb, read the range back, then seal.
        let db = tempfile::tempdir().unwrap();
        let store = Store::open(&db.path().join("hot.redb")).unwrap();
        for r in &rows {
            let v: Value = serde_json::from_str(r).unwrap();
            let key = Store::entity_key(
                v["block_number"].as_u64().unwrap(),
                v["log_index"].as_u64().unwrap(),
            );
            store.put_entity(&key, r).unwrap();
        }
        let readback = store.entities_in_range(100, 102).unwrap();
        seal_range(db.path(), &readback, 100, 102).unwrap();

        // Same tables, same per-table content hashes.
        let ma = load_manifest(da.path()).unwrap();
        let mb = load_manifest(db.path()).unwrap();
        assert_eq!(
            ma.tables.keys().collect::<Vec<_>>(),
            mb.tables.keys().collect::<Vec<_>>()
        );
        for (table, segs) in &ma.tables {
            assert_eq!(
                segs[0].hash, mb.tables[table][0].hash,
                "segment hash differs for {table} between direct and via-hot-store paths"
            );
        }
    }
}
