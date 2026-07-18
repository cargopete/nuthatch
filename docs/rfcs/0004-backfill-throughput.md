# RFC-0004: Backfill throughput — measurement, then optimization

- Status: Implemented (2026-07-18)
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: RFC-0001 (multi-event decode makes benchmarks representative)
- Blocks: RFC-0005 (published numbers ship with v0.1.0), RFC-0007 (launch claims)

## Abstract

Establish honest, reproducible backfill benchmarks (events/sec sustained, wall-clock to
completion) across three sourcing tiers — public RPC, own node, and a caching proxy —
then implement the optimizations with the highest measured leverage: adaptive range
chunking, pipelined decode, and direct-to-Parquet sealing for pre-finality history.
Numbers are published with methodology before any optimization work begins; targets are
set from the baseline, not from wishes.

## Motivation

Backfill is the one axis where Nuthatch's no-third-party stance costs performance:
Envio's ~25K events/sec rides HyperSync, a pre-indexed data service we refuse to depend
on. The credible position is not matching that number over someone's free-tier RPC —
it's knowing our numbers per tier, publishing them with the same honesty as the 37 MB
RAM figure, and being fast where the architecture permits (own node, and the
direct-to-Parquet path). "Fastest at the tip, honest about backfill, competitive when
colocated" only works if the numbers exist.

## Goals

1. A reproducible benchmark harness (`nuthatch bench backfill`) with pinned workloads.
2. Published baseline: events/sec and wall-clock per workload × sourcing tier, with
   hardware, provider, and date.
3. Implemented optimizations, each justified by a measured before/after on the harness.
4. A `--backfill-only` sealing mode that writes finalized history straight to Parquet,
   bypassing the hot store (the single largest expected win).

## Non-goals

- HyperSync-class performance over remote RPC (physically bounded by provider
  throughput; say so on the benchmark page).
- Traces/state backfill (events only, consistent with RFC-0001).
- Distributed/multi-machine backfill.

## Benchmark design

### Workloads (pinned, public, reproducible)

| ID | Nest | Range | Character |
|----|------|-------|-----------|
| W1 | USDC (mainnet) | 100,000 blocks ending 21,400,000 | dense single-contract (~1.3M events) |
| W2 | Horizon nest (Arbitrum) | full history from deployment | sparse multi-contract, L2 block cadence |
| W3 | USDC + WETH + Uniswap V3 factory (mainnet) | 50,000 blocks | mixed density, multi-table fan-out |

### Sourcing tiers

- T1: public RPC defaults (round-robin, as a new user experiences it)
- T2: own node (reth on the Hetzner box, localhost `eth_getLogs`)
- T3: erpc caching proxy in front of T1 (documents the cheap middle option)

### Metrics and rules

Sustained events/sec (total events ÷ wall-clock, excluding init), peak RSS, RPC calls
issued, retries. Three runs, report median. Harness pins block ranges and writes a
`bench-report.json`; the website benchmark page renders from these artifacts only —
no hand-typed numbers. Every published row carries: date, provider, hardware, commit.

## Optimization plan (ordered by expected leverage; each gated on measured wins)

### 1. Direct-to-Parquet backfill (`--seal-direct`)

For ranges already past finality at start time — which is nearly all of a backfill —
rows never touch redb: decode → Arrow record batches → sealed segments, streaming, with
the manifest updated per segment. The hot store engages only for the final
near-tip window. Eliminates per-row redb writes and the later seal-and-prune pass
(currently every historical row is written to redb, read back, written to Parquet, then
range-deleted). Expected: the dominant win on W1; also caps backfill RSS by
construction (bounded batch size, no hot-store growth).

Invariants preserved: segments remain content-addressed and identical to those the
seal-from-hot path would produce (asserted in tests — same input range must yield the
same segment hash regardless of path; this keeps the two-path determinism claim from
RFC-0003 intact).

### 2. Adaptive range chunking

Replace the fixed `getLogs` block-range chunk with a controller targeting a response
budget (events per response ≈ 2,000, adjusting multiplicatively on overshoot/
provider errors — providers cap by result count and differ wildly). Handles dense
ranges (USDC) and sparse ranges (Horizon) with the same code, removes the
per-provider hand-tuning risk flagged in RFC-0002.

### 3. Pipelined fetch → decode → write

Three-stage pipeline over bounded channels: K concurrent range fetches (per-provider
concurrency limits; default K=4 public, K=16 own-node) → rayon decode pool → single
writer (redb txn per chunk, or Arrow batch appender in seal-direct mode). Backpressure
via channel bounds keeps RSS flat. The single-writer constraint (redb, and Parquet
appender) is respected by design.

### 4. Only if measurements demand: request coalescing across contracts

W3 may show per-contract filter fan-out costs; a combined address+topics filter is
already the RFC-0001 design, so this is likely a no-op. Listed to force the
measurement before anyone "optimizes" it.

## Targets (set after baseline; provisional expectations)

Provisional, to be replaced by baseline-derived targets in this RFC's first revision:
T2 (own node) W1 ≥ 10,000 events/sec with seal-direct; T1 (public RPC) is
provider-bound — the target is *stability* (zero failed runs, graceful rate-limit
handling), not a number we don't control. If T2 with seal-direct lands within 2–3x of
Envio's published figure while reading raw `eth_getLogs`, that is the honest headline:
"no pre-indexed data service, N events/sec against your own node."

## Implementation plan

1. Harness + workloads + report format; run full baseline matrix; publish
   `docs/benchmarks.md` (baseline only, labeled as pre-optimization).
2. Seal-direct mode + same-hash path-equivalence tests; re-run W1/W3.
3. Adaptive chunker; re-run all tiers (biggest effect expected on T1 stability).
4. Pipeline; re-run; tune K defaults.
5. Final benchmark page on the website (rendered from bench-report artifacts),
   replacing any prose performance claims. Add a CI smoke-bench (W1 truncated to
   2,000 blocks, threshold generous) to catch order-of-magnitude regressions only —
   full benches run manually, they're too provider-dependent for CI gating.

## Testing and acceptance

- Path equivalence: seal-direct and hot-then-seal produce byte-identical segments for
  the same range (hash assert).
- Determinism under retry: injected fetch failures + retries yield identical final
  state (idempotent (block, log_index) keying).
- RSS during W1 with seal-direct stays ≤ 256 MB CI threshold.
- Published page shows all three tiers with methodology; no tier is omitted because
  its number is unflattering.

## Risks

- **Public-RPC variance makes T1 numbers noisy** → medians of three runs, dated, and
  framed as "what a new user should expect," not as the product's capability.
- **Seal-direct duplicates sealing logic** → mitigate by extracting one
  `SegmentWriter` used by both paths; the path-equivalence test is the guard.
- **Benchmarketing temptation** → the house rule is codified here: every number on the
  site traces to a bench-report artifact in the repo. No exceptions, including
  flattering ones.

## Alternatives considered

- **Consume HyperSync for backfill as an optional accelerator**: rejected as a default
  (mandatory-token dependency on a competitor, per licensing report); may be revisited
  as an explicitly-optional `Source` impl if users ask — the trait makes it a plugin,
  not a dependency.
- **Ship optimization before measurement**: rejected; the entire credibility posture is
  measured-then-claimed.

## Open questions

1. Should `bench backfill` results auto-publish (PR to the site repo) or stay manual?
   Manual for now; automation once format stabilizes.
2. Is W2's "full history" too slow for public-RPC baseline runs? If a T1/W2 run exceeds
   4 hours, cap the workload range and document it.
