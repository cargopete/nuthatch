//! RFC-0018 §2 — the optional Starlark front-end for parameterized, composable nests.
//!
//! A nest may be authored as `nest.star` that *computes* its config (loop over addresses that share
//! an ABI, derive values in code) instead of hand-writing `nuthatch.toml`. It is evaluated
//! hermetically at load time to the **same** `Config` the TOML produces — the core only
//! ever sees the resolved `Config` and never learns which front-end produced it. TOML stays what
//! `init` emits; Starlark is opt-in sugar for authors who want a function, not a fork.
//!
//! Hermetic by construction: no clock, no randomness, no network, no ambient FS. The interpreter runs
//! once at load and is dropped — it is never in the data path (it only produces config), so
//! non-negotiable #4 (determinism in the core) is untouched. Recursion is bounded by starlark's own
//! callstack cap; a `nest.star` is a description, not a program with unbounded loops over host state.
//!
//! ## The bridge
//!
//! The four closed builtins (`nest` / `contract` / `factory` / `template`) each build a Starlark dict
//! whose keys are exactly the serde field names of the matching struct. `nest()` assembles the whole
//! thing into a `serde_json::Value` and `serde_json::from_value`s it into [`Config`] — so the
//! Starlark path and the TOML path deserialize through the *same* serde derives. Round-trip equality
//! (`.star` and equivalent `.toml` → identical `Config`) is therefore structural, not a coincidence
//! we test into existence.

use anyhow::{anyhow, bail, Context, Result};
use std::cell::RefCell;
use std::path::Path;

use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::AllocDict;
use starlark::values::list_or_tuple::UnpackListOrTuple;
use starlark::values::none::NoneType;
use starlark::values::{Heap, Value};

use crate::config::Config;

/// Host-side collector the `nest()` builtin writes its assembled config into (via `Evaluator::extra`).
/// A `nest.star` must call `nest(...)` exactly once; zero or many is an authoring error.
#[derive(ProvidesStaticType, Default)]
struct Collector {
    config: RefCell<Option<serde_json::Value>>,
    calls: RefCell<u32>,
}

/// Evaluate a `nest.star` to a [`Config`] (RFC-0018 §2).
pub fn load_star(path: &Path, _dir: &Path) -> Result<Config> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
    let ast = AstModule::parse(&path.display().to_string(), src, &Dialect::Standard)
        .map_err(|e| anyhow!("nest.star parse error: {e}"))?;

    let globals = GlobalsBuilder::standard().with(nest_builtins).build();
    let collector = Collector::default();
    let eval_result: Result<()> = Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&collector);
        eval.eval_module(ast, &globals)
            .map_err(|e| anyhow!("nest.star error: {e}"))?;
        Ok(())
    });
    eval_result?;

    let calls = *collector.calls.borrow();
    if calls == 0 {
        bail!("nest.star defined no nest — call `nest(name=..., chain=..., rpc_urls=[...])` once");
    }
    if calls > 1 {
        bail!("nest.star called nest() {calls} times — a file defines exactly one nest");
    }
    let json = collector
        .config
        .borrow_mut()
        .take()
        .ok_or_else(|| anyhow!("nest.star produced no config"))?;
    let cfg: Config = serde_json::from_value(json).context(
        "nest.star did not describe a valid nest (a field is missing or the wrong type)",
    )?;
    Ok(cfg)
}

/// Convert a Starlark value to `serde_json::Value` — used to fold builtin outputs into the config JSON.
fn to_json(v: Value) -> Result<serde_json::Value> {
    v.to_json_value()
        .map_err(|e| anyhow!("value is not expressible as config data: {e}"))
}

