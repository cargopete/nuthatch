# Infra track & RFC leftovers (0001–0014)

Everything deferred, parked, or not-yet-done across the RFC series, in one place — so the leftover
work isn't scattered across fourteen "Non-goals" and "Open questions" sections. Companion to the
[RFC index](rfcs/README.md); that table says *what each RFC is*, this says *what's left*. For the
release gate — what must be true before a build is pointed at a real workload unattended — see the
[production-readiness checklist](prod-readiness.md).

Reconciled against the RFCs + [progress log](progress-log.md) on 2026-07-18.

## TL;DR

The buildable-on-a-laptop backlog is essentially cleared — RFCs 0001, 0002, 0004, 0005, 0008, 0009,
0010, 0012 are Implemented, and 0013 §3 (SQL-over-the-tip) shipped. What remains falls into four
tracks:

1. **Infra track** — one thing gates a lot: a **colocated reth node**. It unblocks 0003 (ExEx), which
   unblocks 0014 (firehose traces/state). This is provisioning + sync time, not coding.
2. **Deferred engineering** — real code, but gated on infra (0003, 0014) or on a benchmark (0013
   DataFusion). Not "todo", "not yet".
3. **Process / ongoing** — non-code: grants (0006), launch (0007), the full graph-network migration
   (0011, parked after the pilot).
4. **Small increments** — buildable now, low priority (proxy introspection, child-`end` conditions,
   SSE push, the 0012 live-parity proof).

## The whole backlog at a glance

| RFC | Status | What's left | Blocked on |
|-----|--------|-------------|-----------|
| 0001 Decode/nests | Implemented | Proxy/EIP-1967 impl introspection (follow proxy slots) | — (small, buildable) |
| 0002 Horizon nest | Implemented | — | — |
| 0003 ExEx tip mode | Groundwork only | Wire `ExExSource` to a real node; `nuthatch-node` binary; tip-latency measurement | **reth node** |
| 0004 Backfill | Implemented | — | — |
| 0005 Release eng | Implemented (v0.3.0) | rolling release chores | — |
| 0006 Grants | Accepted | Submit applications; track decisions | process (external) |
| 0007 Launch | Accepted | The actual launch run | process |
| 0008 Compliance | Implemented | — | — |
| 0009 Factory | Implemented | Child `end`/expiry conditions (children are forever); wildcard-address decode | — (small / future RFC) |
| 0010 Admin/webhooks | Implemented | SSE **push** live updates (status page polls today) | — (small) |
| 0011 Graph-network nest | **Parked after pilot** | Full migration: Indexer Directory (step 2) + promote the two ad-hoc pilot nests into a published `graph-network-nest` | product decision |
| 0012 Multi-nest roost | Implemented | One acceptance item: a **sustained** byte-identical-vs-solo table-parity run over a longer range | — (a live run; public RPC ok) |
| 0013 Storage/query | §3 shipped (DuckDB union) | DataFusion convergence (§2/§4, benchmark-gated, scaled-side first); Turso (§1, triple-gated) | scaled mode + a benchmark |
| 0014 Firehose | Deferred | State-diff + trace extraction | **0003 → reth node** |
| 0023 eth_call | Tier 1 building | Tier 2 (metadata cache), a *simple* RPC tier-3 fallback, tier 4 (hosted cache); more recipes (reserves) | — (tiers 1–2 + simple tier-3 buildable) |
| 0024 eth_call engine | Draft (deferred build) | The revm demand-driven state engine — **accepted design, deferred build** until the residue is measured large / archive-RPC-free operation is demanded / 0003 lands | RFC-0003 (best path) or a `--state-rpc` archive endpoint (Stage 1) |

## Track 1 — Infra (the shared blocker)

Almost all the *un-buildable-here* work traces to one missing piece:

- **A colocated reth node** (on the Hetzner box). Full node for tip; archive for deep backfill/traces.
  This is the substrate 0003 reads from and 0014 extracts from. Cost is provisioning + **days** of
  sync (full) or **TB + longer** (archive) — a hardware/ops job, not a coding session.
- Once it exists, the engineering unblocks in order: **0003** (ExEx wiring) → **0014** (traces/state).
- **Scaled-mode infra** (Postgres hot store) is the other greenfield substrate — it's where 0013 says
  to build DataFusion *first* (zero risk to the working embedded path). Also not started.

Nothing here is verifiable on the dev laptop, which is why it's been deferred all along — the project's
discipline has been "build only what we can verify live."

## Track 2 — Deferred engineering (gated)

- **0003 — reth ExEx tip mode.** Groundwork is in (source-agnostic `run`, `ExExSource` stub, lib+bin,
  toolchain/dep gates cleared). Remaining: wire the ExEx to a real reth node, ship the `nuthatch-node`
  binary, and publish an honest tip-latency number (notification → row queryable). *Gated on the node.*
- **0014 — firehose-class extraction (traces + state diffs).** Own-node/ExEx **only** by design (public
  RPC `debug_*` is a stated non-goal). *Gated on 0003 → the node.* But a **node-independent slice is
  buildable now** and would be forward-compatible (the RFC says everything downstream of extraction is
  free):
  - the **calldata decoder** — 4-byte-selector-keyed function decode reusing the alloy ABI machinery
    event decode already uses (the calldata analogue of topic0); unit-testable with fixture calldata;
  - the `[extract]` config (`traces`/`state` opt-in + contract/selector scoping);
  - the `state_diffs` / `traces` row + table schemas;
  - the volume guard — extend the RFC-0009 pre-backfill estimate to loudly flag a `traces = true` nest
    as unbounded-by-construction.
