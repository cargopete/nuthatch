# RFC-0022: Distributed scaled mode — read/write planes, a writer pool, dynamic nest placement

- Status: **Accepted** (2026-07-21) — §0 brief amendment applied to `CLAUDE.md` 2026-07-21; design-now-build-later
- Author: Pete (cargopete)
- Date: 2026-07-21
- Depends on: RFC-0013 (the storage/query-engine direction — external hot store + DataFusion federation
  on the scaled side; this RFC is where "scaled mode" stops being a docker-compose sketch), RFC-0019
  (the registry workers pull nests from, and where runtime-secret injection is realized), RFC-0021 (the
  **per-chain cursor** — this RFC's unit of placement; the writer pool is the multichain roost spread
  across machines), RFC-0009/0012 (nest + roost isolation being distributed).
- Blocks: an operator like **GraphOps running nuthatch directly at fleet scale**, without building a
  bespoke platform layer on top.
- Nature: design RFC. **Design now, build later** — the design is committable today; the *build* is
  **dependency-gated** (RFC-0013's scaled-side external hot store + DataFusion federation, and
  RFC-0021's per-chain cursor), **not hardware-gated**. It's a distributed multi-service stack we
  build and integration-test under docker-compose on the MacBook/VPSes; **GraphOps runs it at scale**
  (see the roadmap's Execution-context note). Never blocked on a node to exist.
- Origin: roadmap thread 2, Decision B (`docs/high-level-roadmap-jul-aug-2026.md`), authorized
  2026-07-21.

## ⚠️ Brief amendment required (see §0)

`CLAUDE.md`'s *Out of scope* bins "hosted-SaaS multi-tenancy." This RFC scopes a **distributed
self-hosted** mode **in** while keeping hosted-SaaS **out** — the line is billing/metering/authz between
mutually-untrusting paying customers, which stays the **gateway's** job. §0 is the proposed nuance; it
must be accepted before build.

## Abstract

RFC-0021 put many chains in one runtime. This RFC puts many runtimes across many machines, under one
control plane, so one operator can scale horizontally:

- **Separate the read plane from the write plane.** **Writer workers** ingest/decode/derive/seal;
  **query-frontend (FE) nodes** serve. They scale independently.
- **Force an external hot store.** redb is embedded-only; scale mode requires an external hot store
  (Postgres, per RFC-0013's scaled side). Object storage (segments) is already shared and needs no
  change.
- **Place nests dynamically.** A **control-plane DB** holds the desired state; nests are added/removed
  **via API**; a **scheduler** balances **per-chain cursors** (RFC-0021's unit) across the writer pool.
- **Resolve any nest at the FE.** A query-FE node accepts a request for *any* nest and knows how to
  **resolve** it to the right version (RFC-0020) and backend.

**The scope line — the keystone.** GraphOps is *one operator* running a fleet of cooperating nests it
chose — **not** a landlord to mutually-untrusting paying customers. The core provides the distributed
*substrate*; it **never** grows per-tenant billing, metering, or authz between untrusting customers.
Hold this line and it's fleet scaling; cross it and we've built the SaaS platform the brief bins.

## §0 — Proposed brief amendment

`CLAUDE.md` *Out of scope* gains an explicit nuance (final wording in the closing pass): *Distributed
**self-hosted** scaled mode — one operator, a writer pool + query-FE tier + control-plane over
cooperating nests — is **IN scope** (RFC-0022). **Hosted-SaaS multi-tenancy** — per-tenant
authz/quotas/billing and isolation between mutually-untrusting **paying** customers — remains **OUT**;
that is the gateway's job, in front of nuthatch.* The single-cursor law (now per-chain, RFC-0021)
still holds under distribution: a cursor is single-chain and owned by exactly **one** worker at a time.

## Motivation

- **One box isn't always enough.** An operator following many chains / hosting many nests will exceed a
  single multichain runtime's RAM (Σ cursors, RFC-0021). Horizontal scale-out is the answer, and it's
  the explicit ask: "so GraphOps can just use it."
- **Read and write scale differently.** Backfill/tip ingestion is write-heavy and bursty; serving is
  read-heavy and latency-sensitive. Coupling them wastes resources. Splitting the planes lets each grow
  to its own load.
- **The substrate already points here.** RFC-0013 named the external hot store + DataFusion federation
  for the scaled side; RFC-0021 defined the placeable unit. This RFC assembles them into an operable
  distributed system, no new founding concepts required.

## Goals

1. **Plane split**: independent **writer workers** (ingest→decode→derive→seal) and **query-FE nodes**
   (serve entity reads + SQL), scalable independently.
2. **External hot store mandatory in scale mode** (Postgres, RFC-0013), replacing redb; segments stay in
   shared object storage.
3. **Writer pool + scheduler**: place and rebalance **per-chain cursors** (RFC-0021) across workers,
   respecting *one cursor, one owning worker*.
4. **Dynamic, API-driven nest lifecycle**: add/remove nests at runtime; state persisted in a
   **control-plane DB**.
5. **Nest resolution at the FE**: any FE node resolves any nest → version/endpoint (RFC-0020) → backend.
6. **Runtime-secret injection** (credential kind **b**, RFC-0019 §4): per-nest secrets held in the
   control-plane, injected to a worker at mount, **never** in a bundle.
7. **Hold the scope line**: no billing/metering/per-untrusting-tenant authz in the core.

## Non-goals

- **Hosted-SaaS multi-tenancy** — per-tenant authz/quotas/billing, isolation between mutually-untrusting
  paying customers. The gateway's job. Out.
- **A second cursor per chain / cursor sharding across workers** — a chain's cursor is single-chain
  (RFC-0021) and owned by **one** worker; we never split one chain's cursor across workers or run two.
- **Kubernetes/Helm as the deliverable** — the brief allows binary + compose only. This RFC designs the
  *system*; packaging beyond compose is out.
- **A new query engine** — the FE federates hot (Postgres) + cold (segments) via DataFusion per
  RFC-0013; it doesn't invent one.

## Design

### §1 — The two planes

**Writer worker**: owns a set of **per-chain cursors** assigned by the scheduler. For each cursor it
runs today's deterministic ingest→decode→derive→seal, writing the **hot store to Postgres** (RFC-0013)
and **sealed segments to object storage** (shared, immutable, content-addressed). A cursor lives on
exactly one worker at a time (the single-writer invariant, now enforced by assignment).

**Query-FE node**: stateless-ish serving. Answers entity point-reads and SQL by federating the external
hot store (recent, Postgres) with sealed segments (object storage) through **DataFusion** (RFC-0013
§2/§4). Any FE node can serve any nest because state lives in the shared stores, not on the node.

The planes share the external hot store + object storage; they do **not** share process or host.

### §2 — The writer pool and the scheduler

The **scheduler** reconciles *desired* nests (control-plane DB) with *actual* cursor→worker assignments:

- Unit of placement = **per-chain cursor** (RFC-0021). Nests on the same chain co-locate on the same
  worker (they share a cursor); nests on different chains may land on different workers.
- **Balancing**: spread cursors across workers by load (RAM = Σ assigned cursors ≤ worker budget, tip
  lag, backfill demand). Rebalance on worker join/leave or hotspot.
- **Single-owner guarantee**: a cursor is assigned to exactly one worker; handoff (drain → reassign) is
  explicit, never concurrent — preserving single-writer + one-observable-failure-boundary under
  distribution.

### §3 — Dynamic, API-driven nest lifecycle + the control-plane DB

A **control-plane API** (add/remove/inspect nests) writes desired state to a **control-plane DB**
(distinct from the Postgres *hot store* — one holds *what should run*, the other holds *indexed data*).
The scheduler watches it and converges. Adding a nest: resolve from the registry (RFC-0019) → assign its
cursor to a worker → worker pulls the bundle, injects secrets (§4), mounts, begins indexing. Removing:
drain the cursor, tear down, free budget. No process restarts; it's API-driven and continuous.

### §4 — Nest resolution (promoted, cross-cutting)

Flagged twice in the source notes, so first-class here. Given an incoming request or a placement:

```
request/placement for "foo"
  → nest name → version           (RFC-0020: compatible-latest, or a pinned/breaking endpoint)
  → version   → bundle hash        (RFC-0019 index)
  → backend   → owning worker      (scheduler assignment)  [for writes]
              → shared stores      (Postgres + segments)   [for reads, any FE node]
```

Reads need no worker affinity (state is in shared stores); writes follow cursor ownership. RFC-0020's
compatible/breaking endpoints ride this resolution: a breaking version resolves to a distinct endpoint,
a compatible one hot-swaps behind the same one — across the fleet, not just one box.

### §5 — Runtime-secret injection (credential kind **b**)

RFC-0019 §4 committed the *rule* (secrets never in a bundle); this RFC provides the *mechanism*. The
control-plane holds per-nest secrets (private RPC URLs, enricher API keys) in a secret store, keyed by
nest. At mount, the scheduler hands the assigned worker only that nest's secrets, out-of-band from the
content-addressed bundle. Rotating a secret is a control-plane op; it never changes a bundle hash.

### §6 — The scope line, made concrete

What the core **does**: place, serve, resolve, isolate, inject secrets, balance load — for **one
operator's** cooperating nests. What the core **must not** grow: per-tenant billing, metering, quota
enforcement between untrusting customers, or customer-facing authz. Those belong to the **gateway** in
front. If a feature request only makes sense when the tenants *don't trust each other and pay*, it's out
of this RFC and this project.

## Implementation (design-now, build-later)

- Feature-flag the storage backend behind the existing `HotStore` trait (founding architecture):
  `Postgres` for scale mode, `redb` for embedded — no `#[cfg]` forks of business logic.
- Writer-worker binary/role and query-FE role from the same crates; a role flag, not a fork.
- Scheduler + control-plane API + control-plane DB as new components (compose services), watching desired
  state and reconciling cursor assignments.
- DataFusion federation at the FE per RFC-0013 §2/§4 (benchmark-gated there).
- Ship as **docker-compose** (writer pool + FE tier + Postgres + control-plane), honoring "binary +
  compose only."

## Testing

- **Plane split**: writers ingest while FE nodes serve; scaling FE nodes changes serving throughput
  without touching ingestion, and vice versa.
- **Placement/rebalance**: adding a worker rebalances cursors; a cursor is *never* owned by two workers
  concurrently (single-owner invariant, asserted).
- **Dynamic lifecycle**: API add/remove a nest with no restart and no impact on other nests' cursors.
- **Resolution**: any FE node resolves + serves any nest; a breaking-version endpoint and its
  compatible-latest sibling both resolve correctly across nodes (RFC-0020 parity, distributed).
- **External-hot-store parity**: served results under Postgres match the embedded redb path for the same
  nest + range (backend-swap must be invisible).
- **Secret isolation**: an injected secret never appears in any bundle or segment; a worker only ever
  receives its assigned nests' secrets.

## Risks

- **Crossing the scope line** — the defining risk. Mitigation: §6 states the line; the non-goal is
  explicit; anything requiring untrusting-tenant authz/billing is refused here and pointed at the
  gateway.
- **Single-cursor invariant under distribution** — a scheduler bug double-assigning a cursor breaks
  single-writer. Mitigation: explicit single-owner assignment + drain-before-reassign handoff, asserted
  in test.
- **Control-plane as SPOF** — its outage shouldn't stop running cursors serving/ingesting. Mitigation:
  workers keep running their last-assigned cursors if the control-plane is briefly unreachable
  (desired-state convergence resumes on reconnect); the control-plane is not in the data path.
- **Complexity + footprint** — a distributed system is a lot of moving parts; keep embedded mode a
  first-class, unaffected default (the brief's primary deliverable is still the single binary).

## Alternatives considered

- **Leave scale-out to the gateway / a bespoke platform** — considered and *declined* 2026-07-21: the
  operator ask is to run nuthatch directly at fleet scale. We provide the substrate, not the SaaS.
- **Shard a chain's cursor across workers for throughput** — rejected: breaks single-writer + single
  observable failure boundary. Throughput scales by adding *chains/nests* across workers, never by
  splitting one chain.
- **Keep redb in scale mode** — impossible; embedded-only. External hot store is forced (as the notes
  said).
- **One coupled scale binary (no plane split)** — wastes resources given read/write asymmetry; rejected.

## Open questions

- Scheduler policy specifics (bin-packing by RAM/lag, rebalance thresholds, anti-flap).
- Control-plane DB choice + whether it co-locates with or is distinct from the Postgres hot store
  (leaning distinct: desired-state vs indexed-data are different lifecycles).
- FE caching/resolution layer: does the FE cache nest resolution, or always hit the control-plane/index?
- Secret-store backend (control-plane native vs external KMS/Vault) for kind-(b) injection.
