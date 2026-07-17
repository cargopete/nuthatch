# RFC-0014: Firehose-class extraction — traces and state diffs via ExEx

- Status: Draft (v1) — **future / deferred**
- Author: Pete (cargopete)
- Date: 2026-07-17
- Depends on: RFC-0003 (the ExEx execution-time source this reads from — the hard
  prerequisite), RFC-0001 (the decode registry, extended here for calldata + state),
  RFC-0004 (own-node re-execution for the backfill path)
- Blocks: full data-coverage parity with Firehose/Substreams-class products (Amp); the
  "nothing they do we can't do" claim (2026-07-17 GraphOps convo).
- Priority: **deferred.** Gated on RFC-0003 actually landing (ExEx wired to a real
  node). Not before the pilot, not before ExEx is live. This RFC exists now for one
  concrete reason — to make RFC-0003 get *built* forward-compatibly (see its §6 add-on)
  so this is additive later, not a rewrite. Building it comes after.
- Nature: capability/design RFC; the plan is a sketch, not a build order.

## Abstract

Index the two data classes a Firehose/Amp-class product exposes that `eth_getLogs`
structurally cannot: **state changes** (storage-slot writes, balance / nonce / code
changes) and **call traces** (the internal call tree with decoded calldata). Both are
outputs of block execution, and both are *deterministic re-execution artifacts* — which
is Nuthatch's founding thesis, not a bolt-on. The extraction surface is new; everything
downstream (hot store, Parquet sealing, SQL, IVM views, reorg-safety) is row-agnostic
and inherited for free.

## Motivation

The competitive edge of Firehose/Amp is the *rich* data, not the events — traces and
state diffs power things (internal transfers, storage-level accounting, MEV analysis,
proxy/impl introspection) that event logs can't express. RFC-0003 gives us the
substrate: in-process execution output from a colocated reth, with no third party. This
RFC closes the coverage gap and turns "we could do that" into a design on record. The
philosophical fit is the flex — Firehose *is* deterministic execution-time extraction,
and that is already what Nuthatch promises for events.

## Goals

1. **State diffs as first-class rows.** A `state_diffs` surface: `(address, slot,
   prev, new, block_number, tx_index, log_index?)`, plus balance/nonce/code changes.
   Cheap — they fall straight out of the ExEx `ExecutionOutcome` / `BundleState` reth
   already computed.
2. **Call traces as first-class rows.** A `traces` surface: `(block_number, tx_hash,
   trace_address, from, to, value, call_type, gas, gas_used, input, output, error)`,
   with **decoded calldata** (function selector → args) reusing the same alloy ABI
   machinery event decode uses — the calldata mirror of topic0-keyed event decode.
3. **Same determinism guarantee.** Content-addressed segments; the ExEx path and an
   own-node re-execution backfill produce byte-identical rows (the RFC-0003 /
   RFC-0004 discipline, extended to traces/state).
4. **Both regimes.** Tip (from the ExEx notification) and backfill (own-node
   re-execution, RFC-0004). RPC mode is explicitly excluded (§Non-goals) — stated
   honestly, not hidden.

## Non-goals

- **RPC-mode firehose data.** Public RPC `debug_trace*` / `debug_storageRangeAt` are
  expensive, rate-limited, and frequently absent. This capability is **own-node / ExEx
  only**; the RPC embedded path stays log-centric. Say so plainly.
- **Full archive state.** We capture *diffs* and *traces* as rows, not full historical
  state at every block. That's an archive node's job.
- **A Firehose wire protocol / gRPC Firehose server.** Our interchange is Arrow +
  content-addressed Parquet. If Firehose *protocol* compatibility is ever wanted, it's a
  separate export shim, not core.
- **Substreams-runtime compatibility.** Different topic; the WASM transform layer is our
  answer to programmable extraction, not a Substreams clone.
- **ABI-aware storage-layout decode** (decoding mappings/structs from slots) in v1 — raw
  slots first; layout-aware decode is a later increment (§Risks).

## Design

### 1. Extraction source — mostly already in hand

The ExEx `ChainCommitted` notification already carries the full `ExecutionOutcome`
(`BundleState`), so **state diffs require no extra execution** — they're a projection of
data reth already handed us (this is exactly why RFC-0003 must pass the whole
notification through, not a logs-only view — RFC-0003 §6). **Traces** need a
tracing-inspector re-execution pass over the block (a revm inspector), so they cost more
and are opt-in per nest.

### 2. Decode model — two new row producers beside event decode

