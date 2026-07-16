# Backfill benchmarks (RFC-0004)

**House rule:** every performance number nuthatch publishes traces to a `bench-report.json`
produced by `nuthatch bench backfill` — with date, provider, hardware, and commit. No hand-typed
numbers, including flattering ones. This page documents the harness and the pinned workloads; the
baseline matrix is filled in from real runs (an archive node is needed for the historical ranges).

## The harness

```sh
nuthatch bench backfill --dir <nest> --from <block> --to <block> [--runs 3] [--rpc <url>] [--out report.json]
```

Runs the real **fetch → decode → store** path over a pinned block range and reports the **median**
across runs of:

- **events/sec** — total decoded events ÷ wall-clock (excluding init). The headline.
- **wall-clock (s)**, **peak RSS (MB)**, **RPC requests** (including failover retries).

It writes to a throwaway store per run (never the nest's own DB), so runs are independent and
repeatable. `--rpc` overrides the nest's endpoints — point it at your own node for a T2 run.

Nothing here optimises anything. It exists so the seal-direct / adaptive-chunker / pipeline work
(later RFC-0004 slices) is each gated on a measured before/after, not a wish.

## Workloads (pinned, public, reproducible)

| ID | Nest | Range | Character |
|----|------|-------|-----------|
| W1 | USDC (mainnet) | 100,000 blocks ending 21,400,000 | dense single-contract (~1.3M events) |
| W2 | [horizon-nest](https://github.com/cargopete/horizon-nest) (Arbitrum) | full history from deployment | sparse multi-contract, L2 cadence |
| W3 | USDC + WETH + Uniswap V3 factory (mainnet) | 50,000 blocks | mixed density, multi-table fan-out |

## Sourcing tiers

- **T1** — public RPC defaults (round-robin), as a new user experiences it.
- **T2** — your own node (localhost `eth_getLogs`), via `--rpc`.
- **T3** — a caching proxy (e.g. erpc) in front of T1.

W1/W3 historical ranges and W2's full history require an **archive** node (public sequencer/
non-archive endpoints don't serve old `eth_getLogs`). Public-RPC (T1) numbers are noisy — take the
median of three runs, date them, and read them as "what a new user should expect," not as the
product's capability.

## Seal-direct (`--seal-direct`)

For ranges already past finality — nearly all of a backfill — rows can go straight to sealed Parquet,
bypassing the hot store: decode → buffered rows → content-addressed segments, no redb write, no
read-back, no prune. The bounded buffer caps RSS by construction. `seal_range` is the one shared
writer, so a given range yields **byte-identical** segments whether sealed directly or via the hot
store (asserted by `seal::seal_direct_matches_seal_via_hot_store`).

Measured before/after (same range, same RPC cost, only the storage path differs):

| Path | Range | Events | Wall-clock | events/sec |
|---|---|---|---|---|
| hot store (decode → redb) | USDC, 120 recent blocks (public RPC) | 12,127 | 42.0 s | **289** |
| seal-direct (decode → Parquet) | same | 12,127 | 4.8 s | **~2,520** |

**~8.7× faster.** The RPC portion is identical between the two (24 requests each); the difference is
that the hot path commits a redb transaction per row (~12k fsyncs), while seal-direct buffers and
writes a handful of segments. Single-run public-RPC smoke figures — noisy in absolute terms, but the
storage-path delta is the point and is not noise. Run it yourself:

```sh
nuthatch bench backfill --dir <nest> --from A --to B                 # hot store (baseline)
nuthatch bench backfill --dir <nest> --from A --to B --seal-direct   # seal-direct
```

## Baseline matrix (pre-optimization)

_Pending — the full W1–W3 × T1–T3 matrix is populated from archive-node runs (needed for the
historical ranges) and committed as `bench-report.json` artifacts._
