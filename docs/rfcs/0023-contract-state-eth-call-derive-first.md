# RFC-0023: Contract state (eth_call) — derive-first, with a verifiable fallback

- Status: **Accepted** (2026-07-21) — **tiers 1–2 building (2026-07-22)**. **Tier 1**: the derive-first
  recipe library (`src/recipes.rs`) + `nuthatch recipe list|add` — **four recipes**, all derived with
  **no eth_call** and derive-correctness proven by e2e: three ERC-20-generic (`total_supply` = Σ mints −
  Σ burns; `balances` = per-address Σ(in) − Σ(out); `holder_count` = non-zero holders) plus the
  protocol-specific **`reserves`** (Uniswap-V2 `getReserves()` = the latest `Sync` per pair).
  **Tier 2**: the immutable-metadata cache (`src/metadata.rs`) + `nuthatch metadata fetch` — `decimals`/
  `symbol`/`name` fetched once (they never change) and cached in `metadata.json`; the pure encode/decode
  (uint8 / ABI-string) is unit-tested, the RPC fetch is live-verified. Pending: tier 3 (a *simple* RPC
  eth_call fallback, sealed + `latest`-guarded), tier 4 (hosted verifiable cache). The
  **local-execution engine** for tier 3/4 is designed in **RFC-0024** (Draft, deferred build).
- Author: Pete (cargopete)
- Date: 2026-07-21
- Depends on: RFC-0018 §1 (authored SQL views — the surface the derive-first recipes are written in),
  RFC-0001 (decode; the events the derivations consume), RFC-0013 §3 (sealed segments the fallback
  results are stored in), RFC-0019 (§4 hosted state-cache reuses the object-storage + content-addressing
  substrate). Adjacent: RFC-0003/0014 (reth + local execution — the substrate for the later
  call-optimized executor).
- Blocks: closing the **>70%-of-subgraphs-use-eth_call** gap — the largest single feature gap versus
  subgraphs, and the thing that lets nuthatch index the whole class of state-dependent nests.
- Nature: design RFC. **Tiers 1+2 buildable now; tiers 3+4 designed now, built later.** The
  "strip-an-archive-node" executor is a **research note against RFC-0014/0003**, not a build this window.
- Origin: roadmap thread 1 (`docs/high-level-roadmap-jul-aug-2026.md`), settled 2026-07-21.

## Abstract

The Foundation says **>70% of subgraphs use `eth_call`** — reading contract *state* the event alone
doesn't carry (`getReserves`, `totalSupply`, `balanceOf`, `decimals`, arbitrary views). nuthatch today
indexes event *logs* only (we have `eth_call`/`eth_getStorageAt` plumbing, but at `latest`, and only for
init-time ABI/proxy introspection — not historical, not in the data path).

The reframing, and the differentiator: **most subgraph `eth_call`s exist because subgraphs have no
incremental-view engine — they can't *derive* state, so they *fetch* it.** nuthatch's DBSP/IVM core was
built for exactly this. We already prove it: **balances are derived** (`BalanceView` from `Transfer`
events), never `balanceOf`-ed. So the play is not "add eth_call" — it's **turn most eth_call into
declarative derivation, and keep a real, verifiable fallback for the irreducible residue.**

A four-tier model:

1. **Derive it** — view recipes over indexed events (reserves, supply, balances). Free, deterministic,
   no archive node. **The differentiating half. Buildable now.**
2. **Cache immutable metadata** — `decimals/symbol/name`: call once, content-address, cache forever.
3. **eth_call fallback** — for the genuinely non-derivable: batched `eth_call` **at a pinned historical
   block**, results content-addressed and sealed to segments, versioned like decodings. Host-side
   ingestion, not a component effect.
4. **Hosted verifiable state cache** *(optional)* — publish call-result segments to shared object
   storage so others pull instead of re-producing. An acceleration, never a dependency.

## Motivation

- **The biggest anti-subgraph gap.** A nest that can't read `getReserves` can't index most AMMs. Closing
  this makes nuthatch viable for the bulk of real subgraph workloads.
- **We can beat, not match.** Deriving state incrementally is *cheaper and faster* than an archive
  `eth_call` per event during backfill (the dominant backfill cost for subgraphs). Where a read is
  derivable, we skip the archive node entirely — a strict win, not parity.
