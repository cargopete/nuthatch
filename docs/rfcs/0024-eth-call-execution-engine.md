# RFC-0024: The eth_call execution engine — a demand-driven state cache, not an archive node

- Status: **Draft** (2026-07-22)
- Author: Pete (cargopete)
- Date: 2026-07-22
- Depends on: **RFC-0023** (contract state / eth_call — this RFC *is* the design of RFC-0023 §3's
  deferred local-execution path and §4's producer), RFC-0001 (decode registry + vendored ABIs — the
  ABI machinery call encode/decode reuses), RFC-0013 §3 (sealed Parquet segments + DuckDB union — where
  cold call/state results spill), RFC-0009 (factory discovery — declared calls must resolve
  `event.params`/`event.address` for dynamically discovered children). Adjacent: RFC-0003 (reth ExEx —
  the local-datadir substrate a later stage reuses), RFC-0014 (firehose state diffs — the sibling
  source of per-block state deltas), RFC-0021 (per-cursor budget/isolation the caches must live inside).
- Blocks: making RFC-0023 tier-3 *actually run on a cheap box* — historical `eth_call` handler support
  **without a local archive node**. Closes the last mechanical gap in the ">70% of subgraphs use
  eth_call" story.
- Nature: **design + engineering RFC — accepted design, deferred build.** This RFC supersedes the
  "research note against RFC-0014/0003" placeholder in RFC-0023 §3/§4 with a concrete engine design. It
  is **not** the next thing built: the build order (below and in Implementation) is derive-first (tiers
  1–2) → a *simple* RPC tier-3 fallback → **measure the residue** → this engine, and only if the residue
  is large *and* archive-RPC-free operation is demanded (or RFC-0003's reth lands and makes the local
  path nearly free). Stage 0/1 (`RpcForkSource` MVP) is buildable now against a user-supplied archive
  RPC; Stages 2–3 are designed-now-built-later.
- Origin: roadmap thread 1 raw note — *"an eth_call-optimized EVM executor that's **not** a generic
  archive node; strip everything but call handling."* (`docs/high-level-roadmap-jul-aug-2026.md`).

## Abstract

RFC-0023 settled the *policy* for contract state — derive what you can (tier 1), cache immutable
metadata (tier 2), fall back to a pinned-block `eth_call` for the irreducible residue (tier 3),
optionally pull a hosted verifiable cache (tier 4) — and deliberately left the *mechanism* of tier-3
**local** execution undesigned, calling it a research note. This RFC designs that mechanism.

The Graph forces indexers to run a full **archive node** for any subgraph with `eth_call` handlers,
because a call must execute against **historical state at the indexed block** (EIP-1898 hash-pinned).
An Erigon 3 Ethereum-mainnet archive is ~**1.77 TB** (4 TB recommended, 32–64 GB RAM); an Arbitrum One
archive is ~**4 TB** on the smallest (PathDB) configuration. That is the antithesis of nuthatch's
"≤2 GB RAM, one tiny binary, cheap box."

The key insight: **a nest only ever needs the state it actually touches.** An indexer calls the same
contracts with the same slots at sequential blocks, so after warm-up, block N+1 is overwhelmingly cache
hits. We therefore build a **demand-driven state cache** — not a smaller archive node — backed by
**revm** for exact `eth_call` semantics, a `StateSource` trait with a blanket `revm::DatabaseRef` impl,
three staged source implementations, three cache tiers spilling to nuthatch's existing redb/Parquet
substrate, and a declared-calls prefetch stage borrowed from graph-node. The archive requirement is
**relocated** (to a remote archive RPC for cold/deep-backfill state, or later a reth datadir), never
claimed to be eliminated — stated honestly in Non-goals and Risks.

## Motivation

- **RFC-0023 tier 3 has no engine.** Tier 1 (derive) and tier 2 (metadata cache) shipped/are buildable,
  but the pinned-block fallback needs something that can actually *produce* `result = f(code, storage,
  block, calldata)` cheaply and — for operators who want it — *without* a hard hot-path dependency on a
  remote archive endpoint.
- **The wedge forbids a local archive.** 1.77 TB / 32 GB RAM violates every non-negotiable in
  `CLAUDE.md`. But a nest indexing three AMM pools touches a few hundred storage slots across a few
  contracts — kilobytes of hot state, tens of GB for a rolling window. Demand-driven caching turns "run
  an archive node" into "cache the slice you touch."
