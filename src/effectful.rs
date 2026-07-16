//! Host side of the effectful-stage runtime (RFC-0008 C4) — the liminal capability-injection model,
//! adapted to the batched-Arrow boundary.
//!
//! A pure stage (`screen`) gets zero capabilities. An **effectful** stage may import host-provided
//! capabilities — a `kv` store here (outbound HTTP arrives in C5) — but only what it is **granted**.
//! Two enforcement layers: (1) the host reads the component's *actual imports* and refuses to load one
//! whose imports exceed its declared grant, with a clear error (no code inspection — the capability
//! requirement is in the component's type); (2) the linker is wired with *only* the granted
//! capabilities, so even if the check were bypassed, an ungranted import fails to instantiate. Grants
//! come from the host (the pack manifest in C6), never from the component. An effectful stage's only
//! output is an annotation batch the host records — it has no import that could write canonical
//! entities, so "annotations only" is enforced by the absence of the capability, not by convention.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

wasmtime::component::bindgen!({
    path: "wit",
    world: "effectful-kv",
});

// `self::` disambiguates the bindgen-generated `nuthatch` module from the crate named `nuthatch`
// (the two collide only in the doctest harness, where the crate is an `--extern nuthatch`).
use self::nuthatch::transform::kv;

/// The capabilities an effectful component may be granted. Absent capability → the component may not
/// import it. `http_hosts` is the outbound-HTTP allowlist wired in C5; declared here so the grant
/// model and the import check already understand it.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    pub kv: bool,
    pub http_hosts: Vec<String>,
}

impl Grants {
    /// Grant only the key-value capability.
    pub fn kv() -> Self {
        Self {
            kv: true,
            http_hosts: Vec::new(),
        }
    }
}

/// A host-owned key-value store shared across a component's batch runs, so an effectful stage can keep
/// state (running counts, seen-before markers) between batches. The host owns it entirely.
type KvStore = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    kv: KvStore,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl kv::Host for HostState {
    fn get(&mut self, key: String) -> Option<Vec<u8>> {
        self.kv.lock().unwrap().get(&key).cloned()
    }

    fn set(&mut self, key: String, value: Vec<u8>) {
        self.kv.lock().unwrap().insert(key, value);
    }
}

/// A loaded effectful component, its content hash, and the capabilities it was granted. Reusable
/// across batches; kv state persists in `kv`.
pub struct EffectfulRuntime {
    engine: Engine,
    component: Component,
    linker: Linker<HostState>,
    component_hash: String,
    kv: KvStore,
}

impl EffectfulRuntime {
    /// Load an effectful component under `grants`. **Refuses** a component whose imports exceed the
    /// grant (over-privileged), then wires a linker carrying exactly the granted capabilities.
    pub fn load(wasm: &Path, grants: Grants) -> Result<Self> {
        let bytes = std::fs::read(wasm)
            .with_context(|| format!("cannot read effectful component {}", wasm.display()))?;
        Self::from_bytes(&bytes, grants)
    }

    pub fn from_bytes(bytes: &[u8], grants: Grants) -> Result<Self> {
        let component_hash = hex::encode(Sha256::digest(bytes));

        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).map_err(|e| anyhow!("wasmtime engine: {e}"))?;
        let component = Component::from_binary(&engine, bytes)
            .map_err(|e| anyhow!("loading effectful component: {e}"))?;

        // Enforcement layer 1: actual imports must not exceed the declared grant.
        check_imports(&component, &engine, &grants)?;

        // Enforcement layer 2: the linker carries only base WASI + the granted capabilities. An
        // ungranted capability the check somehow missed would fail to instantiate here.
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow!("adding base WASI to linker: {e}"))?;
        if grants.kv {
            kv::add_to_linker::<_, HasSelf<_>>(&mut linker, |s: &mut HostState| s)
                .map_err(|e| anyhow!("granting kv capability: {e}"))?;
        }

        Ok(Self {
            engine,
            component,
            linker,
            component_hash,
            kv: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn component_hash(&self) -> &str {
        &self.component_hash
    }

    /// Run the effectful stage over one Arrow IPC batch; returns the annotation rows it produced.
    /// kv state persists across calls on this runtime.
    pub fn run(&self, batch_ipc: &[u8]) -> Result<Vec<Value>> {
        let wasi = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi,
                table: ResourceTable::new(),
                kv: self.kv.clone(),
            },
        );
        let bindings = EffectfulKv::instantiate(&mut store, &self.component, &self.linker)
            .map_err(|e| anyhow!("instantiating effectful component: {e}"))?;
        let out = bindings
            .nuthatch_transform_effectful()
            .call_run(&mut store, batch_ipc)
            .map_err(|e| anyhow!("calling effectful.run: {e}"))?
            .map_err(|e| anyhow!("effectful component returned error: {e}"))?;
        ipc_to_json(&out)
    }
}

