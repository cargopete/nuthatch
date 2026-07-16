//! Host side of the sanctions-screening stage (RFC-0008 C2).
//!
//! Loads the pure `screen` component (`wasm32-wasip2`, exporting `nuthatch:transform/screen`), grants
//! it ZERO capabilities (base WASI only), and calls `run` with two Arrow IPC batches — the transfers
//! and the sanctioned-address set — getting the hits back as a third. Because both the set and the
//! transfers are *inputs*, and the component imports nothing but base WASI, a hit is reproducible from
//! `(list-snapshot bytes, transfer bytes, component hash)` alone. The host stamps each hit with the
//! snapshot hash and the component's own content hash; the component never sees either, so the audit
//! trail is assembled from data the sandbox cannot forge. This is the "prove it" core of the pack.

use anyhow::{anyhow, Context, Result};
use arrow::array::{ArrayRef, RecordBatch, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::transform::batch_to_ipc;

wasmtime::component::bindgen!({
    path: "wit",
    world: "screen-stage",
});

/// A nest-local override path for the screening component. If a nest ships its own
/// `components/screen.wasm`, that is used (and content-hashed) in preference to the embedded default —
/// so an operator can pin a specific reviewed component. Otherwise the embedded one is used.
pub const SCREEN_WASM: &str = "components/screen.wasm";

/// The pure screening component, embedded so it always travels inside the single binary (the
/// non-negotiable) with a deterministic content hash. The committed file is inspectable out-of-band
/// (`wasm-tools component wit components/screen.wasm` → base WASI imports only).
const EMBEDDED_SCREEN: &[u8] = include_bytes!("../components/screen.wasm");

/// Load the screening runtime for a nest: prefer a nest-local `components/screen.wasm` (operator pin),
/// else the embedded component. Either way the bytes are content-hashed for the audit trail.
pub fn load_runtime(dir: &Path) -> Result<ScreenRuntime> {
    let local = dir.join(SCREEN_WASM);
    if local.exists() {
        ScreenRuntime::load(&local)
    } else {
        ScreenRuntime::embedded()
    }
}

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// One screening hit: a transfer side that touched the sanctioned set. `address` is the sanctioned
/// address that matched; `side` is which end of the transfer it was on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanctionHit {
    pub block_number: u64,
    pub log_index: u64,
    pub address: String,
    pub side: String,
    pub counterparty: String,
    pub value: String,
}

/// A loaded pure screening component, plus its content hash (the `source` stamped on every hit).
pub struct ScreenRuntime {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    /// sha256 of the component bytes — the reproducibility anchor recorded on each annotation.
    component_hash: String,
}

impl ScreenRuntime {
    /// Load a screening component from a file (a nest-local operator pin).
    pub fn load(wasm: &Path) -> Result<Self> {
        let bytes = std::fs::read(wasm)
            .with_context(|| format!("cannot read screening component {}", wasm.display()))?;
        Self::from_bytes(&bytes)
    }

    /// Load the embedded pure screening component (the default — always available in the binary).
    pub fn embedded() -> Result<Self> {
        Self::from_bytes(EMBEDDED_SCREEN)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let component_hash = hex::encode(Sha256::digest(bytes));

        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| anyhow!("wasmtime engine: {e}"))?;

        // Base WASI only — the "zero capabilities" grant. A component importing anything else fails
        // to instantiate, loudly, at load time.
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("adding base WASI to linker: {e}"))?;

        let component = Component::from_binary(&engine, bytes)
            .map_err(|e| anyhow!("loading screening component: {e}"))?;
        Ok(Self {
            engine,
            component,
            linker,
            component_hash,
        })
    }

    /// The component's content hash — the `source` recorded on annotations it produces.
    pub fn component_hash(&self) -> &str {
        &self.component_hash
    }

    /// Screen a transfers batch against a sanctioned-address set; returns the hits. Both inputs are
    /// Arrow IPC. Pure and deterministic: identical inputs → identical hits, in a stable order.
    pub fn run(&self, transfers_ipc: &[u8], sanctioned_ipc: &[u8]) -> Result<Vec<SanctionHit>> {
        let wasi = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi,
                table: ResourceTable::new(),
            },
        );
        let bindings = ScreenStage::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| anyhow!("instantiating screen component: {e}"))?;
        let out = bindings
            .nuthatch_transform_screen()
            .call_run(&mut store, transfers_ipc, sanctioned_ipc)
            .map_err(|e| anyhow!("calling screen.run: {e}"))?
            .map_err(|e| anyhow!("screen component returned error: {e}"))?;
        hits_from_ipc(&out)
    }
}