- **Determinism is on our side.** A hash-pinned `eth_call` is a pure, re-executable function — it
  satisfies "determinism in the core" cleanly, provided we never touch `latest` in the data path
  (RFC-0023's `latest`-guard test still applies). revm's `transact_ref` gives exact EVM semantics with
  no state commit.
- **We can amortize.** graph-node proved declared eth_calls run in parallel before handlers cut indexing
  time dramatically. One `CacheDB` per block amortizes shared reads (`token0`/`token1`) across every
  call at that block; a persistent slot cache amortizes across sequential blocks.

## Goals

1. A **`StateSource` trait** with a blanket `revm::DatabaseRef` impl, so any historical-state provider
   plugs into revm identically. Parameterized by pinned block.
2. **Stage 1 MVP `RpcForkSource`**: lazy remote state against a user-supplied archive RPC (`--state-rpc`),
   via `foundry-fork-db`'s `SharedBackend` or `revm` `AlloyDB` pinned per block. Runs on a laptop today.
3. **Three cache tiers** (L1 in-memory `CacheDB`/block, L2 persistent redb slot cache, L3 optional
   call-result cache) spilling cold to sealed Parquet, queryable via DuckDB — consistent with RFC-0013.
4. **Declared eth_calls in `nuthatch.toml`** (analogue of graph-node specVersion ≥ 1.2.0 declared calls)
   so calls prefetch/execute in parallel per block; undeclared calls execute lazily.
5. An **ergonomic host-side call binding** feeding results to handlers as Arrow columns — never a call
   issued *by* a WASM component (preserves the liminal purity model).
6. **In-process Rust API** (`call_at`, `call_batch_at`) as primary; optional `jsonrpsee` `eth_call`
   endpoint for debugging.
7. **Stage 2 `LocalFlatSource`**: a bounded rolling local flat-state window built from per-block state
   diffs, with changeset undo logs for reorg depth, falling back to `RpcForkSource` for older blocks.
8. Stay inside the **≤2 GB per-cursor** budget and the reorg/finality invariants.

## Non-goals

- **The derive-first path stays zero-dependency, and this engine never changes that.** RFC-0023 tier 1
  (derived recipes) and tier 2 (metadata cache) carry **no external data dependency** — a nest built on
  them is fully "be your own indexer," exactly as today. This engine is **strictly opt-in, for the
  irreducible residue only** (feature-gated off by default). "Nuthatch needs an archive RPC" must never
  become true for the derive-first majority; it is a capability a specific workload turns on, never a
  baseline. This is the founding non-negotiable, defended in writing.
- **No local archive node.** We relocate the archive requirement to a remote endpoint (cold/deep
  backfill) or a later reth datadir — we do not eliminate it. Stated plainly.
- **No `latest` in the data path.** Every data-path call is EIP-1898 hash-pinned; RFC-0023's
  `latest`-guard test is extended to cover this engine.
- **Components never issue calls.** eth_call is host-side ingestion; components stay zero-capability/pure
  and receive results as Arrow input.
- **No faithful Arbitrum execution in the MVP** — and this matters, because our own first-party nests
  (horizon, uniswap-v3) are Arbitrum. Stock revm ≠ ArbOS (a "Geth sandwich" with custom precompiles,
  dual-gas, retryables; Stylus WASM out of scope). The MVP **proxies Arbitrum `eth_call` to the
  `--state-rpc` endpoint** — a documented approximation boundary, and honestly a *caching* win rather
  than a *sovereignty* win on Arbitrum until an ArbOS-aware revm fork exists. An `arb-revm`-style fork is
  a future possibility (§Future), not a commitment.
- **No Stylus/WASM-contract execution.** Out of scope entirely.
- **Not a general MEV/simulation framework.** Read-only `eth_call` at a pinned block; nothing that
  commits state.

## Design

### §1 — The `StateSource` trait and its revm binding

```rust
/// Historical chain state at a single pinned block, addressed EIP-1898.
/// One instance is bound to exactly one (chain_id, block_hash/number).
pub trait StateSource: Send + Sync {
    fn basic_account(&self, addr: Address) -> Result<Option<AccountInfo>, StateError>;
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, StateError>;
    fn storage(&self, addr: Address, slot: U256) -> Result<U256, StateError>;
    fn block_hash(&self, number: u64) -> Result<B256, StateError>;
    fn pinned_at(&self) -> BlockPin;   // { chain_id, number, hash }
}
```

A blanket adapter gives every `StateSource` a `revm::DatabaseRef` for free (revm's `DatabaseRef`
requires exactly `basic_ref`/`code_by_hash_ref`/`storage_ref`/`block_hash_ref` — a 1:1 mapping):

```rust
pub struct RevmState<S: StateSource>(pub S);
impl<S: StateSource> DatabaseRef for RevmState<S> {
    type Error = StateError;
    fn basic_ref(&self, a: Address) -> Result<Option<AccountInfo>, Self::Error> { self.0.basic_account(a) }
    fn code_by_hash_ref(&self, h: B256) -> Result<Bytecode, Self::Error>        { self.0.code_by_hash(h) }
    fn storage_ref(&self, a: Address, s: U256) -> Result<U256, Self::Error>     { self.0.storage(a, s) }
    fn block_hash_ref(&self, n: u64) -> Result<B256, Self::Error>               { self.0.block_hash(n) }
}
```

Three staged implementations:

- **Stage 1 — `RpcForkSource` (MVP, buildable now).** Lazy remote state against a user-supplied archive
  RPC. Two interchangeable backends behind the same trait: `revm` `AlloyDB` pinned to `BlockId::Hash`
  (simplest), or `foundry-fork-db`'s `SharedBackend` (thread-safe, shared cache, disk-flushable — the
  mature choice, the backend behind Anvil/Forge forking). Every miss is an
  `eth_getStorageAt`/`eth_getCode`/`eth_getBalance` at the pinned block.
