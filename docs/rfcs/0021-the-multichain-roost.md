# RFC-0021: The multichain roost — one runtime, many chains, one cursor each

- Status: **Accepted** (2026-07-21) — §0 brief amendment applied to `CLAUDE.md` 2026-07-21. **Slice 1
  shipped 2026-07-21**: multichain config (`[[chains]]`), `group_by_chain` (one isolated cursor per
  chain), the per-cursor runtime (one `spawn_roost` per chain, fate-shared via `select_all`), and the
  **per-cursor RSS budget**. Single-chain roosts stay byte-identical to solo (parity e2e green). The
  **cross-cursor reorg-isolation property (§2) is now proven** (2026-07-22): a reorg on one chain leaves
  another's data byte-identical. Pending: only a live two-chain run (VPS).
- Author: Pete (cargopete)
- Date: 2026-07-21
- Depends on: RFC-0012 (the roost — one runtime, many nests, shared serving; and the per-nest isolation
  of storage/reorg/blast-radius this generalises), RFC-0009 (the factory/fan-out primitive and
  shared-cursor routing).
- Blocks: RFC-0022 (the distributed scheduler places *per-chain cursors* — this RFC defines that unit;
  the writer pool is this same model spread across machines).
- Nature: design RFC. **Embedded, buildable now** — no infra dependency. It relaxes one clause of the
  founding brief; that relaxation is proposed as §0 and must be accepted before §1+ is built.
- Origin: roadmap thread 2, Decision A (`docs/high-level-roadmap-jul-aug-2026.md`), authorized
  2026-07-21.

## ⚠️ Brief amendment required (see §0)

`CLAUDE.md` currently states, as a non-negotiable: *"A second chain means a second cursor means a second
process; never multiplex chains behind one cursor."* This RFC keeps the **correctness** half
(cursor-is-single-chain) and retires the **implementation** half (second-chain ⇒ second *process*). §0
is the proposed amendment; it must be accepted, not assumed.

## Abstract

A **roost** today (RFC-0012) is one runtime hosting many nests **on one chain**, sharing one cursor.
This RFC lets a roost host nests across **multiple chains** — e.g. one Base nest and one Arbitrum nest —
in **one runtime**, by running **N cursors, one strictly-isolated cursor per distinct chain**, behind a
shared serving/admin/registry layer.

Two things stay sacred, one habit goes:

- **PRESERVED — a cursor is single-chain.** One cursor tracks exactly one chain's canonical history.
  Chains reorg, finalize, and advance on independent clocks; sharing *one* cursor between two of them is
  incoherent and stays forbidden. "One Base nest + one Arb nest" means **two cursors**, never one doing
  double duty.
- **RETIRED — "second chain ⇒ second *process*."** Nothing about correctness requires each cursor its
  own OS process. A roost becomes **one runtime, N isolated per-chain cursors.**

**This is a capability, not a mandate.** One-chain-per-roost stays fully valid and is the simplest
default; multichain is available for operators who want the density. The RFC *enables* the option; it
never forces co-location.

## §0 — Proposed brief amendment

Three `CLAUDE.md` edits (exact wording finalized in the closing pass across RFC-0021/0022):

1. **"Multi-nest co-tenancy (a roost)"** — from "N nests … on the same chain, sharing one same-chain
   cursor" to: *a roost is one runtime hosting N nests across **one or more chains**, running **one
   isolated cursor per distinct chain**. The single-cursor law is restated as **per-chain**: never
   multiplex two chains behind one cursor; a cursor is always single-chain, single-writer, one
   observable failure boundary.*
2. **Footprint budget** — from "≤2 GB for a single-chain roost … per-runtime" to: *≤2 GB **per
   active-chain cursor**; a roost's total budget = Σ cursors. The CI gate moves from per-process to
   per-cursor.* (Confirmed 2026-07-21.)
3. **Reorg strategy / "single-cursor non-negotiable"** language — clarified to *per-cursor*: reorgs
   touch only the mutable hot store **of the affected chain's cursor**, isolated from other cursors.

## Motivation

- **Operator density without correctness loss.** Running Base + Arbitrum + Optimism as three separate
  OS processes means three serving ports, three admin surfaces, three supervision targets — for what is
  logically one operator's deployment. One runtime with three isolated cursors is the same correctness,
  far less operational drag.
- **It's the embedded shape of the fleet.** RFC-0022's writer pool schedules *per-chain cursors* across
  machines. Defining and isolating that unit *here*, in one process, is the foundation; the distributed
  version is the same unit, placed remotely.
- **The isolation already mostly exists.** RFC-0012 isolates storage, reorg, and blast radius *per
  nest*. Grouping nests by chain into a cursor, and isolating *per cursor*, is a generalisation of work
  already shipped, not a new invention.

## Goals

1. One runtime hosts nests spanning **≥1 chains**, grouping nests by chain into **one cursor per chain**.
2. **Strict per-cursor isolation**: each cursor has its own tip-follow, finality view, reorg boundary,
   and hot-store partition. One chain stalling or reorging **cannot** affect another chain's nests.
3. **Per-active-chain-cursor RAM budget** (≤2 GB each), CI-enforced per cursor.
4. **Shared where safe**: serving, admin/SSE, registry client, config are shared across cursors; cursor,
   hot store, and reorg state are not.
5. **Opt-in**: single-chain roosts are unchanged and remain the default.

## Non-goals

- **Not multiplexing chains behind one cursor** — forbidden, forever. This RFC adds cursors; it never
  makes one cursor span chains.