/// The closed builtin surface. Fixed and non-extensible on purpose: a `nest.star` is not a general
/// program that can reach for arbitrary host power, it is a *description* with exactly four verbs.
#[starlark_module]
fn nest_builtins(builder: &mut GlobalsBuilder) {
    /// A contract to index: `contract(alias, address, abi, start_block=None, events=[])`.
    fn contract<'v>(
        #[starlark(require = named)] alias: String,
        #[starlark(require = named)] address: String,
        #[starlark(require = named)] abi: String,
        #[starlark(require = named, default = NoneType)] start_block: Value<'v>,
        #[starlark(require = named)] events: Option<UnpackListOrTuple<String>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let mut kv: Vec<(&str, Value<'v>)> = vec![
            ("alias", heap.alloc(alias)),
            ("address", heap.alloc(address)),
            ("abi", heap.alloc(abi)),
        ];
        if !start_block.is_none() {
            kv.push(("start_block", start_block));
        }
        let events = events.map(|u| u.items).unwrap_or_default();
        if !events.is_empty() {
            kv.push(("events", heap.alloc(events)));
        }
        Ok(heap.alloc(AllocDict(kv)))
    }

    /// A child-contract template: `template(name, abi, filter=None)`.
    fn template<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(require = named)] abi: String,
        #[starlark(require = named, default = NoneType)] filter: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let mut kv: Vec<(&str, Value<'v>)> =
            vec![("name", heap.alloc(name)), ("abi", heap.alloc(abi))];
        if !filter.is_none() {
            kv.push(("filter", filter));
        }
        Ok(heap.alloc(AllocDict(kv)))
    }

    /// A factory rule: `factory(watch, event, child_param, template, start=None)`.
    fn factory<'v>(
        #[starlark(require = named)] watch: String,
        #[starlark(require = named)] event: String,
        #[starlark(require = named)] child_param: String,
        #[starlark(require = named)] template: String,
        #[starlark(require = named, default = NoneType)] start: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let mut kv: Vec<(&str, Value<'v>)> = vec![
            ("watch", heap.alloc(watch)),
            ("event", heap.alloc(event)),
            ("child_param", heap.alloc(child_param)),
            ("template", heap.alloc(template)),
        ];
        if !start.is_none() {
            kv.push(("start", start));
        }
        Ok(heap.alloc(AllocDict(kv)))
    }

    /// Define the nest. Called exactly once per file; records the assembled config for the host.
    #[allow(clippy::too_many_arguments)]
    fn nest<'v>(
        #[starlark(require = named)] name: String,
        #[starlark(require = named)] chain: String,
        #[starlark(require = named)] rpc_urls: UnpackListOrTuple<String>,
        #[starlark(require = named, default = NoneType)] chain_id: Value<'v>,
        #[starlark(require = named)] contracts: Option<UnpackListOrTuple<Value<'v>>>,
        #[starlark(require = named)] templates: Option<UnpackListOrTuple<Value<'v>>>,
        #[starlark(require = named)] factories: Option<UnpackListOrTuple<Value<'v>>>,
        #[starlark(require = named, default = NoneType)] screening: Value<'v>,
        #[starlark(require = named, default = NoneType)] flags: Value<'v>,
        #[starlark(require = named)] alerts: Option<UnpackListOrTuple<Value<'v>>>,
        #[starlark(require = named)] webhooks: Option<UnpackListOrTuple<Value<'v>>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let collector = eval
            .extra
            .and_then(|e| e.downcast_ref::<Collector>())
            .ok_or_else(|| anyhow!("internal: nest() called outside the nest.star host"))?;
        *collector.calls.borrow_mut() += 1;

        // chain_id is derivable from the chain name (as `init` does); require it explicit only when the
        // chain is unknown, so the Starlark path matches the TOML path exactly.
        let resolved_chain_id: u64 = if chain_id.is_none() {
            crate::chains::lookup(&chain)
                .map(|c| c.chain_id)
                .ok_or_else(|| {
                    anyhow!(
                        "unknown chain {chain:?} — pass chain_id= explicitly for a custom chain"
                    )
                })?
        } else {
            chain_id
                .unpack_i32()
                .map(|i| i as u64)
                .or_else(|| chain_id.to_json_value().ok().and_then(|j| j.as_u64()))
                .ok_or_else(|| anyhow!("chain_id must be an integer"))?
        };

        let mut nest_obj = serde_json::Map::new();
        nest_obj.insert("name".into(), serde_json::Value::String(name));
        nest_obj.insert("chain".into(), serde_json::Value::String(chain));
        nest_obj.insert(
            "chain_id".into(),
            serde_json::Value::from(resolved_chain_id),
        );
        nest_obj.insert("rpc_urls".into(), serde_json::to_value(rpc_urls.items)?);

        let mut root = serde_json::Map::new();
        root.insert("nest".into(), serde_json::Value::Object(nest_obj));
        let collect =
            |vs: Option<UnpackListOrTuple<Value<'v>>>| -> Result<Option<serde_json::Value>> {
                let items = match vs {
                    Some(u) if !u.items.is_empty() => u.items,
                    _ => return Ok(None),
                };
                Ok(Some(serde_json::Value::Array(
                    items.into_iter().map(to_json).collect::<Result<_>>()?,
                )))
            };
        if let Some(v) = collect(contracts)? {
            root.insert("contracts".into(), v);
        }
        if let Some(v) = collect(templates)? {
            root.insert("templates".into(), v);
        }
        if let Some(v) = collect(factories)? {
            root.insert("factories".into(), v);
        }
        if let Some(v) = collect(alerts)? {
            root.insert("alerts".into(), v);
        }
        if let Some(v) = collect(webhooks)? {
            root.insert("webhooks".into(), v);
        }
        if !screening.is_none() {
            root.insert("screening".into(), to_json(screening)?);
        }
        if !flags.is_none() {
            root.insert("flags".into(), to_json(flags)?);
        }

        *collector.config.borrow_mut() = Some(serde_json::Value::Object(root));
        Ok(NoneType)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluate `.star` source in a throwaway dir and hand back the `Config`.
    fn eval(src: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nest.star");
        std::fs::write(&path, src).unwrap();
        load_star(&path, dir.path())
    }

    /// The acceptance bar: a parameterized `.star` and the hand-written `.toml` it stands in for must
    /// deserialize to the *same* `Config`. We compare through `serde_json` because both front-ends
    /// funnel through the identical serde derives — so equality here is structural, not incidental.
    #[test]
    fn star_and_toml_produce_identical_config() {
        let star = r#"
ADDRS = [
    "0x0000000000000000000000000000000000000001",
    "0x0000000000000000000000000000000000000002",
]
nest(
    name = "stables",
    chain = "mainnet",
    rpc_urls = ["https://rpc.example"],
    contracts = [
        contract(alias = "t%d" % i, address = a, abi = "abis/erc20.json")
        for i, a in enumerate(ADDRS)
    ],
)
"#;
        let toml = r#"
[nest]
name = "stables"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "t0"
address = "0x0000000000000000000000000000000000000001"
abi = "abis/erc20.json"

[[contracts]]
alias = "t1"
address = "0x0000000000000000000000000000000000000002"
abi = "abis/erc20.json"
"#;
        let from_star = eval(star).expect("nest.star should evaluate");
        let from_toml: Config = toml::from_str(toml).expect("toml should parse");
        assert_eq!(
            serde_json::to_value(&from_star).unwrap(),
            serde_json::to_value(&from_toml).unwrap(),
            "the loop-authored nest must equal its hand-written TOML twin"
        );
        // And chain_id was *derived* from the chain name, never written in the .star.
        assert_eq!(from_star.nest.chain_id, 1);
        assert_eq!(from_star.contracts.len(), 2);
    }

    /// Factories + templates + an event allowlist survive the bridge untouched.
    #[test]
    fn factory_template_and_events_round_trip() {
        let star = r#"
nest(
    name = "amm",
    chain = "mainnet",
    rpc_urls = ["https://rpc.example"],
    contracts = [
        contract(
            alias = "factory",
            address = "0x0000000000000000000000000000000000000009",
            abi = "abis/factory.json",
            events = ["PoolCreated"],
        ),
    ],
    templates = [template(name = "pool", abi = "abis/pool.json")],
    factories = [
        factory(watch = "factory", event = "PoolCreated", child_param = "pool", template = "pool"),
    ],
)
"#;
        let cfg = eval(star).expect("nest.star with factories should evaluate");
        assert_eq!(cfg.contracts[0].events, vec!["PoolCreated"]);
        assert_eq!(cfg.templates[0].name, "pool");
        assert_eq!(cfg.factories[0].watch, "factory");
        assert_eq!(cfg.factories[0].template, "pool");
    }

    #[test]
    fn nest_called_twice_is_rejected() {
        let src = r#"
nest(name = "a", chain = "mainnet", rpc_urls = ["u"])
nest(name = "b", chain = "mainnet", rpc_urls = ["u"])
"#;
        let err = eval(src).unwrap_err().to_string();
        assert!(err.contains("exactly one nest"), "got: {err}");
    }

    #[test]
    fn nest_never_called_is_rejected() {
        let err = eval("x = 1 + 1\n").unwrap_err().to_string();
        assert!(err.contains("defined no nest"), "got: {err}");
    }

    #[test]
    fn unknown_chain_needs_explicit_chain_id() {
        let err = eval(r#"nest(name = "x", chain = "narnia", rpc_urls = ["u"])"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown chain"), "got: {err}");

        // ...and supplying it explicitly rescues the custom chain.
        let ok = eval(r#"nest(name = "x", chain = "narnia", chain_id = 999, rpc_urls = ["u"])"#)
            .expect("explicit chain_id should satisfy an unknown chain");
        assert_eq!(ok.nest.chain_id, 999);
    }

    #[test]
    fn a_bad_field_type_surfaces_as_an_error() {
        // rpc_urls must be a list of strings — a bare int is a type error, caught at unpack.
        let err = eval(r#"nest(name = "x", chain = "mainnet", rpc_urls = 5)"#)
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty());
    }
}