/// Is this import a *grantable* host capability (as opposed to base WASI, which every component gets)?
/// Returns the grant it requires, or `None` for always-allowed base WASI imports.
fn required_grant(import_name: &str) -> Option<Capability> {
    if import_name.starts_with("nuthatch:transform/kv") {
        Some(Capability::Kv)
    } else if import_name.starts_with("wasi:http") {
        Some(Capability::Http)
    } else {
        None
    }
}

enum Capability {
    Kv,
    Http,
}

/// Refuse a component whose actual imports exceed its declared grant. This is the audit artifact: the
/// capability requirement is checkable from the component's type, so an operator (or `pack verify` in
/// C6) can confirm what a stage may do without trusting its source.
fn check_imports(component: &Component, engine: &Engine, grants: &Grants) -> Result<()> {
    for (name, _item) in component.component_type().imports(engine) {
        match required_grant(name) {
            Some(Capability::Kv) if !grants.kv => {
                bail!("component imports '{name}' but the `kv` capability was not granted")
            }
            Some(Capability::Http) if grants.http_hosts.is_empty() => {
                bail!("component imports '{name}' but no outbound HTTP hosts were granted")
            }
            _ => {}
        }
    }
    Ok(())
}

/// Decode an effectful stage's annotation output (Arrow IPC) into JSON rows. Generic over the schema:
/// UInt64 columns become numbers, everything else its string form — the host records whatever the
/// stage annotated without needing to know the stage's exact schema.
fn ipc_to_json(bytes: &[u8]) -> Result<Vec<Value>> {
    use arrow::array::{Array, StringArray, UInt64Array};
    use arrow::ipc::reader::StreamReader;

    let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).context("ipc reader")?;
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.context("ipc batch")?;
        let names: Vec<String> = batch
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect();
        for i in 0..batch.num_rows() {
            let mut obj = Map::new();
            for (c, name) in names.iter().enumerate() {
                let col = batch.column(c);
                let v = if let Some(u) = col.as_any().downcast_ref::<UInt64Array>() {
                    json!(u.value(i))
                } else if let Some(s) = col.as_any().downcast_ref::<StringArray>() {
                    json!(s.value(i))
                } else {
                    json!(null)
                };
                obj.insert(name.clone(), v);
            }
            rows.push(Value::Object(obj));
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, RecordBatch, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    const RECURRENCE_WASM: &str = "components/recurrence.wasm";

    fn addresses_ipc(addrs: &[&str]) -> Vec<u8> {
        let col: ArrayRef = Arc::new(StringArray::from(
            addrs.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ));
        let schema = Arc::new(Schema::new(vec![Field::new(
            "address",
            DataType::Utf8,
            false,
        )]));
        let batch = RecordBatch::try_new(schema, vec![col]).unwrap();
        crate::transform::batch_to_ipc(&batch).unwrap()
    }

    fn staged() -> Option<Vec<u8>> {
        let p = Path::new(RECURRENCE_WASM);
        if !p.exists() {
            eprintln!("skipping: {RECURRENCE_WASM} not staged");
            return None;
        }
        Some(std::fs::read(p).unwrap())
    }

    /// Gate 1: a component importing an ungranted capability is refused at load — with a clear error,
    /// before instantiation. The recurrence component imports `kv`; loading it with no grant fails.
    #[test]
    fn rejects_component_with_ungranted_capability() {
        let Some(bytes) = staged() else { return };
        let res = EffectfulRuntime::from_bytes(&bytes, Grants::default());
        assert!(
            res.is_err(),
            "loading a kv-importing component with no grant must fail"
        );
        let msg = format!("{:#}", res.err().unwrap());
        assert!(
            msg.contains("kv") && msg.contains("not granted"),
            "unexpected error: {msg}"
        );
    }

    /// Gate 2: with `kv` granted, the effectful component runs, produces annotations, and its kv state
    /// persists across batches (the whole point of an effectful stage — a pure one couldn't remember).
    #[test]
    fn runs_kv_granted_component_producing_annotations() {
        let Some(bytes) = staged() else { return };
        let rt = EffectfulRuntime::from_bytes(&bytes, Grants::kv()).unwrap();

        // First batch: alice seen once, bob once.
        let out = rt.run(&addresses_ipc(&["0xalice", "0xbob"])).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["address"], "0xalice");
        assert_eq!(out[0]["seen"], json!(1));
        assert_eq!(out[1]["seen"], json!(1));

        // Second batch: alice again → seen 2 (kv persisted); carol first time → 1.
        let out = rt.run(&addresses_ipc(&["0xalice", "0xcarol"])).unwrap();
        assert_eq!(
            out[0]["seen"],
            json!(2),
            "kv state must persist across batches"
        );
        assert_eq!(out[1]["seen"], json!(1));
        assert!(!rt.component_hash().is_empty());
    }
}