- **Not distributed** — everything here is one process. Writer pool, external hot store, and remote
  placement are RFC-0022.
- **Not a mandate** — no operator is pushed to co-locate chains.
- **Not cross-chain joins as a core feature** — a shared serving layer *may* let a query read two
  cursors' data, but cross-chain *derivation*/entities are out of scope here (each cursor derives its
  own chain's state independently).

## Design

### §1 — The multi-cursor runtime

The roost inspects its mounted nests, reads each nest's declared **chain**, and groups them: **one
cursor per distinct chain**. A single-chain roost is the degenerate N=1 case — identical to today.

Each **cursor** owns, privately:
- its RPC/source set and tip-follow loop (one chain's `eth_getLogs`/state extraction);
- its **finality view** (each chain finalizes on its own schedule);
- its **reorg boundary** and hot-store partition (redb namespace / table prefix per cursor);
- its sealing watermark and segment lineage.

The runtime supervises N such cursors concurrently. Nests on the same chain still share *their* cursor
(RFC-0012 co-tenancy, unchanged); nests on different chains never share one.

### §2 — Per-cursor isolation (the load-bearing property)

The failure boundary is **per cursor**:
- A **reorg** on chain A rolls back only cursor A's hot store; cursor B is untouched (property-tested).
- A **stalled RPC** on chain A escalates/alerts for cursor A (RFC-0008/0010 signals) without pausing
  cursor B's ingestion or serving.
- A **runaway** nest on chain A (e.g. a factory explosion, RFC-0009) is bounded within cursor A's
  budget and store; it cannot starve cursor B.

**RAM**: each active-chain cursor is held to ≤2 GB, CI-gated per cursor; the roost's resident total is
Σ cursors and is reported per-cursor in `/metrics` (extending the per-nest labelled series, SEC-9).

### §3 — Shared vs per-cursor: the seam

| Shared across cursors | Private to each cursor |
|-----------------------|------------------------|
| HTTP serving + endpoints (RFC-0010) | cursor / tip-follow loop |
| Admin UI + SSE push (RFC-0010) | finality view |
| Registry client (RFC-0019) | reorg boundary + rollback |
| Config / process supervision | hot-store partition (redb) |
| `/metrics` aggregation (per-cursor series) | sealing watermark + segments |

The serving layer routes a request to the right nest, and thus the right cursor — a purely local case
of the **nest resolution** that RFC-0022 generalises across workers.

### §4 — Scheduling within the runtime

Placement here is trivial (all cursors are local): the roost groups nests → chains → cursors at mount,
and spins/tears cursors as nests are added/removed. The *policy* (which cursor, rebalancing) is a no-op
in one process; RFC-0022 makes it real when cursors are placed across machines. Defining the grouping
cleanly now is what lets 0022 reuse it unchanged.

## Implementation

- Generalise the roost runtime (RFC-0012) from a single shared cursor to a **map of chain → cursor**,
  each cursor owning its store partition, finality, and reorg state.
- Per-cursor `redb` namespacing (or one file per cursor) so rollback and pruning are cursor-local.
- Extend the footprint model + CI gate from per-process RSS to **per-cursor** RSS.
- Serving/admin/registry stay single instances; add cursor-aware routing (already implied by nest→chain).
- No change to the deterministic ingest→decode→seal path *within* a cursor — it is today's path,
  instantiated N times.

## Testing

- **Two-chain roost** (Base + Arbitrum): both cursors index concurrently; served data for each nest
  matches the same nest run **solo** (parity vs two separate processes).
- **Reorg isolation** (property test): a random reorg on chain A converges A to canonical **and leaves
  chain B's hot store byte-identical** — cross-cursor non-interference is the invariant.
- **Stall isolation**: killing chain A's RPC escalates for A only; B keeps serving and ingesting.
- **Per-cursor RAM gate**: each cursor ≤2 GB under a tip-follow + serve load; the CI budget fails if a
  single cursor blows it, independent of the others.
- **Degenerate N=1**: a single-chain roost is byte-identical to today (no regression).

## Risks

- **Budget blowout** — N cursors in one process could collectively exhaust a host. Mitigation: the gate
  is *per cursor* (each still ≤2 GB, guaranteed), and the operator sizes the host for Σ cursors; density
  is explicitly RAM-bounded, not free (founding budget note).
- **Cross-cursor interference** — a bug leaking state/locks across cursors would break isolation.
  Mitigation: per-cursor store partitions + the reorg-isolation property test as a hard gate.
- **Shared serving bottleneck** — one serving layer for N chains' traffic. Mitigation: it's read-path,
  already the cheaper side; RFC-0022 splits it out entirely when scale demands.

## Alternatives considered

- **One process per chain (status quo)** — correct but operationally heavy; the thing operators asked to
  avoid. This RFC is the relaxation.
- **One cursor spanning chains** — rejected on correctness: independent reorg/finality clocks make a
  shared cursor incoherent. Non-negotiable, preserved.
- **Wait for RFC-0022 and do it only distributed** — rejected: the embedded multichain roost is valuable
  on its own, buildable now with no infra, and it's the clean place to define the per-chain-cursor unit
  the distributed mode then reuses.

## Open questions

- Cursor lifecycle on dynamic nest add/remove: when the last nest on a chain unmounts, tear the cursor
  immediately or idle it? (Trivial locally; matters more in 0022.)
- Cross-cursor read queries in the shared serving layer — expose now (read-only, no derivation) or defer?
- Per-cursor vs per-nest metric granularity — confirm the label scheme extends SEC-9 cleanly.