Extend the registry with producers that sit alongside topic0-keyed event decode:

- **State-diff rows:** no ABI needed for raw slots — `(address, slot, prev, new)` keyed
  by block/tx. Emitted for addresses the nest scopes (all, or a contract set).
- **Trace rows:** calldata decoded via the ABI, keyed by the 4-byte **function
  selector** (the calldata analogue of topic0). Undecoded calls still get a raw row
  (selector + raw input), same contract-ABI-priority-then-generic-fallback rule as
  events.

Config is opt-in and scoped (volume, §3):

```toml
[extract]
traces = true            # per-nest; default false
state  = true            # per-nest; default false
# optional: restrict to a contract set / selector allowlist to bound volume
```

### 3. Volume and footprint — the real risk, named up front

Traces and state diffs are **high volume**: every internal call, every `SSTORE`. This is
where the row-count estimate and the ≤2 GB budget bite hardest. Therefore: **opt-in per
nest, scoping per-contract/selector is first-class, and the pre-backfill estimate must
loudly flag a traces/state nest as unbounded-by-construction** (the RFC-0009 estimate
already exists; extend it). An un-scoped `traces = true` on a busy chain is a foot-gun
and must warn like one.

### 4. Downstream — free

Trace and state rows are ordinary rows: they seal to Parquet past finality, gain
per-table SQL views, feed IVM/derived views, and roll back reorg-safely — no new
plumbing. The entire cost is §1–§3 (extraction, decode, volume management).

### 5. Sequencing within this RFC

State diffs **first** (cheap, straight from the bundle, no inspector). Traces **second**
(re-execution pass, dearer, more decode surface). Ship the cheap correct half before the
expensive half.

## Implementation plan (when unblocked by RFC-0003)

1. State-diff extraction from the ExEx `BundleState`; `state_diffs` table; determinism
   test vs `debug_storageRangeAt` at a pinned block.
2. Own-node backfill parity: re-execution produces byte-identical state-diff segments to
   the ExEx tip path (content hashes match).
3. Trace extraction via a revm tracing inspector; `traces` table; calldata decode reuse.
4. Volume controls: per-nest opt-in, contract/selector scoping, estimate integration +
   the loud warning.
5. Trace determinism/parity vs `debug_traceBlock` at a pinned block; published volume +
   RSS numbers (the honesty rule).

## Testing and acceptance

- Determinism: ExEx vs own-node re-execution → byte-identical trace and state segments.
- Parity: spot-check state diffs vs `debug_storageRangeAt`, traces vs `debug_traceBlock`
  at pinned blocks.
- Reorg: trace/state rows roll back with their block range (they're block-keyed rows —
  the existing rollback covers them; assert it).
- Volume: published row-count + RSS for a scoped traces nest; the estimate's warning
  fires for an un-scoped one.

## Risks

- **Volume / footprint (the big one).** Traces + state can dwarf event data. Mitigation:
  opt-in, scoping, loud estimate. Without discipline this blows the budget — the RFC
  treats that as a first-class constraint, not a footnote.
- **Trace re-execution cost** at the tip (inspector pass per block). Mitigation: traces
  opt-in; state-diffs (the cheap half) usable alone.
- **reth inspector / ExEx API churn.** Same containment as RFC-0003 — confined to
  `nuthatch-node`, pinned, CI-gated.
- **Storage-layout decode complexity.** Raw slots are easy; decoding mappings/structs
  needs layout metadata. Deferred — raw slots ship first, layout-aware decode later.

## Alternatives considered

- **Consume an external Firehose** (StreamingFast/Amp feed). Rejected: reintroduces a
  third party and a serialization boundary — against the sovereignty thesis and RFC-0003
  §Alternatives.
- **RPC `debug_` traces/storage.** Rejected: expensive, rate-limited, often unavailable,
  and not sovereign. Own-node/ExEx only.
- **Fold into RFC-0003.** Rejected: this is a distinct capability (new data model, new
  decode, applies to backfill re-execution too, not just tip). RFC-0003 gets only the §6
  forward-compat constraint; the feature lives here.

## Open questions

1. `state_diffs` as one wide table vs per-contract storage tables? Start with one wide
   table; per-contract views over it if demanded.
2. Do traces need a per-nest selector allowlist (not just contract scoping) to bound
   volume on very busy contracts? Likely yes for anything DEX-router-adjacent.
3. Raw-slot vs ABI-aware storage decode — v1 raw; when does layout-aware decode earn its
   complexity? Defer until a nest actually needs mapping/struct decode.