- **Stage 2 — `LocalFlatSource`.** A bounded rolling local flat-state window built *incrementally* from
  per-block state diffs as nuthatch processes blocks (redb flat store keyed `(address, slot)` +
  changeset undo logs sized to reorg depth). Older blocks fall back to `RpcForkSource`. The RFC-0014
  state-diff sibling: extraction feeds both the indexer and this cache.
- **Stage 3 — `ProofVerifyingSource` (optional).** `eth_getProof` + Merkle verification for
  trust-minimized head-following only; bounded by recent-state limits.

### §2 — Execution

`revm` (pin the current stable major post-Framework-API rewrite; lock it in `Cargo.lock` behind an
`eth-call` feature so the default binary never pulls it). Per call: build `TxEnv` (`caller`,
`TransactTo::Call(to)`, `data`, `value=0`), a `BlockEnv`/`CfgEnv` from the pinned block header, and run
**`transact_ref`** — execute without committing state. One **`CacheDB<RevmState<S>>` per block**
amortizes shared reads across every call at that block. EIP-1898 block-hash pinning everywhere.

### §3 — Cache tiers

- **L1 — in-memory `CacheDB` per block.** Lives for one block's handler run; discarded after.
- **L2 — persistent per-block state cache (redb).** Keyed `(chain_id, block_number, address, slot) →
  value` (and `(chain_id, code_hash) → bytecode`). The demand-driven core: after warm-up, sequential
  blocks are mostly L2 hits. Beside `nuthatch.redb`; counts against the per-cursor budget.
- **L3 — optional call-result cache.** Keyed `(chain_id, block, to, keccak(calldata)) → returndata`,
  mirroring graph-node's `call_cache` (and its lesson: a poisoned cache must be clearable — ship
  `nuthatch state-cache clear --from --to`). Off by default.
- **Cold spill.** Past finality, L2/L3 entries seal into **content-addressed Parquet segments**, listed
  in `manifest.json` like event segments, attached read-only by DuckDB. Immutable, past finality —
  reorgs never touch them (the `CLAUDE.md` law).

### §4 — Declared calls / prefetch

A **declared eth_calls** concept in the manifest, analogous to The Graph's specVersion ≥ 1.2.0 declared
calls (declared per handler, executed in parallel before handlers run):

```toml
[[contracts]]
alias = "pool"
address = "0x…"
abi = "abis/uniswap_v3_pool.json"

# Declared calls run in parallel BEFORE handlers, at the event's pinned block.
[[contracts.calls]]
on_event = "Swap"                                 # fires alongside pool__swap rows
name     = "reserves"                             # -> column pool__swap.call_reserves (Arrow)
call     = "Pool[event.address].slot0()"          # event.address / event.params.* resolvable
```

The prefetch stage sits **between window-fetch and decode/handler-run** — graph-node's slot. Declared
calls for a block execute in parallel (one `CacheDB`/block, fan-out), populating L1/L2 before handlers.
Results reach handlers as Arrow columns. **Undeclared calls** execute lazily during the handler run,
still hash-pinned, still cached; a lint nudges authors to declare them, and to **prefer a derived recipe
where one exists** (RFC-0023 tier 1).

### §5 — Handler API surface

- **Declarative (default).** Declared-call results are extra columns on the event table
  (`pool__swap.call_reserves_*`), queryable in SQL views exactly like decoded fields — zero new concepts.
  A nest author "gets" eth_call by adding a `[[contracts.calls]]` entry, never by writing code.
