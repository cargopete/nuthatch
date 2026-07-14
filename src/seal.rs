//! Sealing: once a block range is final (past reorg risk), its entities are written to an
//! immutable, content-addressed Parquet segment. The columnar cold layer is append-only — it
//! never sees a reorg, because reorgs only ever touch the mutable hot store (see store::rollback_to).
//!
//! Slice-2 scope: segments are *written and catalogued*; the hot store is not pruned yet (that
//! lands with the DuckDB read-only serving path, so the API keeps answering point-reads meanwhile).

use anyhow::{Context, Result};
use arrow::array::{ArrayRef, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
}

/// The decoded shape we seal. Mirrors decode::Transfer's JSON (kept structurally in sync).
#[derive(Debug, Deserialize)]
struct Row {
    from: String,
    to: String,
    value: Option<String>,
    value_hex: String,
    block_number: u64,
    tx_hash: String,
    log_index: u64,
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt64, false),
        Field::new("from", DataType::Utf8, false),
        Field::new("to", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, true),
        Field::new("value_hex", DataType::Utf8, false),
        Field::new("tx_hash", DataType::Utf8, false),
    ]))
}

/// Seal the given entity-JSON values (a finalized `[from, to]` range) into a content-addressed
/// Parquet segment under `dir/segments/`, and append it to the manifest. Returns None if empty.
pub fn seal_range(dir: &Path, entity_json: &[String], from: u64, to: u64) -> Result<Option<Segment>> {
    if entity_json.is_empty() {
        return Ok(None);
    }
    let rows: Vec<Row> = entity_json
        .iter()
        .map(|j| serde_json::from_str(j).context("corrupt entity JSON while sealing"))
        .collect::<Result<_>>()?;

    let batch = to_batch(&rows)?;
    let bytes = write_parquet(&batch)?;

    // Content address: sha256 of the exact file bytes.
    let hash = hex::encode(Sha256::digest(&bytes));
    let seg_dir = dir.join(SEGMENTS_DIR);
    std::fs::create_dir_all(&seg_dir).with_context(|| format!("cannot create {}", seg_dir.display()))?;
    let file_name = format!("{hash}.parquet");
    std::fs::write(seg_dir.join(&file_name), &bytes).context("failed to write segment")?;

    let segment = Segment {
        hash,
        from_block: from,
        to_block: to,
        rows: rows.len(),
        file: file_name,
    };
    append_manifest(dir, &segment)?;
    Ok(Some(segment))
}

fn to_batch(rows: &[Row]) -> Result<RecordBatch> {
    let blocks: ArrayRef = Arc::new(UInt64Array::from(rows.iter().map(|r| r.block_number).collect::<Vec<_>>()));
    let logidx: ArrayRef = Arc::new(UInt64Array::from(rows.iter().map(|r| r.log_index).collect::<Vec<_>>()));
    let from: ArrayRef = Arc::new(StringArray::from(rows.iter().map(|r| r.from.as_str()).collect::<Vec<_>>()));
    let to: ArrayRef = Arc::new(StringArray::from(rows.iter().map(|r| r.to.as_str()).collect::<Vec<_>>()));
    let value: ArrayRef = Arc::new(StringArray::from(rows.iter().map(|r| r.value.clone()).collect::<Vec<Option<String>>>()));
    let value_hex: ArrayRef = Arc::new(StringArray::from(rows.iter().map(|r| r.value_hex.as_str()).collect::<Vec<_>>()));
    let tx: ArrayRef = Arc::new(StringArray::from(rows.iter().map(|r| r.tx_hash.as_str()).collect::<Vec<_>>()));
    RecordBatch::try_new(schema(), vec![blocks, logidx, from, to, value, value_hex, tx])
        .context("failed to build record batch")
}

fn write_parquet(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
        .context("failed to create parquet writer")?;
    writer.write(batch).context("failed to write batch")?;
    writer.close().context("failed to finalise parquet")?;
    Ok(buf)
}

/// Load the segment catalogue (empty if none yet).
pub fn load_manifest(dir: &Path) -> Result<Vec<Segment>> {
    let path = manifest_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("corrupt segments manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).context("failed to read manifest"),
    }
}

fn append_manifest(dir: &Path, segment: &Segment) -> Result<()> {
    let mut segments = load_manifest(dir)?;
    segments.push(segment.clone());
    let raw = serde_json::to_string_pretty(&segments)?;
    std::fs::write(manifest_path(dir), raw).context("failed to write manifest")?;
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

    fn entity(block: u64, li: u64, value: &str) -> String {
        format!(
            r#"{{"from":"0xaaaa","to":"0xbbbb","value":"{value}","value_hex":"0x0","block_number":{block},"tx_hash":"0xcccc","log_index":{li}}}"#
        )
    }

    #[test]
    fn seals_and_reads_back_a_segment() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![entity(100, 0, "5"), entity(100, 1, "7"), entity(101, 0, "9")];
        let seg = seal_range(dir.path(), &entities, 100, 101).unwrap().unwrap();
        assert_eq!(seg.rows, 3);
        assert_eq!((seg.from_block, seg.to_block), (100, 101));

        // Manifest catalogues exactly one segment.
        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].hash, seg.hash);

        // The Parquet file reads back with the right rows (round-trips through Arrow).
        let path = dir.path().join(SEGMENTS_DIR).join(&seg.file);
        let file = File::open(path).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file).unwrap().build().unwrap();
        let total: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn empty_range_seals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seal_range(dir.path(), &[], 1, 2).unwrap().is_none());
    }

    #[test]
    fn content_address_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![entity(1, 0, "1")];
        let a = seal_range(dir.path(), &entities, 1, 1).unwrap().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let b = seal_range(dir2.path(), &entities, 1, 1).unwrap().unwrap();
        assert_eq!(a.hash, b.hash); // same bytes in → same content address
    }
}