/// The transfers schema the screening interface speaks. `value` is text (base units) so an i128 value
/// crosses the boundary without loss — screening never does arithmetic on it, only carries it.
fn transfers_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt64, false),
        Field::new("from", DataType::Utf8, false),
        Field::new("to", DataType::Utf8, false),
        Field::new("value", DataType::Utf8, false),
    ]))
}

/// Build a transfers Arrow IPC batch from `(block, log_index, from, to, value)` tuples for screening.
pub fn transfers_to_ipc(rows: &[(u64, u64, String, String, String)]) -> Result<Vec<u8>> {
    let blocks: Vec<u64> = rows.iter().map(|r| r.0).collect();
    let logidx: Vec<u64> = rows.iter().map(|r| r.1).collect();
    let from: Vec<String> = rows.iter().map(|r| r.2.clone()).collect();
    let to: Vec<String> = rows.iter().map(|r| r.3.clone()).collect();
    let value: Vec<String> = rows.iter().map(|r| r.4.clone()).collect();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(blocks)),
        Arc::new(UInt64Array::from(logidx)),
        Arc::new(StringArray::from(from)),
        Arc::new(StringArray::from(to)),
        Arc::new(StringArray::from(value)),
    ];
    let batch =
        RecordBatch::try_new(transfers_schema(), columns).context("building transfers batch")?;
    batch_to_ipc(&batch)
}

/// Build a sanctioned-address Arrow IPC batch (single `address` column) from a list snapshot.
pub fn list_to_ipc(addresses: &[String]) -> Result<Vec<u8>> {
    let col: ArrayRef = Arc::new(StringArray::from(addresses.to_vec()));
    let schema = Arc::new(Schema::new(vec![Field::new(
        "address",
        DataType::Utf8,
        false,
    )]));
    let batch = RecordBatch::try_new(schema, vec![col]).context("building list batch")?;
    batch_to_ipc(&batch)
}

/// Decode the component's hit output (Arrow IPC) into [`SanctionHit`]s.
fn hits_from_ipc(bytes: &[u8]) -> Result<Vec<SanctionHit>> {
    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).context("ipc reader")?;
    let mut hits = Vec::new();
    for batch in reader {
        let batch = batch.context("ipc batch")?;
        let block = u64_col(&batch, "block_number")?;
        let logidx = u64_col(&batch, "log_index")?;
        let addr = str_col(&batch, "address")?;
        let side = str_col(&batch, "side")?;
        let cp = str_col(&batch, "counterparty")?;
        let value = str_col(&batch, "value")?;
        for i in 0..batch.num_rows() {
            hits.push(SanctionHit {
                block_number: block.value(i),
                log_index: logidx.value(i),
                address: addr.value(i).to_string(),
                side: side.value(i).to_string(),
                counterparty: cp.value(i).to_string(),
                value: value.value(i).to_string(),
            });
        }
    }
    Ok(hits)
}

/// The annotation-fact JSON a hit becomes in the store: an append-only `sanction_hit` row, stamped
/// with the list snapshot and component hashes (the reproducibility anchors). Keyed distinctly from
/// the triggering transfer (see [`annotation_key`]) so both survive in the same store.
pub fn hit_to_annotation(
    hit: &SanctionHit,
    tx_hash: &str,
    list_snapshot: &str,
    source: &str,
) -> Value {
    json!({
        "table": "sanction_hit",
        "kind": "sanction_hit",
        "block_number": hit.block_number,
        "log_index": hit.log_index,
        "tx_hash": tx_hash,
        "address": hit.address,
        "side": hit.side,
        "counterparty": hit.counterparty,
        "value": hit.value,
        "list_snapshot": list_snapshot,
        "source": source,
    })
}

/// The hot-store key for a sanction-hit annotation: the triggering transfer's `{block}-{log_index}`
/// plus the side and an 8-char list-snapshot prefix. Deterministic (so a replay reproduces the exact
/// same key), unique (a transfer can hit on both sides and against multiple lists), and block-prefixed
/// so it rolls back and prunes with its block exactly like any other row.
pub fn annotation_key(hit: &SanctionHit, list_snapshot: &str) -> String {
    let short = &list_snapshot[..list_snapshot.len().min(8)];
    format!(
        "{:012}-{:06}-{}-{}",
        hit.block_number, hit.log_index, hit.side, short
    )
}