- **Determinism is already on our side.** An `eth_call` at a pinned block is a pure, re-executable
  function — it fits the "determinism in the core" non-negotiable cleanly, *provided* we never call
  `latest` in the data path.

## Goals

1. **Tier 1** — a first-party **view-recipe library** for the common derivable reads, authored as
   RFC-0018 §1 SQL views over decoded events, maintained by the IVM core (reorg-safe by construction).
2. **Tier 2** — an **immutable-metadata cache**: one call per (contract, selector), content-addressed,
   cached; served as a normal field.
3. **Tier 3** — a **pinned-block `eth_call` fallback** as a *host-side ingestion source*: batched,
   deterministic, content-addressed, sealed to segments, decode-versioned.
4. **Tier 4 (design)** — an **optional, verifiable, pull-through hosted state cache** over the RFC-0019
   substrate; sovereign by construction (re-executable), never mandatory.
5. Keep the **transform-layer purity model intact**: components never issue calls.

## Non-goals

- **No `latest` in the data path** — non-deterministic; forbidden for anything feeding stored state.
  `latest` stays only for init-time introspection (existing use).
- **Components never make calls.** eth_call is *ingestion*; components receive results as Arrow input and
  stay zero-capability/pure (liminal purity model). No effectful component feeds canonical entities.
- **No mandatory hosted cache.** Tier 4 is optional acceleration; producing results yourself from an
  archive node must always work (the founding "no mandatory third-party data dependency").
- **Not the call-optimized executor build.** "Strip an archive node to just historical-state call
  handling" is a **research note against RFC-0014/0003**, not built here.

## Design

### §1 — Tier 1: derive-first (the differentiator, buildable now)

The common "eth_call" reads are functions of events we already index:

| Subgraph `eth_call` | Derived from | Mechanism |
|---------------------|--------------|-----------|
| `balanceOf(a)` | `Transfer` in/out | already shipped (`BalanceView`, slice 3) |
| `totalSupply()` | mints − burns (`Transfer` from/to 0) | IVM view |
| `getReserves()` | `Mint`/`Burn`/`Swap` deltas | IVM view (pair reserves) |
| `getAmountOut`-style | reserves + curve math | authored SQL view over derived reserves |

Ship these as a **first-party recipe library** authored in RFC-0018 §1 views + `semantic.toml`, so
they're validated, drift-gated, described, and taught by the builder skill (RFC-0017) — not hand-rolled.
Reorgs are retractions (IVM), backfill and tip run the same circuit. **No archive node, no per-event
round-trip.**

### §2 — Tier 2: immutable-metadata cache

`decimals/symbol/name` (and similar write-once constants) are called **once** per contract, the result
content-addressed by (chain, contract, selector) and cached indefinitely (they never change). Sourced at
init or first sighting; served as an ordinary field. Cheap, and it removes the second-biggest chunk of
"eth_call" noise.

### §3 — Tier 3: pinned-block `eth_call` fallback (host-side ingestion)

For the **irreducible** — an oracle read, an ungoverned param, a view on a contract we lack full event
coverage of:

- The **host** (never a component) issues **batched `eth_call` at a pinned historical block** — the same
  batched-boundary discipline as log extraction; Arrow interchange.
- Results are **content-addressed** by (chain, block, contract, calldata) and **sealed to segments**,
  **versioned like decodings** (never retroactively re-executed when inputs improve — a new version, not
  a rewrite).
- Determinism holds because the block is pinned: `result = f(code, storage, block, calldata)`,
  re-executable byte-for-byte.
- A nest **declares** an irreducible call (the read it needs, the contract/selector); the host schedules
  the batched calls during backfill/tip. Because it's declared and host-run, components still receive
  only *data*.

Source of the calls: RPC/archive first (buildable when an archive endpoint is available); **local
execution via reth ExEx** (RFC-0003/0014) later, which removes the round-trip entirely; the
**call-optimized executor** is the research end of that same axis.

### §4 — Tier 4: the hosted verifiable state cache (optional)

A pinned-block call-result at (chain, block, contract, calldata) is **immutable, content-addressable,
expensive to produce, cheap to verify** (re-execute one call). So one operator can produce a range once
and others **pull** the sealed segments instead of re-running millions of calls:

- Stored in **shared object storage** — the **same substrate as the nest registry** (RFC-0019 §1): a
  nest bundle and a call-result segment are both immutable addressed blobs. One substrate, two payloads.
