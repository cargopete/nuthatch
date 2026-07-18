# RFC-0013: Storage and query-engine direction — DataFusion convergence, Turso deferred

- Status: Accepted (2026-07-18) — direction recorded; **§3 (SQL-over-the-tip) shipped**, but via a
  DuckDB hot+cold `UNION` rather than a DataFusion `TableProvider` (see the §3 note below). DataFusion
  convergence (§2) stays the deferred, benchmark-gated destination.
- Author: Pete (cargopete)
- Date: 2026-07-17
- Depends on: RFC-0001 (Implemented — decode registry, per-table model), RFC-0004
  (Implemented — the `nuthatch bench` harness this RFC's gate reuses)
- Blocks: nothing now. Informs the scaled-mode build and any future hot-store swap.
- Priority: explicitly **NOT now**. This is a *direction record*, not a build order.
  Nothing here is touched until after the GraphOps pilot (RFC-0011) and multi-nest
  (RFC-0012) land. The whole point is to decide the direction once, in writing, so it
  is not relitigated from a chat thread — and to *not* destabilise the stack the pilot
  runs on.
- Nature: process/decision RFC (per the RFCs README convention — the standard
  engineering-section structure is adapted).

## Abstract

Two storage/query decisions raised in the 2026-07-17 GraphOps conversation (chris:
"Turso would be great for the hot store"; "are you moving from DuckDB to DataFusion for
federation?"), recorded here:

1. **Hot store stays redb (embedded) / Postgres (scaled).** Turso is a watch-item,
   **deferred** behind the existing `HotStore` trait — not adopted now.
2. **The analytical/query layer converges on DataFusion as the destination** — one
   Arrow-native, pure-Rust engine across both modes — but **benchmark-gated**,
   **built scaled-side first**, and **only after the pilot**. DuckDB stays until the
   numbers say otherwise.

These are one decision, not two: **DataFusion federation is what makes the tip
SQL-queryable** (redb exposed as a `TableProvider`), which dissolves the case for
Turso-in-embedded. So the query-engine call also answers the hot-store question.

## Motivation

The pilot and multi-nest are the near-term work; both run happily on today's stack
(redb hot + DuckDB-over-Parquet cold). But a partner is (rightly) probing the storage
direction, and two attractive-sounding moves — adopt Turso for the hot store, swap
DuckDB for DataFusion — are exactly the kind that feel obvious in a chat and cost a
quarter if taken mid-flight. This RFC fixes the direction and the discipline (measure
first, sequence late) so the answer is on record and the pilot stack is left alone.

## Decisions

### §1 — Hot store: redb (embedded), Postgres (scaled); Turso deferred

**Keep redb for embedded hot.** The hot store holds only *pre-finality* blocks —
everything past finality is sealed to Parquet — so it is a small, hot, mutable window,
not a large dataset. redb's KV point-reads are ideal for that shape, it is pure-Rust
and zero-service (the embedded non-negotiable), and it carries the most
correctness-critical machinery in the system: single-writer, reorg rollback, durable
restart. That is the last place to take a dependency risk on a young engine.

**Postgres for the scaled hot store** — unchanged from the brief, and chris agrees.
It's the component uptime is bet on; maturity beats novelty there. Turso's
Postgres-in-Rust is far too young to carry scaled infra today.

**Turso is deferred, not rejected.** Its appeal for embedded hot is SQL-over-the-tip,
which §3 delivers *without* a store swap. The hot store already sits behind a
`HotStore` trait, so Turso can land later as an *alternative backend* and be A/B'd with
zero drama. Revisit criteria: Turso reaches a production-ready release, its licence
clears the AGPL/no-BSL rule, and a measured win over redb exists that federation
doesn't already provide. Until all three, no.

### §2 — Query engine: converge on DataFusion (as destination)

DataFusion is the correct long-term analytical engine for Nuthatch:

- **One engine, both modes.** Today it is DuckDB (embedded) + DataFusion (planned,
  scaled) — two SQL engines, two dialects, two sets of quirks. A query that behaves one
  way in the pilot behaving differently in scaled is a latent inconsistency and a soft
  violation of "same crates both modes, no forks of business logic." One engine removes
  it.
- **Arrow-native, and the stack is going Arrow.** The transform layer is batched Arrow
  IPC by mandate. DuckDB is the one island that marshals in/out of Arrow; DataFusion
  *is* Arrow. Convergence aligns the query layer with the rest of the system.
- **Federation is native, not bolted on.** Hot (redb / Postgres) and cold (Parquet) as
  `TableProvider`s behind one SQL surface — the direct answer to the federation
  question, in both modes.
- **Pure-Rust, Apache-2.0.** No bundled C++ blob (DuckDB is built with
  `features = ["bundled"]` today), smaller/simpler static binary, cleaner cross-compile,
  a licence already on the brief's safe list.

**DuckDB is not ripped out on preference.** It is faster at heavy OLAP today and it
works. It stays until §4's benchmark gate says DataFusion holds analytical performance
within the ≤2 GB budget.

### §3 — SQL-over-the-tip is a `TableProvider`, not a store swap

The reason the tip isn't SQL-queryable today is that redb is KV, so hot+cold "federation"
is done in app code (the `/table` endpoint's hot+cold merge). The clean fix is a
DataFusion `TableProvider` over redb: the tip becomes queryable through the same
federated SQL surface, the store stays redb. This is the linchpin — it means the
DataFusion decision (§2) *also* satisfies the wish that motivated Turso-for-embedded
(§1), so there is one decision to make, not two.

> **Shipped (2026-07-18) — via DuckDB, not DataFusion.** A dependency spike found DataFusion
> premature to adopt *now*: under the project MSRV (Rust 1.85) cargo resolves DataFusion 48, which
> pulls **arrow 55** and clashes with our **arrow 56** seal/parquet stack (aligning would need an MSRV
> bump to 1.88 or an arrow downgrade), and it adds ~100 crates to a binary that already bundles
> DuckDB — exactly the weight §2/§4 say to benchmark-gate first. So the §3 *goal* was delivered with
> the engine already shipped: for `/sql`, each table's DuckDB view becomes `sealed Parquet UNION ALL
> hot-tip`, where the hot rows are scanned from redb (`Store::hot_rows_by_table`) into a per-table temp
> table typed to match the Parquet (`analytics::load_hot_temp`). Hot and cold are disjoint by block
> (sealed rows are pruned from hot), so the union is exact with no dedup. The tip is now SQL-queryable
> — verified live on Arbitrum (`SELECT … FROM arb__transfer` over unsealed rows) and by hot-only +
> hot∪cold unit tests. A DataFusion `TableProvider` remains the destination when §4's gate is run.

### §4 — Sequencing and the gate (the discipline)

1. **Not during the pilot.** Lodestar runs on DuckDB and works; do not destabilise the
   thing being proven to the partner.
2. **Build DataFusion scaled-side first.** Scaled mode is greenfield — introducing
   DataFusion there adds the engine and yields real federation numbers with **zero risk
   to the working embedded path.**
3. **Benchmark-gate the embedded question.** Point the RFC-0004 `nuthatch bench` harness
   at a DataFusion spike over the same sealed segments. Publish analytical-query latency
   and RSS vs DuckDB. Benchmarks are CI artifacts precisely for calls like this — this
   is measure-then-switch, not a vibes migration.
4. **Collapse embedded onto DataFusion only if the numbers hold.** If they do, retire
   DuckDB → one engine. If DuckDB's OLAP edge matters more than the consistency win,
   keep it in embedded and pay the two-engine tax with a shared SQL-compat/golden test
   suite (the same query, same result, both engines). Let the data decide.

## Non-goals

- Adopting Turso now, in either mode.
- Removing DuckDB now, or before the benchmark gate.
- Any change to the pilot or multi-nest stack.
- A hot store that unifies mutable tip + immutable cold into one engine — the
  hot/cold split (redb mutable, Parquet sealed/immutable) is what keeps reorgs safe and
  is deliberately preserved; DataFusion federates *across* the two, it does not merge
  them into one mutable store.

## Implementation plan (when the time comes)

1. DataFusion as the scaled-mode query surface: `TableProvider`s for Parquet cold and
   the scaled Postgres hot; one federated SQL surface. Greenfield.
2. A redb `TableProvider` (block-range pruning / predicate pushdown; the tip is small so
   this is about correctness, not scan speed) → SQL-over-the-tip in embedded, still on
   DuckDB for cold, to prove the federation shape.
3. Benchmark spike: DataFusion over sealed segments vs DuckDB, via `nuthatch bench`;
   publish latency + RSS; port the `/sql` guard (DataFusion supports query cancellation
   for the timeout, and Arrow-batch limiting for the row cap — the `QueryGuard`
   semantics carry over).
4. Golden SQL-compat suite: the existing analytical tests run against both engines,
   asserting identical results, before any switch.
5. Decision point: retire DuckDB, or keep both behind the suite. Record the outcome
   (amend this RFC).

## Testing and acceptance

- The benchmark gate is the acceptance: DataFusion must hold analytical latency within a
  stated tolerance of DuckDB **and** stay within the ≤2 GB budget on the same segments.
- Result parity: the golden analytical queries (incl. `net_balances` folds, nest views,
  the `/sql` join/aggregate tests) return identical results on DuckDB and DataFusion.
- `/sql` guard parity: timeout-interrupt and row-cap behave identically under DataFusion
  (the RFC's DoS hardening must survive the engine change).

## Risks

- **DataFusion OLAP perf gap.** The core risk, and the reason for the gate. Mitigation:
  measure before switching; keep DuckDB as the fallback if it loses.
- **Migration surface.** `net_balances`, nest `views/*.sql`, the derived
  `_dec`/`_overflow` bigint columns, and the `/sql` guard all touch the engine. Dialect
  differences between DuckDB and DataFusion are real. Mitigation: the golden SQL-compat
  suite is the safety net, and the migration is scaled-first so embedded is untouched
  until proven.
- **Two-engine tax if we keep both.** If DuckDB stays in embedded, we maintain two SQL
  behaviours. Mitigation: the shared golden suite makes the divergence visible; accepted
  only if the perf delta justifies it.
- **Deferring Turso ages badly if it matures fast.** Low cost: it's behind the
  `HotStore` trait, so revisiting is a backend impl, not a re-architecture.

## Alternatives considered

- **Turso for embedded hot (chris's suggestion).** Rejected for now: the tip is tiny by
  construction, redb is proven and carries reorg-critical logic, Turso is
  pre-production, and the SQL-over-tip win comes free from §3. Deferred behind the trait,
  not dismissed.
- **Turso-Postgres for scaled hot.** Rejected: too young for infra uptime is bet on;
  real Postgres is the boring correct choice (brief + chris agree).
- **Keep the DuckDB (embedded) / DataFusion (scaled) split permanently.** Rejected as
  the *default*: it's a latent cross-mode inconsistency. Kept only as the fallback if the
  benchmark says DuckDB's OLAP edge outweighs one-engine consistency.
- **Rip DuckDB out now and go DataFusion everywhere.** Rejected: destabilises the pilot
  for a preference, unmeasured. Scaled-first + gate is the disciplined path.

## Open questions

1. Does the redb `TableProvider` want to expose the tip as one logical union with the
   cold segments (a single `{table}` view spanning hot+cold), or as a separate
   `{table}__tip` the caller unions explicitly? The former is nicer UX; the latter keeps
   the finality boundary visible in SQL. Decide when §2 of the plan is built.
2. If both engines are kept, does the golden suite run in CI on every PR (cost) or
   nightly (latency to catch divergence)? Decide at that point.