/// A transfer to screen: chain coordinates + endpoints + value + tx. `tx_hash` is carried host-side
/// (the component doesn't need it) and stamped onto any resulting annotation.
#[derive(Debug, Clone)]
pub struct TransferRow {
    pub block_number: u64,
    pub log_index: u64,
    pub from: String,
    pub to: String,
    pub value: String,
    pub tx_hash: String,
}

/// Screen a batch of transfers against one loaded list snapshot, returning `(annotation_key,
/// annotation_json)` per hit — ready to store. The heavy lifting (set membership) happens in the pure
/// component; the host only builds the batches, maps hits back to their tx hash, and stamps the
/// snapshot/component hashes. Empty in, empty out (no component call when there's nothing to screen).
pub fn screen_batch(
    rt: &ScreenRuntime,
    transfers: &[TransferRow],
    list_snapshot: &str,
    addresses: &[String],
) -> Result<Vec<(String, Value)>> {
    if transfers.is_empty() || addresses.is_empty() {
        return Ok(Vec::new());
    }
    let tuples: Vec<(u64, u64, String, String, String)> = transfers
        .iter()
        .map(|t| {
            (
                t.block_number,
                t.log_index,
                t.from.clone(),
                t.to.clone(),
                t.value.clone(),
            )
        })
        .collect();
    let tx_by: std::collections::HashMap<(u64, u64), &str> = transfers
        .iter()
        .map(|t| ((t.block_number, t.log_index), t.tx_hash.as_str()))
        .collect();

    let hits = rt.run(&transfers_to_ipc(&tuples)?, &list_to_ipc(addresses)?)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in &hits {
        let tx = tx_by
            .get(&(h.block_number, h.log_index))
            .copied()
            .unwrap_or("");
        out.push((
            annotation_key(h, list_snapshot),
            hit_to_annotation(h, tx, list_snapshot, rt.component_hash()),
        ));
    }
    Ok(out)
}

/// The live screening stage: the loaded pure component plus the list snapshots configured in
/// `[screening]`. Held by the indexer and run over each window's transfers. When no lists are
/// configured (or the component isn't staged), `from_config` returns `None` and screening is simply
/// absent — the deterministic core is unchanged (RFC-0008 rule 1: it must run with effectful stages
/// off, and this pure stage is likewise fully optional).
pub struct LiveScreener {
    runtime: ScreenRuntime,
    lists: Vec<(String, Vec<String>)>,
}

impl LiveScreener {
    /// Build the live screener from a nest's `[screening].lists`. `None` when screening is off. Errors
    /// only when a configured list snapshot is missing (a misconfiguration worth surfacing loudly).
    pub fn from_config(dir: &Path, lists: &[String]) -> Result<Option<Self>> {
        if lists.is_empty() {
            return Ok(None);
        }
        let runtime = load_runtime(dir)?;
        let mut loaded = Vec::new();
        for hash in lists {
            let addrs = crate::lists::load(dir, hash)
                .with_context(|| format!("screening list snapshot {hash} not found"))?;
            loaded.push((hash.clone(), addrs));
        }
        let total: usize = loaded.iter().map(|(_, a)| a.len()).sum();
        tracing::info!(
            "screening enabled: {} list(s), {} sanctioned address(es), component {}…",
            loaded.len(),
            total,
            &runtime.component_hash()[..12]
        );
        Ok(Some(Self {
            runtime,
            lists: loaded,
        }))
    }

    /// Screen one window's transfers against every configured list, returning all annotations to store.
    pub fn screen_window(&self, transfers: &[TransferRow]) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        for (hash, addrs) in &self.lists {
            match screen_batch(&self.runtime, transfers, hash, addrs) {
                Ok(anns) => out.extend(anns),
                Err(e) => tracing::warn!(
                    "screening against {}… failed: {e:#}",
                    &hash[..12.min(hash.len())]
                ),
            }
        }
        out
    }

    /// The staged component's content hash — recorded for the audit trail / `pack verify` later.
    pub fn component_hash(&self) -> &str {
        self.runtime.component_hash()
    }
}