- **Optional, never required** — you can always produce results yourself from an archive node; the
  hosted set is a CDN, not a dependency (founding non-negotiable).
- **Verifiable, so trustless** — any pulled segment is spot-checkable by re-executing the call at the
  pinned block; verification = deterministic re-execution + content-addressing (the founding
  verifiability stance; nothing heavier). Degrades **warn-and-skip** when unconfigured/offline (the
  liminal optional-sink pattern).

This is the concrete form of the "host that data and let people pull/query it" idea — framed so it
accelerates without ever becoming a mandatory third-party data service.

### Determinism & purity placement (why this is clean)

An `eth_call` result is a pure function of pinned chain state — deterministic and re-executable. It
belongs to **ingestion, not the transform layer**: the host extracts and pins it; components receive it
as Arrow input and never issue a call, so they remain **zero-capability and pure**, and may feed entity
derivation. `latest` is never used in the data path. This keeps us squarely inside the liminal purity
model and the "determinism in the core" non-negotiable — no bending required.

## Implementation

- **Tier 1**: extend the first-party recipe/view set (RFC-0018 §1); generalise the slice-3 balance
  circuit pattern to reserves/supply; author curve-math views. No core-path change (read-only views over
  decoded facts + IVM).
- **Tier 2**: a metadata cache keyed by (chain, contract, selector), populated via the existing
  `eth_call` plumbing (pinned/one-shot), content-addressed.
- **Tier 3**: a host-side **call-extraction source** beside log extraction — batched pinned-block
  `eth_call`, Arrow output, sealed via RFC-0013; a nest-level declaration of irreducible reads.
- **Tier 4**: reuse RFC-0019's `BundleStore`/object-storage for call-result segments; a pull-through
  cache with re-execution verification and warn-and-skip degradation.

## Testing

- **Derive-vs-call parity** (tier 1): a derived `reserves`/`totalSupply` view equals the actual
  `eth_call` result at the same block, on fixed fixtures — proves the derivation is *correct*, not merely
  plausible.
- **Pinned-block determinism** (tier 3): the same declared call at the same block re-executes to a
  byte-identical result across runs/machines.
- **Metadata cache** (tier 2): one call per constant; cached value served identically thereafter.
- **Cache verify-by-re-execution** (tier 4): a pulled segment re-executes to the same result; a
  tampered/mismatched segment is rejected.
- **Offline degradation** (tier 4): with the hosted cache unreachable, the nest still indexes (produces
  results locally or warns-and-skips), never hard-fails on a missing optional dependency.
- **`latest` guard**: a test asserts no data-path code path issues an `eth_call` at `latest`.

## Risks

- **Derive/eth_call divergence** — a derivation subtly wrong vs the contract's own value. Mitigation: the
  derive-vs-call parity test on fixtures gates every recipe; when a read *can't* be derived
  provably-correctly, it belongs in tier 3, not tier 1.
- **`latest` leaking into the data path** — would silently break determinism. Mitigation: the explicit
  `latest`-guard test.
- **Hosted cache becoming a de-facto dependency** — the founding risk. Mitigation: optional by
  construction, verifiable, warn-and-skip; self-production always works and is CI-asserted.
- **Executor scope creep** — the stripped executor is tempting to build. Mitigation: explicitly a
  research note against RFC-0014/0003, out of this window.

## Alternatives considered

- **Plain per-event `eth_call` (the subgraph way)** — simplest, but slow (archive round-trip per event
  during backfill) and misses our core advantage. Rejected as the *primary*; kept only as tier-3
  fallback for the irreducible.
- **`eth_call` as an effectful component** — would let a component make calls, violating the purity model
  (effectful components produce annotations only, never canonical entities). Rejected: eth_call is
  ingestion, host-side.
- **Mandatory hosted state service** — fast, but a third-party data dependency the brief forbids.
  Rejected; tier 4 is optional + verifiable instead.

## Open questions

- The recipe library's initial coverage — which reads ship first-party (reserves, supply, balances +
  what else)?
- How a nest **declares** an irreducible call, and how "derivable vs irreducible" is surfaced to the
  author (a lint/hint: "you're calling `getReserves` — a derived recipe exists, prefer it").
- Cache segment format + addressing key precision (calldata normalization, chain/block identity).
- Trigger for promoting tier-3 sourcing from RPC/archive to local execution (RFC-0003/0014 readiness).
