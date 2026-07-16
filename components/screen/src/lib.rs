//! `screen` — a pure, batched sanctions-screening component (RFC-0008 C2).
//!
//! Given a batch of transfers and a sanctioned-address set (both Arrow IPC), it emits one hit per
//! transfer side that touches the set: if `from` is sanctioned → a hit on the sender side, if `to`
//! is sanctioned → a hit on the recipient side. Zero capabilities: it cannot call a screening API,
//! read the clock, or touch the filesystem — the set it screens against is an *input*, not a lookup.
//! So a hit is reproducible from (list bytes, transfer bytes, component hash) alone: that is the
//! whole audit story. The host stamps the snapshot/component hashes; the component only sees data.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt64Array};
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema};

wit_bindgen::generate!({
    world: "screen-stage",
    path: "../../wit",
});

struct Component;

impl exports::nuthatch::transform::screen::Guest for Component {
    fn run(transfers: Vec<u8>, sanctioned: Vec<u8>) -> Result<Vec<u8>, String> {
        let transfers = read_batch(&transfers)?;
        let list = read_batch(&sanctioned)?;

        // The sanctioned set, normalised lowercase (the host normalises too, so this is defensive).
        let addrs = str_col(&list, "address")?;
        let set: HashSet<String> = (0..list.num_rows())
            .filter(|&i| !addrs.is_null(i))
            .map(|i| addrs.value(i).to_ascii_lowercase())
            .collect();

        let block = u64_col(&transfers, "block_number")?;
        let logidx = u64_col(&transfers, "log_index")?;
        let from = str_col(&transfers, "from")?;
        let to = str_col(&transfers, "to")?;
        let value = str_col(&transfers, "value")?;

        // Emit hits in deterministic order: input row order, sender side before recipient side. This
        // ordering is part of the reproducibility contract (the host keys annotations by side anyway).
        let mut h_block = Vec::new();
        let mut h_log = Vec::new();
        let mut h_addr = Vec::new();
        let mut h_side = Vec::new();
        let mut h_cp = Vec::new();
        let mut h_val = Vec::new();
        for i in 0..transfers.num_rows() {
            let f = from.value(i).to_ascii_lowercase();
            let t = to.value(i).to_ascii_lowercase();
            let mut push = |addr: &str, side: &str, cp: &str| {
                h_block.push(block.value(i));
                h_log.push(logidx.value(i));
                h_addr.push(addr.to_string());
                h_side.push(side.to_string());
                h_cp.push(cp.to_string());
                h_val.push(value.value(i).to_string());
            };
            if set.contains(&f) {
                push(&f, "from", &t);
            }
            if set.contains(&t) {
                push(&t, "to", &f);
            }
        }

        let out = RecordBatch::try_new(
            hits_schema(),
            vec![
                Arc::new(UInt64Array::from(h_block)),
                Arc::new(UInt64Array::from(h_log)),
                Arc::new(StringArray::from(h_addr)),
                Arc::new(StringArray::from(h_side)),
                Arc::new(StringArray::from(h_cp)),
                Arc::new(StringArray::from(h_val)),
            ],
        )
        .map_err(|e| e.to_string())?;
        write_batch(&out)
    }
}

fn hits_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt64, false),
        Field::new("address", DataType::Utf8, false),
        Field::new("side", DataType::Utf8, false),
        Field::new("counterparty", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
    ]))
}

fn read_batch(bytes: &[u8]) -> Result<RecordBatch, String> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(|e| e.to_string())?;
    match reader.next() {
        Some(b) => b.map_err(|e| e.to_string()),
        // An empty stream is legitimate (no transfers, or an empty list) → an empty batch is not an
        // error; but StreamReader yields None, so the caller handles emptiness. We surface it clearly.
        None => Err("empty Arrow IPC stream".to_string()),
    }
}

fn write_batch(batch: &RecordBatch) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut writer =
            StreamWriter::try_new(&mut out, &batch.schema()).map_err(|e| e.to_string())?;
        writer.write(batch).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn u64_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array, String> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| format!("column {name} missing or not u64"))
}

fn str_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray, String> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| format!("column {name} missing or not utf8"))
}

export!(Component);
