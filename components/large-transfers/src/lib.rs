//! `large-transfers` — a pure, batched transform component.
//!
//! Reads a batch of transfers (Arrow IPC), keeps only those with value ≥ a threshold, and returns
//! the filtered batch (Arrow IPC). Zero capabilities: it cannot call out, read the clock, or touch
//! the filesystem — deterministic by construction, so its output is safe to feed entity derivation.
//! The whole batch crosses the wasm boundary in one call (the point of the batched WIT).

use std::io::Cursor;

use arrow_array::{BooleanArray, Int64Array, RecordBatch};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_select::filter::filter_record_batch;

wit_bindgen::generate!({
    world: "pure-transform",
    path: "../../wit",
});

/// 1,000 USDC in base units (6 decimals). A real deployment would parameterise this; the skeleton
/// hardcodes it to keep the component pure and config-free.
const THRESHOLD: i64 = 1_000_000_000;

struct Component;

impl exports::nuthatch::transform::stage::Guest for Component {
    fn run(batch: Vec<u8>) -> Result<Vec<u8>, String> {
        let input = read_batch(&batch)?;

        let value = input
            .column_by_name("value")
            .ok_or("input batch has no `value` column")?
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or("`value` column is not Int64")?;

        let mask = BooleanArray::from_iter(
            value.iter().map(|v| Some(v.map(|x| x >= THRESHOLD).unwrap_or(false))),
        );
        let filtered = filter_record_batch(&input, &mask).map_err(|e| e.to_string())?;

        write_batch(&filtered)
    }
}

fn read_batch(bytes: &[u8]) -> Result<RecordBatch, String> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(|e| e.to_string())?;
    reader
        .next()
        .ok_or("empty Arrow IPC stream")?
        .map_err(|e| e.to_string())
}

fn write_batch(batch: &RecordBatch) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut out, &batch.schema()).map_err(|e| e.to_string())?;
        writer.write(batch).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

export!(Component);
