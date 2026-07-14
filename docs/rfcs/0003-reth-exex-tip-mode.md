# RFC-0003: reth ExEx tip mode — wiring and latency measurement

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: — (parallel to RFC-0001/0002; ExExSource bridge already stubbed and tested)
- Blocks: RFC-0006 (the tip-latency number strengthens grant applications), website
  tip-latency claims

## Abstract

Wire the stubbed `ExExSource` to a real reth node on the Hetzner box, producing the
"no third-party" sovereignty mode: native-block-time tip following with logs read
in-process from `CanonStateNotification` — no RPC, no polling. Deliverable is twofold:
the working `nuthatch-node` binary, and a published, honestly-measured tip-latency
number (notification → row queryable over HTTP) with full methodology.

## Motivation

ExEx is the one place Nuthatch beats every competitor structurally: Amp claims 747 ms
median freshness; RPC pollers are bounded by poll interval + provider lag; an ExEx is
bounded by process-internal channel latency. It is also the completion of the
sovereignty story — with ExEx, zero external parties exist in the pipeline. The bridge
(reth's push → indexer's pull) is already implemented and tested; what remains is the
reth embedding, the revert path, and the measurement.

## Goals

1. `nuthatch-node` — a separate binary embedding reth with the Nuthatch ExEx installed,
   feeding the same indexing core the RPC path uses (`Arc<dyn Source>`; zero business-
   logic forks, per the Source-trait design).
2. Reorg handling via `ChainReverted` → hot-store rollback + IVM retraction (weight −1),
   proving the retraction path against real notifications, not just proptests.
3. Published p50/p99 tip latency with reproducible methodology.
4. `ExExEvent::FinishedHeight` emitted correctly so reth can prune.

## Non-goals

- Shipping reth inside the main `nuthatch` binary (compile time, binary size, and the
  ≤2 GB budget all say no; embedded mode remains RPC-first).
- Historical backfill through reth stages/ExEx backfill API (RFC-0004 owns backfill;
  the ExEx starts at current head and the RPC/own-node path fills history).
- Non-Ethereum ExEx (OP-stack reth variants) — later.

## Design

### 1. Binary layout

New workspace member `nuthatch-node` (feature-gated out of default builds; CI compiles
it in a dedicated job with aggressive caching — reth is a large dependency and must not
slow the main test loop):

```
nuthatch-node
  └─ reth NodeBuilder (EthereumNode defaults)
       .install_exex("nuthatch", |ctx| nuthatch_exex(ctx, indexer_handle))
       .launch()
  └─ nuthatch core: indexer task + HTTP serving, same process
```

The nest lives wherever `--nest <dir>` points; serving stays on 8288. One process,
one failure boundary (the liminal lesson, kept).

### 2. Notification handling

```
match notification {
  ChainCommitted { new } =>
    for block in new.blocks():
      rows = decode(receipts_logs(block))        // logs from receipts, in-process
      hot.commit(block.number, block.hash, rows)  // same path as RPC source
      ivm.apply(rows, weight = +1)
      bridge.advance(block.number)
  ChainReverted { old } | ChainReorged { old, .. } =>
    revert_point = old.first().number - 1
    hot.rollback_to(revert_point)                 // existing, proptested
    ivm.retract(rows_of(old))                     // weight = −1, existing circuit
}
ctx.events.send(ExExEvent::FinishedHeight(sealed_through))   // §3
```

Key correctness detail: **FinishedHeight advertises `sealed_through`, not the tip.**
Nuthatch's finality-gated sealing needs blocks to remain available until sealed; letting
reth prune only up to the sealed watermark makes the ExEx crash-safe (on restart, replay
from `sealed_through + 1` — reth still has those blocks). This is the ExEx-specific
restatement of the existing invariant.

Reverted blocks' rows are reconstructed for retraction from the hot store (rows keyed
by block range — no need to re-decode old chain data reth may have discarded).

### 3. Crash/restart semantics

On start, the ExEx receives `notifications` from reth beginning at the head reth
considers next for this ExEx (reth persists ExEx progress via FinishedHeight). Nuthatch
reconciles: if hot-store head > incoming start, rollback to incoming start − 1 (cheap);
if behind, the notification stream catches it up. No RPC needed in either case.

