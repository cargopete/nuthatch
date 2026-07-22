# nuthatch troubleshooting

Symptom → what to look at (`/metrics`, Prometheus) → remedy. All `/metrics` series are on the running
`dev`/`roost` at `http://127.0.0.1:8288/metrics`.

## Backfill seems stuck / hung

- **Most common cause:** high `--concurrency` against a *single* RPC endpoint. Many concurrent requests
  to one host can stall the whole runtime. Remedy: `--concurrency 1` for one endpoint, or configure
  multiple `rpc_urls` (then 8–16 is fine).
- Watch `nuthatch_rows_decoded_total` / `nuthatch_last_block` climb. On a TTY, `dev` shows a live progress
  line with events/sec + ETA; if it's frozen, it's the concurrency stall above.
- A *sparse* contract over millions of blocks looks slow because each window is near-empty — widen it:
  `--window 50000`.

## "block N alone exceeds the provider's getLogs result cap"

A single block's logs are too large for the provider to return, and it can't be split further. Remedy:
use a provider with a higher/no result cap. This fails loudly rather than looping forever.

## Tip lag (falling behind the chain head)

- Check `nuthatch_tip_height` vs `nuthatch_last_block` — the gap is `nuthatch_tip_lag_blocks`. `nuthatch_sealed_through` trails
  further (past finality, by design).
- Causes: slow/rate-limited RPC (add endpoints or your own node), or a dense contract on a fast chain.
  The adaptive getLogs window self-tunes; a persistent gap means RPC throughput, not nuthatch.

## Reorgs

- Reorgs only ever touch the **hot store** — sealed segments are immutable. You should never see sealed
  data change. `nuthatch_reorgs_total` counts detections; the hot store rolls back and IVM views retract
  automatically, converging to canonical state.
- If you think you need to rewrite sealed Parquet to "fix" a reorg, the approach is wrong — the hot
  store already handles it.

## `/sql` returns 503 or times out

- **503 "server busy":** the analytical gate is saturated (default 2 concurrent). It's node
  self-protection — retry, don't raise the cap.
- **30 s timeout:** the query is too heavy. Add a `WHERE`/`LIMIT`, aggregate with `GROUP BY`, or run
  `explain` first to validate cheaply. Not a reason to remove the guard.
- **Binder/parse errors** come back with a fix hint (unknown table → nearest real table; `from`/`to` →
  double-quote; `sum(value)` → use `value_dec`). Follow the hint; it's derived from the real schema.

## RAM near the 2 GB budget

- The budget is per-runtime and CI-enforced. In a roost it's shared across nests (`max_rss_mb`, default
  2048); a mount projected to exceed it is refused. Check actual `nuthatch_rss_bytes` in the roster.
- DuckDB queries have their own 512 MB / 2-thread cap; the concurrency gate bounds the aggregate. If
  you're tight, lower concurrency rather than the per-query cap.

## "semantic.toml drift" warnings at startup

`semantic.toml` describes a table/column the decode registry doesn't have (a stale edit, or the ABI
changed). Fix the file, or regenerate the derived parts — the footguns are always recomputed; only the
authored descriptions are yours to maintain.

## ABI won't resolve at `init`

nuthatch tries Sourcify then Etherscan-class APIs. If both miss (unverified contract), supply the ABI
manually into `abis/` and reference it in `nuthatch.toml`, or point `--rpc` at a node for a proxy's
implementation lookup (EIP-1967 proxies resolve the implementation ABI automatically).