- **Imperative (WASM escape hatch).** Handlers receive an Arrow batch already carrying declared-call
  returndata columns; a generated typed binding (`ctx.calls.reserves()`) is **read-from-Arrow**, not a
  live call. Purity preserved.
- **In-process Rust API (primary internal surface).** `call_at(pin, req)` / `call_batch_at(pin, reqs)`.
- **Optional JSON-RPC `eth_call` (debug).** A `jsonrpsee` endpoint at a pinned block — off by default,
  localhost-only. Never a production data service.

### §6 — Reorg handling

- All data-path calls are **hash-pinned** (EIP-1898) — a result is bound to a block hash, not a number.
- On reorg, the existing `rollback_to(block)` also **invalidates L2/L3 entries for orphaned block
  hashes** — a range-delete over the mutable cache tables, identical discipline to event rows. Sealed
  cold segments are strictly past finality and untouched.
- Stage 2 `LocalFlatSource` keeps **changeset undo logs** sized to the reorg depth.
- Integrates with the per-cursor "detected once, rolled back across all nests" roost mechanism — a reorg
  invalidates event rows and call/state cache entries in the same transaction.

### §7 — Multi-chain fidelity

- **Ethereum mainnet + OP-stack:** stock revm (and `op-revm`) is faithful. Full engine support.
- **Arbitrum Nitro/ArbOS:** not stock EVM. **MVP proxies Arbitrum `eth_call` directly to `--state-rpc`**
  (a `StateSource` variant answering whole calls, not slots), documented as an approximation boundary. A
  later `arb-revm`-style fork is a future possibility, explicitly not promised.

## Implementation

**Build sequencing (deliberate — this engine is not the next build):**

1. **Derive-first tiers 1–2** (RFC-0023) — in flight; zero external dependency.
2. **A *simple* tier-3 RPC fallback** — batched pinned-block `eth_call` to a user-supplied archive RPC,
   result content-addressed and sealed to segments, `latest`-guarded. **No revm.** This is the pragmatic
   80%: correctness for the residue for operators who accept an archive-RPC dependency.
3. **Measure the residue** on a real AMM nest (what fraction of reads are genuinely non-derivable, and
   the L2 hit-rate). Only if the residue is large *and* archive-RPC-free operation is demanded (or
   RFC-0003 lands) do we build:
4. **This engine** (revm-backed local execution), Stage 1 → 2 → 3.

**Modules (feature-gated `eth-call`, mirroring `exex`/`object-store` gating):** `src/state/source.rs`
(`StateSource` + `RevmState`), `rpc_fork.rs`, `local_flat.rs`, `proof.rs`, `exec.rs` (revm + per-block
`CacheDB`), `cache.rs` (L2/L3 redb + Parquet spill), `declared.rs` (manifest parse + prefetch), `api.rs`
(`call_at`/`call_batch_at` + optional `jsonrpsee`).

**New deps behind `eth-call`:** `revm` (pinned), `foundry-fork-db`, `alloy-provider`, `jsonrpsee`. The
default embedded binary pulls none.

**Manifest/CLI:** `[[contracts.calls]]` (validated against the `DecodeRegistry`; factory children
resolve `event.params.*`); `--state-rpc <url>`, `--state-window <blocks>`, `nuthatch state-cache clear
--from --to`.

**Budget:** L2 disk-first with a bounded in-RAM LRU; a CI scenario measures peak RSS with a 3-pool AMM
nest making declared calls, asserting ≤2 GB/cursor.

## Testing

- **Parity vs a reference archive:** for a fixture range, `call_at` via `RpcForkSource` equals the
  archive node's `eth_call` at the same pinned block, byte-for-byte.
- **Derive-vs-call (the tier-1 boundary):** where a derived recipe exists (`reserves`, `total_supply`),
  the derivation equals the engine's `eth_call` on fixtures — proving "prefer derived" is safe.
- **Cache hit-rate / warm-up:** measure L2 hit-rate over sequential blocks on a real AMM nest; assert it
  crosses a target (the demand-driven claim, labeled a target until measured live).
- **Reorg convergence (proptest, extended):** random reorg depths invalidate exactly the orphaned-hash
  cache entries; canonical state converges; sealed segments never mutate.
- **`latest`-guard:** extend RFC-0023's test to assert no `state/` data-path code issues a call at
  `latest`.
- **Budget bench:** `nuthatch bench` gains a declared-call scenario; RSS regression fails CI.
- **L2 approximation guard:** an Arbitrum call routed through revm (rather than the RPC proxy) fails a
  golden test — asserting the MVP *doesn't* silently mis-execute ArbOS.

## Risks