/// Read sealed transfers in `[from, to]` from the analytical (DuckDB) surface, as [`TransferRow`]s —
/// the backfill screening input. `transfer_tables` gives each transfer table with its (from, to,
/// value) column names (registry-derived; they vary by token), so the query is never user text.
pub fn read_sealed_transfers(
    dir: &Path,
    transfer_tables: &[(String, String, String, String)],
    from: u64,
    to: u64,
) -> Result<Vec<TransferRow>> {
    let mut rows = Vec::new();
    for (table, from_col, to_col, val_col) in transfer_tables {
        let sql = format!(
            "SELECT block_number, log_index, tx_hash, lower(\"{from_col}\") AS f, lower(\"{to_col}\") AS t, \
             CAST(\"{val_col}\" AS VARCHAR) AS v FROM \"{table}\" \
             WHERE block_number BETWEEN {from} AND {to} ORDER BY block_number, log_index"
        );
        // Best-effort per table: a table with no sealed segment yet has no view — skip it.
        let result = match crate::analytics::query(dir, &sql) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("no sealed transfers for {table}: {e:#}");
                continue;
            }
        };
        for r in result {
            let (Some(block), Some(log), Some(f), Some(t)) = (
                r["block_number"].as_u64(),
                r["log_index"].as_u64(),
                r["f"].as_str(),
                r["t"].as_str(),
            ) else {
                continue;
            };
            rows.push(TransferRow {
                block_number: block,
                log_index: log,
                from: f.to_string(),
                to: t.to_string(),
                value: r["v"].as_str().unwrap_or("0").to_string(),
                tx_hash: r["tx_hash"].as_str().unwrap_or("").to_string(),
            });
        }
    }
    rows.sort_by(|a, b| {
        a.block_number
            .cmp(&b.block_number)
            .then_with(|| a.log_index.cmp(&b.log_index))
    });
    Ok(rows)
}

/// Registry-derived (table, from_col, to_col, value_col) for every transfer-shaped table in a nest.
pub fn transfer_tables(
    registry: &crate::registry::DecodeRegistry,
) -> Vec<(String, String, String, String)> {
    registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, t, v)| (d.table.clone(), f.to_string(), t.to_string(), v.to_string()))
        })
        .collect()
}

/// `nuthatch screen --list <hash> --from --to` (RFC-0008 C2): screen the *sealed* transfers in a
/// range against a list snapshot with the pure component, and seal the resulting `sanction_hit`
/// annotations to their own Parquet table. This is the audit-grade path: it re-runs over immutable
/// segments, so `(list hash, block range, component hash)` reproduces byte-identical hits — the
/// "prove it" command. Idempotent (segment sealing is content-addressed).
pub fn backfill(args: crate::cli::ScreenArgs) -> Result<()> {
    let dir = std::path::PathBuf::from(&args.dir);
    let config = crate::config::Config::load(&dir)?;
    let registry = crate::registry::DecodeRegistry::from_nest(&dir, &config)?;
    let tables = transfer_tables(&registry);
    if tables.is_empty() {
        anyhow::bail!("this nest has no transfer-shaped tables to screen");
    }
    let addresses = crate::lists::load(&dir, &args.list)?;
    let rt = load_runtime(&dir)?;

    let transfers = read_sealed_transfers(&dir, &tables, args.from, args.to)?;
    let annotations = screen_batch(&rt, &transfers, &args.list, &addresses)?;

    if !annotations.is_empty() {
        let jsons: Vec<String> = annotations.iter().map(|(_, v)| v.to_string()).collect();
        crate::seal::seal_range(&dir, &jsons, args.from, args.to)?;
    }
    let short = &args.list[..12.min(args.list.len())];
    println!(
        "✓ screened {} sealed transfer(s) over {}..={} against list {short}… (component {}…)",
        transfers.len(),
        args.from,
        args.to,
        &rt.component_hash()[..12]
    );
    println!(
        "  → {} sanction_hit annotation(s) sealed. Query: /sql?q=SELECT * FROM sanction_hit",
        annotations.len()
    );
    Ok(())
}

fn u64_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .ok_or_else(|| anyhow!("column {name} missing or not u64"))
}

