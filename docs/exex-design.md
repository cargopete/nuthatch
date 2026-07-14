# ExEx tip-ingestion — design

**Status:** designed + stubbed (`feature = "exex"`), reth wiring deferred to a node environment.
**Why deferred:** reth is an enormous compile and an ExEx can only be exercised against a synced
node, so it can't be end-to-end verified in the same loop as the RPC path. The `Source` trait
(`src/source.rs`) is in place so this lands as a new impl, never a fork of the indexing logic.

## What ExEx buys us

A reth **Execution Extension** is compiled *into* the reth binary and runs in-process, consuming a
`CanonStateNotification` stream over shared memory — no RPC, no serialization boundary. It gives:

- **Native-block-time tip latency** (target <500ms, vs ~747ms for RPC-class hosted indexers). This
  is the "no third-party data API" sovereignty upgrade: reth + ExEx needs no external endpoint.
- **A first-class reorg signal** — `ChainCommitted` / `ChainReverted` variants, rather than the
  heuristic block-hash diffing the RPC source does today.
- **A prune back-channel** — the ExEx emits `ExExEvent::FinishedHeight` so reth knows how far we've
  processed and can prune.

## The push→pull bridge (the one real design point)

ExEx is **push** (reth calls us as blocks commit); the indexer's `Source` is **pull** (the single
cursor asks for `[from, to]`). `ExExSource` (in `src/source.rs`, `feature = "exex"`) bridges them
with a bounded in-memory buffer keyed by block number:

```
reth ExEx handler                         ExExSource (Source impl)        indexer loop
─────────────────                         ────────────────────────        ────────────
CanonStateNotification
  ChainCommitted(chain) ──► for each block: decode logs
                            source.commit(n, hash, logs) ──► buffer.insert(n, …)
  ChainReverted(chain)  ──► source.revert(from)         ──► buffer drop ≥ from
                                                                            source.tip()      ◄─ max key
                                                                            source.block_hash(n)
                                                                            source.logs(a,t,from,to)
  emit FinishedHeight(sealed_through) ◄──────────────────────────────────────────────────────┘
```

Nothing downstream changes: decode is already shared (both sources hand the loop the same `Log`
type), and the hot-store rollback + IVM retraction already handle reorgs. `ChainReverted` simply
becomes a *more precise, earlier* trigger for the same `store.rollback_to` + `balances.apply(retract)`
path the RPC source reaches via hash-diffing.

## Wiring checklist (when built in a node environment)

1. Add `reth` (+ `reth-exex`, `reth-node-ethereum`) as **optional** deps under `feature = "exex"`.
2. Implement the ExEx `async fn` that owns an `ExExSource`, loops over `ctx.notifications`, and calls
   `commit`/`revert`; decode via the existing `crate::decode`.
3. Emit `ExExEvent::FinishedHeight` from the indexer's `sealed_through` watermark (the sealing seam
   already computes exactly this).
4. Select the source in `indexer::dev` from config (`ingest = "rpc" | "exex"`); the `Arc<dyn Source>`
   line is the only construction site.
5. Bound the buffer: prune buffered blocks below `sealed_through` (they're durably in Parquet).

## Reference

Chief's **Rethix** (`gibz104/Rethix`, "Reth + Index") is the reference ExEx→Postgres implementation
to mirror: gap detection, resumable processing, thousands of blocks/sec.