- **Archive requirement relocated, not removed.** Cold/deep-backfill state still needs a remote archive
  RPC (or Stage 3 reth datadir). Mitigation: name it honestly; the derive-first policy avoids calls
  entirely for most reads; a tip-following nest needs only recent state.
- **Wedge erosion.** If the engine drifts from opt-in-for-residue to baseline, "no mandatory data
  dependency" breaks. Mitigation: feature-gated off; derive-first is the default and stays
  zero-dependency (Non-goals, defended by test/CI framing).
- **Cold-start latency.** A fresh nest starts cache-empty. Mitigation: declared-call parallel prefetch;
  optional tier-4 hosted verifiable cache pull to warm from a shared producer.
- **`latest` leaking in.** Would silently break determinism. Mitigation: the extended `latest`-guard.
- **Arbitrum infidelity.** Stock revm mis-executes ArbOS. Mitigation: RPC-proxy MVP + explicit
  approximation-boundary docs + the golden guard test.
- **Dep weight / build time.** revm + foundry-fork-db + alloy-provider is heavy. Mitigation: strict
  feature-gating (same discipline as `exex`).
- **Cache poisoning.** A bad archive endpoint poisons L2/L3. Mitigation: content-addressing on cold
  segments + `state-cache clear` + spot re-execution verification.

## Alternatives considered

- **Ship a local mini-archive node.** Rejected: 1.77 TB (ETH) / ~4 TB (Arb), 32–64 GB RAM — violates
  every non-negotiable. The whole point is *not* to be an archive node.
- **Witness-only / proof-verifying as the primary path.** Rejected as primary: non-archive nodes serve
  only recent state, so witnesses can't cover historical backfill. Kept as optional Stage 3.
- **Firehose extended blocks carry callable state.** Rejected: Firehose blocks carry logs/traces/state
  *diffs*, not a queryable trie; state diffs *feed* Stage 2 but aren't an execution substrate.
- **Per-event RPC `eth_call` (the subgraph way).** Kept only as the lazy/undeclared fallback and Stage 1
  miss path — but note the *simple tier-3* (Implementation build-order step 2) is exactly this,
  content-addressed and sealed, and is the pragmatic first fallback before the engine.
- **eth_call from a WASM component.** Rejected: violates the liminal purity model.

## Prior art

- **graph-node `EthereumCallCache`** (`call_cache`, keyed per contract/block, clearable) + **declared
  eth_calls** (specVersion ≥ 1.2.0). L1/L3 and §4 mirror this directly.
- **foundry-fork-db** (`SharedBackend`/`BlockchainDb`) — the mature thread-safe fork backend; the
  recommended Stage 1 backend.
- **revm** `DatabaseRef`/`CacheDB`/`AlloyDB`/`transact_ref` — the execution core; the Framework API lets
  us pin a stable major and (later) build an Arbitrum variant as `op-revm` extends for OP-stack.
- **ress / paradigmxyz `stateless`** — the model for Stage 3's proof-verifying head-following.
- **Erigon 3 flat state** — the demand-driven L2 slot cache is a per-nest, bounded analogue.

## Open questions

1. **revm major to pin** — the current stable line post-Framework-API rewrite; lock in `Cargo.lock`.
2. **Stage 1 backend default** — `foundry-fork-db SharedBackend` (mature) vs bare `AlloyDB` (fewer deps).
   Lean `SharedBackend`.
3. **Declared-call manifest grammar** — `ABI[addr].method(args)` parsing, `event.params.*` binding,
   factory-child resolution (RFC-0009 overlap).
4. **L2 eviction policy** — LRU vs finality-based spill-to-Parquet; interaction with the per-cursor
   budget.
5. **Arbitrum** — RPC-proxy MVP now; trigger for adopting an `arb-revm`-style fork later (audit gate).
6. **Tier-4 handoff** — how §3 cold segments publish to the RFC-0019 object store as the hosted
   verifiable cache (RFC-0023 §4) without becoming a mandatory dependency.

## Future possibilities

- **reth-datadir reuse (Stage 3+).** When RFC-0003's colocated reth exists, a `RethDatadirSource` reads
  historical state locally — no RPC round-trip, no remote archive. The "strip everything but call
  handling" endpoint of the same axis.
- **Arbitrum-aware revm fork** for faithful ArbOS execution.
- **Cross-nest L2 sharing** within a roost (shared slot cache for same-chain nests), inside per-cursor
  isolation.
- **Tier-4 producer role:** a nuthatch instance that has warmed a deep cache publishes content-addressed
  state/call segments others pull — the CDN, never a dependency (RFC-0023 §4).
