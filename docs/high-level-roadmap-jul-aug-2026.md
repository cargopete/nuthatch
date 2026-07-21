# High-level roadmap — Jul–Aug 2026

**Status: DONE (2026-07-21) — strategy agreed, RFCs [0019](rfcs/0019-nest-registry-and-distribution.md)–[0023](rfcs/0023-contract-state-eth-call-derive-first.md)
Accepted, `CLAUDE.md` amended** (footprint budget → per-cursor; roost → multichain; reorg → per-cursor
isolation; Out-of-scope → distributed-self-hosted IN / hosted-SaaS OUT). Implementation follows the RFC
build order; this doc is now historical record.

This is the working strategy doc for the next planning window. It captures raw direction from
conversations, groups it into coherent threads, flags where a thread rubs against a `CLAUDE.md`
non-negotiable, and proposes an RFC slate. **We agree the strategy here first; only then do the
RFCs (0019+) get written.** Companion to [backlog.md](backlog.md) (what's left in 0001–0018) and
[rfcs/README.md](rfcs/README.md) (what each RFC is).

Started 2026-07-21.

---

## Source material (raw notes, verbatim)

From conversations, lightly de-duplicated:

- shared db, centralised, hosted
- eth calls / eth call data
- there could be an eth_call-optimized EVM executor that's **not** a generic archive node (which
  does many things besides call handling); strip everything but call handling → more performant,
  easier
- **on multitenancy:**
  - separate read path from write path
  - allow number of write workers to be scaled up
  - needs balancing — balancing nests across an available worker pool
  - query FE side can accept requests for any nest and knows how to resolve those
  - object/S3 no problem; hot store — embedded db no bueno
  - "scale mode" may need to **force** an external hot store
  - both a writer pool and query-FE nodes
  - taking out / putting in nests: dynamic, API-driven, backed by a db
  - nest resolution important
- nests as packages
- private credentials
- user publishes a **private** nest to the registry, and nuthatch pulls that
- nest registry etc. should be **decoupled** from nuthatch
- object storage — writes the bundle to object storage; object storage **or** nests on disk
- the **N-1 problem** of upgrades should be fixed — huge issue with subgraphs
- a nest updates itself, whether the update has schema changes or not
  - if update is **compatible** → UX is to just serve it on the same endpoint
  - **breaking** change → different endpoint

---

## Execution context — who builds, who runs

- **We build and validate on modest kit:** the MacBook (primary), a couple of VPSes, and (via
  Tailscale) the ThinkPad — `ssh pepe@pepe-thinkpad.tailb0627.ts.net`. Everything here must be buildable
  and integration-testable there: embedded on one machine, or the distributed stack under docker-compose
  on a VPS or two.
- **GraphOps runs it at scale.** When the work lands, Chris / GraphOps runs it on GraphOps infra. We do
  **not** self-provision a production fleet or archive nodes.
