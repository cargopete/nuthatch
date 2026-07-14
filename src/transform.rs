//! Host side of the WASM transform runtime (ported from liminal, batched-Arrow variant).
//!
//! A transform is a `wasm32-wasip2` component exporting `nuthatch:transform/stage`. The host loads
//! it, grants it ZERO capabilities (base WASI only — stderr for logging, no fs/net/kv), and calls
//! `run` with one Arrow IPC batch, getting a derived Arrow IPC batch back. Capabilities would be
//! granted by *adding imports to the linker*; a component that imports something the linker doesn't
//! carry fails to instantiate — loudly, at load time. Purity is therefore checkable from the
//! component's imports alone (see `wasm-tools component wit`), no code inspection.

use anyhow::{anyhow, Context, Result};
use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "pure-transform",
});

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

/// A loaded pure transform component, ready to run over batches. Cheap to reuse across batches;
/// each `run` gets a fresh store because the component is a stateless pure function.
pub struct TransformRuntime {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
}

impl TransformRuntime {
    pub fn load(wasm: &Path) -> Result<Self> {
        // wasmtime 44 uses its own error type (not anyhow), so map its results explicitly.
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| anyhow!("wasmtime engine: {e}"))?;

        // Base WASI only — no http/kv/filesystem. This is the "zero capabilities" grant.
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("adding base WASI to linker: {e}"))?;

        let component = Component::from_file(&engine, wasm)
            .map_err(|e| anyhow!("loading component {}: {e}", wasm.display()))?;
        Ok(Self { engine, component, linker })
    }

    /// Run the transform over one Arrow IPC batch; returns the derived Arrow IPC batch.
    pub fn run(&self, batch_ipc: &[u8]) -> Result<Vec<u8>> {
        let wasi = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(&self.engine, HostState { wasi, table: ResourceTable::new() });
        let bindings = PureTransform::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| anyhow!("instantiating pure-transform component: {e}"))?;
        bindings
            .nuthatch_transform_stage()
            .call_run(&mut store, batch_ipc)
            .map_err(|e| anyhow!("calling stage.run: {e}"))?
            .map_err(|e| anyhow!("component returned error: {e}"))
    }
}

/// The schema the transform interface speaks: value is Int64 (base units) here, unlike the sealed
/// Parquet schema (which keeps value as a string + hex). Kept deliberately simple for transforms.
fn transform_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt64, false),
        Field::new("from", DataType::Utf8, false),
        Field::new("to", DataType::Utf8, false),
        Field::new("value", DataType::Int64, false),
    ]))
}

/// Build an Arrow IPC batch from stored transfer JSON, ready to hand to a transform. Transfers whose
/// value doesn't fit i64 are dropped (the transform schema is i64).
pub fn transfers_to_ipc(entity_json: &[String]) -> Result<Vec<u8>> {
    let mut blocks = Vec::new();
    let mut logidx = Vec::new();
    let mut from = Vec::new();
    let mut to = Vec::new();
    let mut value = Vec::new();
    for j in entity_json {
        let v: serde_json::Value = serde_json::from_str(j).context("corrupt entity JSON")?;
        let (Some(f), Some(t)) = (v["from"].as_str(), v["to"].as_str()) else { continue };
        let Some(val) = v["value"].as_str().and_then(|s| s.parse::<i64>().ok()) else { continue };
        blocks.push(v["block_number"].as_u64().unwrap_or(0));
        logidx.push(v["log_index"].as_u64().unwrap_or(0));
        from.push(f.to_string());
        to.push(t.to_string());
        value.push(val);
    }
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(blocks)),
        Arc::new(UInt64Array::from(logidx)),
        Arc::new(StringArray::from(from)),
        Arc::new(StringArray::from(to)),
        Arc::new(Int64Array::from(value)),
    ];
    let batch = RecordBatch::try_new(transform_schema(), columns).context("building batch")?;
    batch_to_ipc(&batch)
}

pub fn batch_to_ipc(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut out, &batch.schema()).context("ipc writer")?;
        w.write(batch).context("ipc write")?;
        w.finish().context("ipc finish")?;
    }
    Ok(out)
}

/// Decode an Arrow IPC batch (the transform's output) back into JSON rows for serving/printing.
pub fn ipc_to_json(bytes: &[u8]) -> Result<Vec<serde_json::Value>> {
    let mut reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).context("ipc reader")?;
    let mut rows = Vec::new();
    while let Some(batch) = reader.next() {
        let batch = batch.context("ipc batch")?;
        let blocks = col_u64(&batch, "block_number")?;
        let logidx = col_u64(&batch, "log_index")?;
        let from = col_str(&batch, "from")?;
        let to = col_str(&batch, "to")?;
        let value = col_i64(&batch, "value")?;
        for i in 0..batch.num_rows() {
            rows.push(serde_json::json!({
                "block_number": blocks.value(i),
                "log_index": logidx.value(i),
                "from": from.value(i),
                "to": to.value(i),
                "value": value.value(i),
            }));
        }
    }
    Ok(rows)
}

fn col_u64<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| anyhow!("column {name} missing or not u64"))
}
fn col_i64<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
        .ok_or_else(|| anyhow!("column {name} missing or not i64"))
}
fn col_str<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("column {name} missing or not utf8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(block: u64, li: u64, value: i64) -> String {
        format!(
            r#"{{"from":"0xaaaa","to":"0xbbbb","value":"{value}","value_hex":"0x0","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }

    #[test]
    fn ipc_roundtrips_transfers() {
        let entities = vec![entity(1, 0, 5), entity(1, 1, 2_000_000_000)];
        let ipc = transfers_to_ipc(&entities).unwrap();
        let rows = ipc_to_json(&ipc).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["value"], serde_json::json!(2_000_000_000i64));
    }

    /// End-to-end: load the staged pure component and run it over a batch. Only runs if the wasm
    /// has been built+staged (`components/large-transfers.wasm`); otherwise skipped with a note.
    #[test]
    fn pure_component_filters_large_transfers() {
        let wasm = Path::new("components/large-transfers.wasm");
        if !wasm.exists() {
            eprintln!("skipping: {} not staged (build the guest first)", wasm.display());
            return;
        }
        let rt = TransformRuntime::load(wasm).unwrap();
        // three transfers: 5, 2_000_000_000 (≥1e9), 1_500_000_000 (≥1e9) → two survive the filter.
        let entities = vec![entity(1, 0, 5), entity(1, 1, 2_000_000_000), entity(2, 0, 1_500_000_000)];
        let input = transfers_to_ipc(&entities).unwrap();
        let output = rt.run(&input).unwrap();
        let rows = ipc_to_json(&output).unwrap();
        assert_eq!(rows.len(), 2, "only transfers ≥ 1e9 base units survive");
        assert!(rows.iter().all(|r| r["value"].as_i64().unwrap() >= 1_000_000_000));
    }
}