- **0013 — DataFusion convergence.** §3 (SQL-over-the-tip) already shipped via a DuckDB hot+cold union.
  The *destination* — one Arrow-native engine across both modes, redb/Postgres/Parquet as
  `TableProvider`s — is deferred and **benchmark-gated** (§4): build scaled-side first, then a
  `nuthatch bench` spike of DataFusion vs DuckDB over the same segments (latency + RSS within the ≤2 GB
  budget), then a golden SQL-compat suite, then decide whether to retire DuckDB. A dependency reality
  to design around: under MSRV 1.85 cargo resolves DataFusion 48 (arrow 55) — clashes with our arrow 56;
  aligning needs an MSRV bump to 1.88 (DataFusion 54) or an arrow downgrade.
- **Turso hot store (0013 §1).** Deferred, not rejected — behind the existing `HotStore` trait.
  Triple-gated: a production-ready release, an AGPL/no-BSL-clean licence, and a measured win over redb
  that federation doesn't already provide. Until all three, no.

## Track 3 — Process / ongoing (non-code)

- **0006 — grants.** Drafts + governance shipped; submitting to NLnet/EF-ESP and tracking decisions is
  ongoing external process.
- **0007 — launch & validation.** The launch kit is built; the launch itself is a go-when-ready run.
- **0011 — full graph-network migration.** *Parked* after the pilot proved the wedge (two Lodestar
  panels live on nuthatch, byte-identical to the subgraph). The full migration is a product decision;
  the RFC names the natural resumption: **step 2 (Indexer Directory** — highest query volume, clean
  top-N parity gate**)**, and/or promoting the two ad-hoc pilot nests into a real published
  `graph-network-nest` (which overlaps 0012 nest packaging).

## Track 4 — Small increments (buildable now, low priority)

- **0001 — proxy / EIP-1967 introspection.** Follow proxy→impl slots (`eth_getStorageAt`) so a proxied
  contract's implementation ABI is picked up. Open question; documented workaround exists.
- **0009 — child lifecycle.** Discovered children are currently forever; `end`/expiry conditions are
  deferred until demand. Also wildcard-address decode (the "future wildcard RFC").
- **0010 — SSE push.** The admin status page + table counts **poll** (~2 s); a Server-Sent-Events push
  channel was deferred in slice 5.
- **0012 — live parity acceptance.** The one open 0012 item: a *sustained* two-nest byte-identical-vs-
  solo table-parity run over a longer range (holds by construction — the roost runs the same per-window
  code as solo `dev` — but the belt-and-braces proof wants a real run; the public-RPC example roost
  suffices, no paid quota).

## Track 5 — 0.4.0 hardening audit: deferred items

The 0.4.0 hardening sweep fixed the critical/high tier (2 security, 2 data-corruption), added an e2e
test harness, batched the tip-loop writes, and cleared the correctness + defensive fixes that earned
their churn. These audit items were judged defer-worthy, with rationale:

- **Benchmarks (from the perf audit).** `nuthatch bench` measures backfill events/sec + peak RSS but
  **not** tip-lag ms or entity point-read p50/p99 or the `/sql` hot-scan cost — so those can regress
  silently. A future regression-guard, not a release blocker; add a point-read + tip-lag bench before
  the next perf push.
- **Perf, larger refactors.** Bound the `/sql` hot-scan (it materialises the whole tip per query — the
  #1 RAM risk on deep-finality L2s); single-scan the restart rebuild (currently 3× full scans); a
  persistent DuckDB connection instead of rebuilding the world per query; a compact binary row format
  instead of JSON-string storage. All real, all bigger than 0.4.
- **COR-5 — factory tip-cap recovery.** A factory nest's topic0-only tip fetch can't clear a provider
  `getLogs` cap on a very common template topic0 (busy chain) → the ingest task dies. It **fails safe**
  (a loud error, not silent corruption) and the fix needs surgery in the sensitive tip loop; do it with
  the address-filtered fallback the backfill path already has.
- **Low-severity, deferred with rationale:** COR-6 reserved-column collision (rare; needs a schema
  decision — namespace implicits or reject at build), COR-7 roost reorg fan-out blast radius (defensible
  under the single-failure-boundary rule), COR-8 i128-band balance drop (exotic amounts), COR-10 `_seq`
  20-bit `log_index` truncation (unreachable under current gas limits; add a debug-assert), SEC-7
  `WITH`-prefixed DML slipping the keyword gate (ephemeral in-memory only), SEC-8 sequential webhook
  delivery (one slow sink throttles others — `for_each_concurrent`), SEC-9 roost `/metrics` is the
  process global not per-nest (observability, bigger refactor).

## Suggested sequencing

1. **Decide the infra question** — is a colocated reth node worth provisioning now? It's the single
   unlock for 0003 + 0014 (the whole firehose-parity story). If yes, that's an ops track that runs in
   parallel with everything below.
2. **Free, high-signal now:** the 0014 node-independent slice (calldata decode + `[extract]` config +
   schemas + volume guard) — advances the last unstarted RFC without the node, fully testable.
3. **Cheap wins:** the 0012 live-parity run (public RPC), then the Track-4 small increments as appetite
   allows.
4. **When scaled mode is real:** start 0013's DataFusion convergence scaled-side, behind the benchmark
   gate. Not before.