### 4. Latency measurement (the publishable number)

Metric: **T(row queryable) − T(notification received)**, and separately
**T(row queryable) − block timestamp** (the end-user-meaningful "behind head" figure,
which includes propagation/execution time Nuthatch doesn't control — published as a
second column, clearly labeled).

Instrumentation: monotonic timestamp at notification receipt; a probe task issues
`/table/{t}/row/{block}/{first_log_index}` immediately after commit signal and records
first-success latency. Sample ≥ 5,000 blocks (~17 hours). Publish p50/p90/p99, hardware
spec, reth version, and the probe code path. Target: **p99 < 50 ms** for
notification→queryable (expectation: single-digit ms p50 — it's a channel hop, a decode,
and a redb commit); "behind head" lands wherever mainnet propagation puts it, honestly.

Comparisons on the website follow the house rule: our measured numbers vs their
published claims, links to both methodologies, no adjectives.

### 5. Operational notes (Hetzner)

reth mainnet full node (not archive — ExEx needs the live stream, history comes from
sealed Parquet): ~1.2–2.5 TB NVMe, days to sync. Runs alongside the existing Graph
indexer stack; disk is the constraint to verify before starting sync. Systemd unit +
the existing Prometheus scrape (reth exposes metrics; add nuthatch tip-lag gauge).

## Implementation plan

1. `nuthatch-node` crate skeleton; compile reth pinned to a released version; CI job
   (build-only) so version bumps are caught.
2. Start reth sync on the Hetzner box (long pole — start first, in parallel with code).
3. Committed path against a synced node; soak 24 h; verify no drift vs an RPC-source
   instance running side-by-side on the same nest (row-count and content diff — a
   free correctness check between the two sources).
4. Reverted path: hard to reorg mainnet on demand — validate via (a) the existing
   bridge unit tests, (b) a reth-provided test harness notification sequence, and
   (c) waiting: small mainnet reorgs occur regularly; log and assert convergence when
   one lands during the soak.
5. Latency probe + 5,000-block sample + write-up (site: replace "target" phrasing on
   tip latency with measured numbers).
6. FinishedHeight/pruning verification: confirm reth prunes past sealed_through and a
   restart replays cleanly.

## Testing and acceptance

- Side-by-side ExEx vs RPC source on the same nest: identical sealed segments
  (content-addressed — hashes must match exactly; this is the determinism claim,
  proven across two independent ingestion paths).
- 72-hour soak, zero missed blocks, ≥1 natural reorg handled with converged state.
- Published latency table with methodology; p99 notification→queryable < 50 ms.
- Restart mid-stream: no gaps, no duplicate rows (idempotent commit by
  (block, log_index) keys).

## Risks

- **reth API churn**: ExEx APIs are still evolving; pin a release, gate upgrades on the
  CI build job. The Source trait means churn is contained to `nuthatch-node`.
- **Disk on the shared box**: full-node growth alongside the Graph stack; verify
  headroom (≥ 2.5 TB free) before sync or provision a dedicated NVMe.
- **The two-process temptation**: resist splitting serving from the node "for safety" —
  it reintroduces IPC and a second failure boundary. If node restarts hurt serving
  availability, the answer is the RPC-mode instance as a fallback, not a split.

## Alternatives considered

- **Standalone process consuming reth's gRPC/IPC (Firehose-style)**: keeps binaries
  small but adds a serialization boundary and an ops surface; rejected — in-process is
  the whole point of ExEx.
- **`finalized`-tag RPC polling as "good enough"**: it is good, but bounded at
  poll-interval latency and still a third party unless it's your node; ExEx is both
  faster and the sovereignty completion. Both modes coexist regardless.

## Open questions

1. Should `nuthatch-node` also expose the RPC source as an automatic fallback when the
   ExEx stream stalls (node still syncing)? Leaning yes, behind a flag, sharing the
   checkpoint store — but only after the soak proves the primary path.
2. OP-stack reth (Base) ExEx support — high leverage for RFC-0004's multi-chain story;
   sequence after mainnet soak.