fn str_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| anyhow!("column {name} missing or not utf8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged_component() -> Option<ScreenRuntime> {
        let wasm = Path::new(SCREEN_WASM);
        if !wasm.exists() {
            eprintln!("skipping: {SCREEN_WASM} not staged (build the guest first)");
            return None;
        }
        Some(ScreenRuntime::load(wasm).unwrap())
    }

    /// Golden test (RFC-0008 C2 gate): a fixture list + fixture transfers → the exact hits, in the
    /// exact order. Only runs when the pure component is staged.
    #[test]
    fn screens_transfers_against_a_fixture_list() {
        let Some(rt) = staged_component() else { return };
        let sanctioned = "0x1111111111111111111111111111111111111111"; // the "bad" address
        let clean_a = "0x00000000000000000000000000000000000000aa";
        let clean_b = "0x00000000000000000000000000000000000000bb";

        let transfers = vec![
            // clean → sanctioned : a hit on the recipient side.
            (
                10,
                0,
                clean_a.to_string(),
                sanctioned.to_string(),
                "100".to_string(),
            ),
            // sanctioned → clean : a hit on the sender side.
            (
                10,
                1,
                sanctioned.to_string(),
                clean_b.to_string(),
                "250".to_string(),
            ),
            // clean → clean : no hit.
            (
                11,
                0,
                clean_a.to_string(),
                clean_b.to_string(),
                "7".to_string(),
            ),
        ];
        let hits = rt
            .run(
                &transfers_to_ipc(&transfers).unwrap(),
                &list_to_ipc(&[sanctioned.to_string()]).unwrap(),
            )
            .unwrap();

        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            SanctionHit {
                block_number: 10,
                log_index: 0,
                address: sanctioned.into(),
                side: "to".into(),
                counterparty: clean_a.into(),
                value: "100".into(),
            }
        );
        assert_eq!(hits[1].side, "from");
        assert_eq!(hits[1].counterparty, clean_b);
        assert_eq!(hits[1].value, "250");
    }

    /// The C2 replay gate: screening the **live** transfers and screening the same transfers read
    /// back from **sealed Parquet** produce identical annotations (keys + content). This is the audit
    /// guarantee — the backfill `nuthatch screen` command reproduces exactly what the live stage
    /// recorded, over immutable segments. Uses a value > i64::MAX to prove the text path is loss-free.
    #[test]
    fn live_and_backfill_screening_agree() {
        let Some(rt) = staged_component() else { return };
        let dir = tempfile::tempdir().unwrap();
        let bad = "0x1111111111111111111111111111111111111111";
        let clean = "0x00000000000000000000000000000000000000aa";
        let big = "100000000000000000000"; // > i64::MAX

        // The live transfers the indexer would have screened this window.
        let live_transfers = vec![
            TransferRow {
                block_number: 20,
                log_index: 0,
                from: clean.into(),
                to: bad.into(),
                value: big.into(),
                tx_hash: "0xtx1".into(),
            },
            TransferRow {
                block_number: 20,
                log_index: 1,
                from: bad.into(),
                to: clean.into(),
                value: "5".into(),
                tx_hash: "0xtx2".into(),
            },
            TransferRow {
                block_number: 21,
                log_index: 0,
                from: clean.into(),
                to: clean.into(),
                value: "9".into(),
                tx_hash: "0xtx3".into(),
            },
        ];
        let live = screen_batch(&rt, &live_transfers, "cafef00d1234", &[bad.to_string()]).unwrap();

        // Seal the same transfers as a t__transfer segment, then read them back and screen those.
        let sealed_json: Vec<String> = live_transfers
            .iter()
            .map(|t| {
                json!({
                    "table": "t__transfer", "from": t.from, "to": t.to, "value": t.value,
                    "block_number": t.block_number, "log_index": t.log_index, "tx_hash": t.tx_hash,
                })
                .to_string()
            })
            .collect();
        crate::seal::seal_range(dir.path(), &sealed_json, 20, 21).unwrap();

        let read_back = read_sealed_transfers(
            dir.path(),
            &[(
                "t__transfer".into(),
                "from".into(),
                "to".into(),
                "value".into(),
            )],
            20,
            21,
        )
        .unwrap();
        let backfill = screen_batch(&rt, &read_back, "cafef00d1234", &[bad.to_string()]).unwrap();

        assert_eq!(
            live, backfill,
            "live and backfill screening must produce identical annotations"
        );
        assert_eq!(
            live.len(),
            2,
            "two hits (recipient-side big, sender-side 5)"
        );
        // The big value survived the round-trip through Parquet + the Arrow text boundary intact.
        assert!(live.iter().any(|(_, a)| a["value"] == big));
    }

    /// A transfer between two sanctioned addresses yields two hits (both sides), deterministically
    /// ordered from-before-to — the ordering the annotation keys rely on being stable.
    #[test]
    fn both_sides_sanctioned_yields_two_hits() {
        let Some(rt) = staged_component() else { return };
        let a = "0x1111111111111111111111111111111111111111";
        let b = "0x2222222222222222222222222222222222222222";
        let hits = rt
            .run(
                &transfers_to_ipc(&[(5, 3, a.to_string(), b.to_string(), "9".to_string())])
                    .unwrap(),
                &list_to_ipc(&[a.to_string(), b.to_string()]).unwrap(),
            )
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].side, "from");
        assert_eq!(hits[1].side, "to");
        // Distinct, stable annotation keys for the two hits at the same log.
        assert_ne!(
            annotation_key(&hits[0], "deadbeefcafe"),
            annotation_key(&hits[1], "deadbeefcafe")
        );
    }
}
