# Launch copy — Show HN

Draft. Refresh every number against the [README](../../README.md) at post time (Rule: every published
number traces to a reproducible artifact). Post one channel per day; this is the Phase 2 artifact from
[RFC-0007](../rfcs/0007-launch-and-validation.md).

---

## Title (pick one at post time)

> **Show HN: Nuthatch – a self-hosted blockchain indexer in one Rust binary (58 MB RAM)**

Alternate, if the single-contract figure reads better:

> **Show HN: Nuthatch – be your own blockchain indexer (one Rust binary, 37 MB RAM)**

Both numbers are measured and CI-enforced. Lead with the footprint — it's the figure nobody else in
this space publishes, and it's the whole thesis in four characters.

---

## First comment (the "why I built this")

I got tired of every app that reads a blockchain depending on a handful of hosted providers that
meter, gate, and can cut you off. Nuthatch is the opposite bet: one Rust binary, one command, a live
queryable API in under two minutes, with **no mandatory third-party data dependency, ever**. No
Postgres, no Docker, no IPFS, no token, no phone-home.

You point it at a contract address, it fetches the ABI, and it decodes every event into tables you can
query over SQL (or over an MCP server compiled into the binary, so a coding agent gets real schema
instead of hallucinating). Storage is a mutable hot store at the chain tip (redb) and
content-addressed Parquet past finality, with DuckDB attaching the segments read-only for analytics. A
reorg only ever touches the hot store; sealed segments are immutable.

The numbers I actually care about, all measured on the release build and reproducible in-repo:

- **~58 MB peak RAM** for a live 3-contract nest (USDC + WETH + DAI, 23 tables, indexing + sealing +
  DuckDB SQL + incremental balance views all at once); ~37 MB for a single contract. The RAM budget is
  ≤2 GB and CI fails the build above 256 MB — it's a budget, not a hope.
- Backfill throughput optimised in the open: **~289 → ~5,837 events/sec (~20× stacked)** — a
  seal-direct path that bypasses the hot store's per-row fsync (8.7×) plus an 8-way in-order pipeline
  (2.4× on top), with byte-identical output proven across paths so the fast path is provably the same
  data as the slow one. Public-RPC-bound at that figure; higher against your own node.

One genuinely different bit: entity views are **incremental** (Feldera/DBSP). A per-address balance
view treats a reorg as a *retraction*, not a recompute — the same circuit runs a backfill as a batch
and a reorg as a diff. Balances are i128 end-to-end (a transfer above i64::MAX won't silently vanish),
and they survive a restart.

**Honest limits, because you'll ask:** Ethereum + Arbitrum + Base only; events only (no call/trace
decoding yet); RPC polling — the reth ExEx in-process path is designed and stubbed, not shipped; no
GraphQL layer yet (SQL + point-reads + MCP today). It's v0.1.0 and solo-maintained. AGPL-3.0, a
grant-funded public good, not a startup — the sustainability plan and the "what we'll never build"
list are both in-repo.

Install, quickstart, the footprint methodology, and the full progress log:
https://github.com/cargopete/nuthatch

Happy to answer anything — architecture, the DuckDB single-writer design, the determinism proofs, or
why the binary is 67 MB (DuckDB + DBSP + wasmtime statically bundled; it's 5.8 MB without them, but a
single file is the non-negotiable).

---

## Anticipated questions (have answers ready, don't pre-post them)

- **"Why not just use The Graph / Ponder / Envio?"** → The Graph and Alchemy/Infura-class RPC are a
  metered third party you can be cut off from. Ponder still needs a capable (often paid) RPC. Envio's
  self-host now wants a token for HyperSync (phones home). Nuthatch's wedge: Rust single-binary ops +
  zero mandatory third-party API + IVM correctness + AI-native surface.
- **"67 MB binary?"** → answered inline above; offer the 5.8 MB no-embed figure.
- **"Is this a GraphOps product?"** → No. An operator is preparing a hosted offering under AGPL and
  shares revenue to fund core dev; the AGPL license means anyone can host the identical software. Link
  GOVERNANCE.md — don't argue it, link it.
- **"Events only is a dealbreaker for me because X"** → thank them, that's exactly the validation
  signal; log it (docs/validation).
