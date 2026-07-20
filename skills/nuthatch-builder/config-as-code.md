# Config as code: `nest.star` (Starlark front-end)

Most nests are a `nuthatch.toml` — that's what `init` writes and what you should reach for by
default. But when a nest's config is *repetitive* — fifty pools that share one ABI, a basket of
tokens configured identically — hand-maintaining TOML is a chore and a drift risk. RFC-0018 §2 adds
an **optional** second front-end: a `nest.star` file that
*computes* the config in [Starlark](https://github.com/facebook/starlark-rust) (a small, hermetic
Python dialect) and evaluates to the **exact same `Config`** the TOML would.

**This is sugar, not a new capability.** A `nest.star` can express nothing a `nuthatch.toml` can't —
it deserializes through the identical serde model, so a `.star` and its equivalent `.toml` produce a
byte-identical config. Prefer TOML unless a loop or a composition genuinely earns the Starlark.

## When it takes precedence

If a nest dir contains **`nest.star`, it is used and `nuthatch.toml` is ignored.** Don't ship both
expecting a merge — there is none. Keep one front-end per nest.

## The four verbs (the whole surface)

The evaluation environment is *closed*: standard Starlark (lists, dicts, comprehensions, `%`
formatting, `enumerate`, `range`, defs) plus exactly four host builtins. There is no file, clock,
network, or randomness access — a `nest.star` is a description, not a program.

- **`nest(...)`** — call **exactly once** per file. It defines the nest and its arguments map 1:1 to
  the TOML sections:
  - `name` (str), `chain` (str), `rpc_urls` (list[str]) — required.
  - `chain_id` (int) — **optional**; for a known chain it is derived from `chain` exactly as `init`
    does. Pass it explicitly only for a custom chain nuthatch doesn't know.
  - `contracts`, `templates`, `factories`, `alerts`, `webhooks` — lists of the builtins below.
  - `screening`, `flags` — dicts matching the `[screening]` / `[flags]` TOML tables.
- **`contract(alias, address, abi, start_block=None, events=[])`** — one contract to index. `events`
  is the optional per-contract event allowlist (same as `[[contracts]].events`).
- **`template(name, abi, filter=None)`** — a child-contract template (factory pattern, RFC-0009).
- **`factory(watch, event, child_param, template, start=None)`** — a factory discovery rule.

All arguments are **keyword-only** — `contract(alias="usdc", ...)`, never `contract("usdc", ...)`.

## The canonical win: a loop instead of copy-paste

```python
# nest.star — index a basket of ERC-20s that all share one ABI
STABLES = {
    "usdc": "0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48",
    "usdt": "0xdAC17F958D2ee523a2206206994597C13D831ec7",
    "dai":  "0x6B175474E89094C44Da98b954EedeAC495271d0F",
}

nest(
    name = "stables",
    chain = "mainnet",
    rpc_urls = ["https://eth.example/rpc"],
    contracts = [
        contract(alias = name, address = addr, abi = "abis/erc20.json")
        for name, addr in STABLES.items()
    ],
)
```

That is the whole file. Adding a fourth stablecoin is one line, and every contract is guaranteed to
share the same ABI and settings — the drift a hand-written three-contract TOML invites is gone.

## Rules that keep it honest

1. **One `nest()` call.** Zero calls or two calls is an error, caught at load with a clear message.
2. **Keyword arguments only**, and only the fields listed above — an unknown field is a load error,
   not a silent no-op.
3. **It resolves to a `Config`, then stops.** The interpreter runs once at `nuthatch dev` / mount time
   and is dropped; it never touches the data path, so determinism in the core is untouched.
4. **Everything downstream is identical.** `pack`, mount, semantic layer, views — all operate on the
   resolved config and neither know nor care that it came from Starlark.

If you find yourself wanting logic a `nest.star` can't express, that's the signal you're reaching past
config into *transform* territory — that's the WASM/handler layer, not this one.