- **Consequence for sequencing:** "infra-gated" in this doc means **dependency-gated** (an RFC waiting on
  another RFC's substrate), **not hardware-gated**. The distributed mode (0022) is a compose stack we
  exercise on a VPS — not blocked on hardware to exist. The only genuinely hardware-heavy pieces (a
  synced archive node for 0003/0014; 0023's tier-3 eth_call fallback source) are GraphOps-infra concerns
  to hand off, never blockers we self-host.

## Notes → thread coverage map

Every raw note, mapped to where it's handled — so nothing is orphaned. (Reconciled 2026-07-21.)

| Note (paraphrased) | Lands in |
|--------------------|----------|
| separate read path from write path | Thread 2B |
| scale up write workers | Thread 2B — writer pool |
| balance nests across the worker pool | Thread 2B — scheduler/placement |
| query FE accepts any nest, resolves it | Thread 2B **+ Nest resolution (cross-cutting)** |
| object/S3 fine; embedded hot store no bueno | Thread 2B — external hot store |
| scale mode forces an external hot store | Thread 2B |
| both writer-pool and query-FE nodes | Thread 2B — deployment topology |
| dynamic add/remove nests, API-driven, db-backed | Thread 2B — control-plane DB |
| **nest resolution important** | **Cross-cutting (2B / 3 / 4)** — promoted, see below |
| nests as packages | Thread 3 |
| private credentials | Thread 3 — **two kinds, one was a gap (see below)** |
| publish a private nest to the registry | Thread 3 |
| nuthatch pulls that | Thread 3 |
| registry decoupled from nuthatch | Thread 3 |
| writes the bundle to object storage; or on disk | Thread 3 |
| N-1 upgrade problem (subgraph pain) | Thread 4 |
| update with/without schema changes | Thread 4 |
| compatible → same endpoint | Thread 4 |
| breaking → different endpoint | Thread 4 |

**Two items promoted from "buried" after this check:**

1. **Nest resolution** — you flagged it *twice* ("query FE… resolves those" **and** "nest resolution
   important"), so it earns first-class, cross-cutting status: given an incoming request, resolve
   **nest name → version (compat/breaking endpoint, thread 4) → backend worker (thread 2B)**, using the
   **registry (thread 3)** as the catalogue. It touches all three of 2B/3/4 — likely its own section in
   RFC-0021, not a footnote.
2. **Credentials are two different things** — your "private credentials" note covers only the first:
   - **(a) Registry auth** — creds to publish/pull *private nests*. Covered in thread 3.
   - **(b) Nest runtime secrets** — creds a *running* nest needs (private RPC endpoints, enricher API
     keys for effectful components). In the distributed mode these must be injected **per-nest,
     per-worker**, never baked into a bundle. This was **not** captured; now flagged in thread 3.

## The five threads

### Thread 1 — Contract state (eth_call) ✅ **AUTHORIZED 2026-07-21 (derive-first now; fallback/cache designed)**

**The gap:** the Foundation says **>70% of subgraphs use `eth_call`** — reading contract *state* the
event alone doesn't carry (`getReserves`, `totalSupply`, `balanceOf`, `decimals`, arbitrary views).
Today nuthatch indexes event *logs* only. We *do* have `eth_call`/`eth_getStorageAt` plumbing, but at
`latest` and only for init-time ABI/proxy introspection — not historical, not in the data path.

**The reframing (the differentiator):** most subgraph `eth_call`s exist because subgraphs have **no
incremental-view engine** — they can't *derive* state, so they *fetch* it. Nuthatch's DBSP/IVM core
was built for exactly this. We already prove it: **balances are derived** (`BalanceView` from
`Transfer` events), never `balanceOf`-ed. So the play isn't "add eth_call" — it's **turn most eth_call
into declarative derivation, and keep a real fallback for the residue.**

**Tiered model:**
1. **Derive it** (free, deterministic, no archive node) — view recipes for the common reads (reserves,
   supply, balances). The IVM core doing its job. **Buildable now. The differentiating half.**
2. **Cache immutable metadata** — `decimals/symbol/name`: call once, content-address, cache forever.
3. **eth_call fallback for the irreducible** — genuinely dynamic/non-derivable reads (oracle price,
   ungoverned param, arbitrary view on a contract we lack full event coverage of). Batched `eth_call`
   **at a pinned historical block**, results content-addressed and **sealed to segments, versioned like
   decodings**. RPC/archive-sourced first; local execution (reth ExEx / stripped executor) later.

**Determinism & purity placement (clean):** an `eth_call` result is a pure function of (code, storage,
block, calldata) — deterministic **given a pinned block** (never `latest` in the data path). It belongs
to **ingestion, not the transform layer**: the *host* extracts call results like it extracts logs, pins
+ content-addresses them, and feeds components *Arrow input*. Components never issue calls → stay
zero-capability and pure. eth_call is an extraction source, not a component effect.

**The hosted "state cache" (Chief's pull/query idea) — optional & verifiable only:** a historical
call-result at (chain, block, contract, calldata) is immutable, content-addressable, expensive to
produce, cheap to verify (re-execute one call). Perfect for a **shared pull-through cache**: one
operator produces it once, others pull the sealed segments. Guardrails from the non-negotiables:
- *"No mandatory third-party data dependency / be your own indexer"* → the hosted store is an **optional
  acceleration (a CDN), never required**. You can always produce it yourself from an archive node.
- *"Verifiability = deterministic re-execution + content-addressed segments"* → trusting the hosted set
  needs **no trust**; spot-verify by re-running. Sovereign by construction.
- **Same object-storage + content-addressing infra as the nest registry** (thread 3 / 0019): a nest
  bundle and a call-result segment are both immutable addressed blobs. One substrate, two payloads.

**Relation to existing work:** tier 1 extends the DBSP core (slice 3). Tiers 3/4 are the RPC-side
sibling of RFC-0014 (state via ExEx); RFC-0003 (reth) is the substrate for local execution.

**Scope for the window:** tiers 1+2 **now** (0023). Tiers 3 (fallback) + 4 (hosted verifiable cache)
**designed now, built later** — fallback RPC-buildable soon; the **stripped call-optimized executor is
a research note against RFC-0014/0003**, not a build this window.

**Open questions (design, not scope):**
- The view-recipe library: which reads ship as first-party derivations, and how are they authored
  (reuse RFC-0018 §1 authored SQL views)?
- Fallback: how does a nest *declare* an irreducible call, and how is "irreducible vs derivable"
  surfaced to the author (lint/hint)?
- Cache segment format + addressing key, and how pull-through degrades offline (warn-and-skip).

---

### Thread 2 — Multichain roosts + scaled mode

Split into two decisions: **A) the multichain roost** (embedded; authorized) and **B) the distributed
scaled plane** (the bigger scope question; still open).

#### Decision A — Multichain roost ✅ **AUTHORIZED 2026-07-21**

Chief's call: a single roost **may** host nests across **multiple chains** — e.g. one Base nest + one
Arbitrum nest in one runtime.

**This is a capability, not a mandate.** Operators decide how to run their roosts: one-chain-per-roost
remains fully valid (and the simplest default); multichain is available for those who want it. The RFC
enables the option; it never forces co-location.

**This overturns a current `CLAUDE.md` non-negotiable** ("A second chain means a second cursor means a
second process; never multiplex chains behind one cursor") — but only the *implementation stance*, not
the correctness invariant:

- **PRESERVED (the real law):** a **cursor is single-chain**. One cursor tracks exactly one chain's
  canonical history; we never multiplex two chains behind one cursor. Chains reorg, finalize, and
  advance on independent clocks — sharing a cursor is incoherent, and stays forbidden.
- **RETIRED (the implementation habit):** "second chain ⇒ second *process*." A roost becomes **one
  runtime hosting N cursors — one isolated cursor per distinct chain** among its mounted nests —
  behind one shared serving/admin/registry layer.

**Costs to rule on (both touch non-negotiables):**
1. **RAM budget.** Today: "≤2 GB per *single-chain* roost, per-runtime." A multichain runtime can't
   hold N chains under one 2 GB ceiling without starving each. **Proposed:** budget becomes
   **per-active-chain-cursor (≤2 GB each)**; roost total = Σ cursors. Keeps each chain's guarantee
   intact; CI gate moves from per-process to per-cursor. → **CONFIRMED 2026-07-21.**
2. **Failure isolation.** "One observable failure boundary" now means *per cursor*: one chain stalling
   or reorging cannot harm another chain's nests. Same per-nest isolation the multitenancy work needs
   anyway — RFC-0012 already isolates storage/reorg/blast-radius per nest.

**`CLAUDE.md` amendments required** (exact wording pending Chief's OK): the "Multi-nest co-tenancy"
paragraph, the footprint-budget paragraph, and the reorg "single-cursor" language.

#### Decision B — Distributed scaled plane ✅ **AUTHORIZED 2026-07-21**

Chief's call: **yes** — nuthatch grows a *distributed self-hosted* mode (writer pool across machines,
external hot store, query-frontend tier, dynamic API-driven nest placement) so an operator like
GraphOps can run it directly, without a bespoke platform layer on top.

**What the notes describe:** separate read plane (query FE) from write plane (ingest workers);
scale the writer pool; a control-plane DB that places/balances nests across workers dynamically,
API-driven; an external hot store (redb is embedded-only, so scale mode *forces* Postgres);
query FE resolves any nest → its backend.

**The tension:** the very first note was "shared db, centralised, hosted." A writer pool + query-FE
+ dynamic API-driven nest placement **is the architecture of a multi-tenant platform**. `CLAUDE.md`
draws a hard line:

- **In scope** — a *roost*: cooperating nests an operator picked, one chain, one cursor, one process.
- **Out of scope** — *hosted-SaaS multi-tenancy*: per-tenant authz/quotas/billing, isolation between
  mutually-untrusting paying customers ("the become-a-data-service-company path, and the gateway's
  job regardless").

**Reading of the notes:** nothing here mentions billing, metering, or per-paying-customer isolation.
"Private credentials" is about **nest-author** privacy, not customer walls. So this reads as *one
operator scaling their own fleet of roosts horizontally* — defensible scaled-mode engineering, not
the data-service path.

**Guardrails that keep it honest (proposed):**
- **Cursor stays single-chain (per Decision A).** The scheduler's unit is the **chain-cursor**: it
  places per-chain cursors onto workers and balances them. Multichain roost = the embedded case; the
  writer pool = the scaled case — same model, two scales. Never multiplex chains behind a cursor.
- **No billing/metering/per-tenant authz in the core.** That stays the gateway's job.

**The scope line (what keeps this self-hosted, not hosted-SaaS) — the keystone:** GraphOps is *one
operator* running a fleet of cooperating nests, **not** a landlord to mutually-untrusting paying
customers. The core gets the distributed *substrate* (planes, pool, scheduler, control-plane DB) but
**never** per-tenant billing, metering, or authz between untrusting customers — that stays the
**gateway's** job, in front of nuthatch. Hold this line and it's fleet scaling; cross it and we've
built the SaaS platform the brief bins. → must land in `CLAUDE.md`'s *Out-of-scope* section:
distributed self-hosted scaled mode = IN; hosted-SaaS multi-tenancy = still OUT.

**Open questions (design, not scope):**
- Scheduling granularity = the **chain-cursor** (per Decision A): the scheduler places per-chain
  cursors onto writer workers; a multichain roost is just co-scheduled cursors sharing a serving tier.
  Confirm the placement/rebalancing policy in 0021.
- Does the query FE need its own resolution/cache layer, or delegate resolution to the registry
  (thread 3)?
- Control-plane DB: new component, or reuse the scaled-mode Postgres hot store?
- External hot store is **mandatory** in scale mode (redb is embedded-only) — Postgres now, per
  RFC-0013's scaled-side direction. DataFusion federation (0013 §2/§4) is the query-FE's read path.

---

### Thread 3 — Nest registry & distribution

**What:** the natural step after RFC-0012 (we already ship content-addressed nest bundles). Add a
**registry**: publish/pull nests as packages, private nests + credentials, an S3-backed bundle store
(object storage **or** on-disk), resolution `name/version → bundle`.

**Key principle from the notes:** the registry is **decoupled** from the nuthatch binary. nuthatch
*pulls*; it does not *become* the registry. Bundles live in object storage; nuthatch fetches and
mounts.

**Relation to existing work:** direct extension of RFC-0012's portable bundles. Feeds thread 2
(query FE resolution) and thread 4 (versioned updates).

**Two kinds of credentials (don't conflate them):**
- **(a) Registry auth** — creds to publish/pull *private nests*. Registry's problem.
- **(b) Nest runtime secrets** — creds a *running* nest needs (private RPC endpoints, enricher API
  keys for effectful components). Injected **per-nest, per-worker** at mount time in the distributed
  mode; **never** baked into a content-addressed bundle (that would leak them and break addressing).

**Open questions:**
- Registry protocol: OCI-style (reuse container registries)? Custom? Just an S3 bucket + index?
- Private-nest auth (a): who holds credentials, and how does a puller present them?
- Runtime-secret injection (b): where do per-nest secrets live in the control-plane, and how are they
  handed to a worker at mount without touching the bundle?
- Content-addressing already gives us immutability + dedup — does the registry add naming/versioning
  on top, or more?

---

### Thread 4 — The N-1 upgrade problem (the differentiator)

**What:** subgraphs' worst UX — deploy v2, it resyncs from genesis, you run v1 **and** v2 in parallel
burning double resources until v2 catches up, then flip. Everyone who's run subgraphs has felt this.

**The fix:** schema-diff a nest update.
- **Compatible** change → hot-swap, serve on the **same** endpoint, reusing sealed content-addressed
  segments so there's **no full resync**.
- **Breaking** (schema) change → **new** versioned endpoint, run alongside, deprecate the old.

**Why it matters:** arguably the single most differentiating item in the whole pile. Content-addressed
immutable segments are exactly the substrate that makes segment-reuse-across-versions possible — a
capability subgraphs structurally can't have.

**Relation to existing work:** builds on RFC-0012 (bundles), RFC-0018 §1 (authored SQL views / schema),
and thread 3 (registry as the source of versions).

**Settled definition (2026-07-21):** the predicate is *backward-compatibility for downstream consumers*
(e.g. Lodestar), not "zero schema delta."

- **Compatible** = every existing downstream query/subscription keeps working with unchanged meaning.
  Covers internal-only changes (decode fixes, perf, view refactors yielding identical output) **and
  purely additive** schema changes (new column/table/view, nothing existing touched). → operator
  indexes the new version, then **hot-swaps on the same endpoint** when caught up; consumer notices
  nothing.
- **Breaking** = anything a consumer can observe as removed, renamed, retyped, or semantically
  changed. → **new versioned endpoint**, both versions run in parallel, app devs migrate on their own
  clock, old endpoint deprecated afterwards.

Both cases index the new version; segment reuse (below) is an orthogonal optimization to cut that
cost, not part of the definition.

**Open questions:**
- **Detection/authority** — is compatible-vs-breaking auto-detected by schema-diff, declared by the
  nest author (semver-style), or diff-proposes-author-confirms? (0020 design detail, not strategy.)
- Endpoint aliasing / deprecation lifecycle — how long do old endpoints live?
- Segment reuse: can we reuse sealed segments across versions to avoid a full resync? Interaction with
  versioned decodings (we never re-decode history) when the schema is unchanged but decode logic bumps.

---

## Proposed RFC slate (0019+)

| RFC | Title | Builds on | Scope |
|-----|-------|-----------|-------|
| **0019** | Nest registry & distribution — publish/pull, private nests, S3-backed bundle store, registry-auth | 0012 | ✅ clean |
| **0020** | Nest lifecycle & the N-1 upgrade — compat/breaking, schema-diff, segment reuse, endpoint aliasing | 0012, 0018 §1, 0019 | ✅ clean, high value |
| **0021** | Multichain roost (Decision A) — one runtime, N per-chain cursors, per-cursor budget & isolation | 0012, 0009 | ✅ settled (embedded; buildable now) |
| **0022** | Distributed scaled mode (Decision B) — read/write plane split, writer pool, query-FE tier, control-plane DB, nest resolution, runtime-secret injection | 0013, 0019, 0021 | ✅ settled scope; **dependency-gated** (0013-scaled + 0021), distributed compose build — not hardware-gated |
| **0023** | Contract state (eth_call) — derive-first view recipes + metadata cache (tiers 1+2); pinned-block fallback + hosted verifiable cache designed (tiers 3+4) | 0018 §1, 0001, 0019; adj. 0003/0014 | ✅ settled (tiers 1+2 buildable; executor = note vs 0014) |

_Note: split the old 0021 into **0021 (multichain roost, embedded)** and **0022 (distributed scaled
mode)** — Decision A is buildable on a laptop now; Decision B is the multi-machine build. eth_call
shifts to **0023**._

**Sequencing logic:**
1. **0019 → 0020 first.** Sovereignty-respecting, laptop-buildable, no infra needed, and they hit the
   subgraph pain everyone actually feels. Highest value per unit risk.
2. **0021 (multichain roost) next.** Embedded, buildable now; unlocks the co-tenancy story without the
   distributed-systems weight. Gated only on the per-cursor budget/isolation work.
3. **0022 (distributed scaled mode) later.** The heavy one — external hot store (RFC-0013 scaled side),
   DataFusion federation, plane split, scheduler, control-plane DB. Depends on 0019 + 0021.
4. **0023 split.** eth_call ingestion half buildable now; executor spike overlaps the reth-node blocker.

---

## Decisions log

Record agreements here as we settle them, so the eventual RFCs inherit a clear brief.

| Date | Decision | Notes |
|------|----------|-------|
| 2026-07-21 | Roadmap doc created; strategy to be agreed before any RFC is written | — |
| 2026-07-21 | **Multichain roost authorized (Thread 2, Decision A).** One runtime may host nests across multiple chains = one isolated cursor per chain. **A capability, not a mandate** — operators choose; one-chain-per-roost stays valid & default. Preserves cursor-is-single-chain; retires "second chain ⇒ second process." | Decision B (distributed plane) still open. |
| 2026-07-21 | **RAM budget confirmed per-active-chain-cursor (≤2 GB each);** roost total = Σ cursors; CI gate moves per-process → per-cursor. | — |
| 2026-07-21 | **Distributed scaled mode authorized (Thread 2, Decision B).** Writer pool across machines + external hot store + query-FE tier + dynamic API-driven nest placement, so GraphOps can run nuthatch directly. **Scope line:** core provides the substrate; per-tenant billing/metering/authz stays the gateway's job. | CLAUDE.md *Out-of-scope* needs the distributed-self-hosted=IN / hosted-SaaS=OUT nuance. |
| 2026-07-21 | **`CLAUDE.md` amendments deferred** until the RFC set is pinned down — one clean constitutional pass at the end, not piecemeal. | Affected clauses noted under Decision A. |
| 2026-07-21 | **Thread 4 compatible-vs-breaking settled.** Predicate = backward-compat for downstream consumers; additive schema = compatible (same endpoint), observable removal/rename/retype/semantic change = breaking (new endpoint, parallel run). **Additive-is-compatible confirmed** (not stricter zero-delta). | Detection authority (auto-diff vs author-declared) deferred to 0020 design. |
| 2026-07-21 | **Private nests confirmed in scope.** The registry (0019) ships with auth + private bundles from day one — not a public-only catalogue. | Registry-auth (creds kind *a*) is core to 0019. |
| 2026-07-21 | **Thread 1 (contract state / eth_call) settled — derive-first.** >70% of subgraphs use eth_call; most is *derivable* via the IVM core (not fetchable in subgraphs). Model: (1) derive via view recipes, (2) cache immutable metadata, (3) eth_call-at-pinned-block fallback for the irreducible, sealed+versioned. eth_call = *ingestion* concern, host-side, components stay pure. Hosted "state cache" = optional + verifiable only (CDN, never a mandatory dependency), reusing registry object-storage infra. **Window scope:** tiers 1+2 now (0023); tiers 3+4 designed-now-built-later; stripped executor = note vs RFC-0014/0003. | Turns "can't do the 70%" into "derives what subgraphs pay archive nodes to fetch." |

## Open decisions

**Remaining strategy decisions:** _none — all threads settled 2026-07-21._ Strategy phase closed; RFC
authoring unblocked.

**Not blockers — resolved *inside* the RFCs as design work** (listed so they're not forgotten):
- Registry protocol (0019) — OCI / custom / S3-bucket-plus-index.
- Compat-vs-breaking detection authority (0020) — auto-diff / author-declared / propose-confirm.
- Runtime-secret injection mechanism (0019/0022) — per-nest, per-worker, never in the bundle.
- Scheduler placement/rebalancing policy + nest resolution (0022).
- View-recipe library + "irreducible vs derivable" author hints; cache segment format/addressing (0023).

**Deferred until the RFC set is agreed** (then handled in one pass): the `CLAUDE.md` amendments —
Decision A (co-tenancy, footprint budget, reorg single-cursor clauses) **and** Decision B (the
*Out-of-scope* nuance: distributed self-hosted scaled mode IN, hosted-SaaS multi-tenancy OUT).
