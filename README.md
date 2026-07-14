# nuthatch

> **Be your own indexer.** One Rust binary, one command, live indexed API in under two minutes —
> AI-native, and with no mandatory third-party data API to trust or pay. Ever.

Self-hosted-first, AI-native blockchain indexer. Embedded mode runs as a single process with no
external services — no Postgres, no Docker, no IPFS. See [`CLAUDE.md`](CLAUDE.md) for the standing
design brief.

## Status: walking skeleton (slice 1)

This is the thinnest end-to-end path that actually runs — deliberately minimal. It proves the
`init → dev → live API` spine; the interesting layers grow onto it next.

| Working now | Coming (per build order) |
|---|---|
| `init` → ABI resolve (Sourcify → Etherscan) → scaffold | Parquet sealing + DuckDB analytics (slice 2) |
| RPC log polling with round-robin failover | DBSP declarative views / IVM (slice 3) |
| Deterministic ERC-20 `Transfer` decode | WASM transform runtime, Arrow WIT (slice 4) |
| Reorg self-healing (hot-store rollback) | MCP server + `llms.txt` + skills (slice 5) |
| Finality-gated Parquet sealing (content-addressed) | DBSP declarative views / IVM (slice 3) |
| Read-only analytical SQL (DuckDB) over sealed segments | MCP server + `llms.txt` + skills (slice 5) |
| **IVM balance view (DBSP) — reorg = retraction** | scaled Postgres/DataFusion mode (HotStore trait) |
| **WASM transform runtime (pure, sandboxed, batched Arrow)** | effectful transform worlds + pipeline manifests (later) |
| **MCP server (stdio, 6 tools, offline) + `llms.txt` + skills scaffold** | governed semantic layer + NL queries (later) |
| **Ingestion behind a `Source` trait — RPC live; reth ExEx designed + stubbed** | reth ExEx wired to a node (needs a synced node) |
| redb hot store, point-reads with cold fallback | |
| HTTP API (`/`, `/entities`, `/entity/{id}`) | reth ExEx tip mode, scaled Postgres mode (slice 6) |

Scope of the skeleton: **one chain (Ethereum), Transfer events only, RPC polling, redb-only.**
No IVM, no DuckDB/Parquet, no MCP yet.

### Measured footprint (the number nobody else publishes)

| | |
|---|---|
| **Peak RAM** | **~37 MB** (hot indexing + sealing + DuckDB SQL, live mainnet) |
| Binary size | 49 MB (release; DuckDB + DBSP statically bundled — 5.8 MB without them) |
| Budget | ≤2 GB RAM — **using 1.8%** of it |

Honest and reproducible: `nuthatch init 0xA0b8…eB48 && nuthatch dev --backfill 200`, sampled with
`ps -o rss`. Measured on the release build with the full slice-2 pipeline active. The RAM budget is
the CI-enforced one and holds comfortably; the binary grew because DuckDB bundles a C++ engine
statically (still a single file — the embedded-mode non-negotiable). Hot layer stays bounded by
pruning sealed rows to Parquet past finality.

## Quickstart

```sh
cargo build --release

# Index USDC on mainnet (uses public RPC defaults; no key needed)
./target/release/nuthatch init 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain mainnet
./target/release/nuthatch dev

# in another shell
curl localhost:8288/
curl localhost:8288/entities?limit=5
```

`init` writes `nuthatch.toml` (config) and `abi.json` (resolved ABI). `dev` polls logs, decodes
Transfers into an embedded `nuthatch.redb`, and serves the API on `127.0.0.1:8288`.

### AI-native, offline

`init` also scaffolds an `llms.txt` and a `.claude/skills/nuthatch/` skill so coding agents learn
the real query surface. Expose a running index to an agent over the Model Context Protocol:

```sh
nuthatch mcp                 # stdio MCP server: status, schema, sql, entity, balance, top_balances
```

It bridges to the local `nuthatch dev` — no external calls, no telemetry, no gated data API.

## Design principles (non-negotiable)

- **Single static binary**, zero external services in embedded mode.
- **≤2 GB RAM** for single-chain tip-following + serving (a CI-enforced budget, not a hope).
- **No phone-home** — no telemetry, no mandatory tokens. AI features are local-first (Ollama / BYO-key).
- **Determinism in the core** — decode, reorg, and entity derivation are deterministic and
  re-executable. LLMs write code and tests; they never sit in the runtime data path.

## Progress log

Newest first. One entry per push, tracking the [build order](CLAUDE.md#build-order-vertical-slices-each-ends-runnable).

- **2026-07-14 — Slice 6 (first half): ingestion behind a `Source` trait.** Decode, hot store,
  sealing, IVM, and serving are now oblivious to where blocks come from — the indexer sees only
  `Arc<dyn Source>` (`tip` / `block_hash` / `logs`). `RpcSource` is the working impl (RPC polling, no
  node). `ExExSource` (feature = "exex") is the "no third-party" sovereignty upgrade — native-block-
  time tip latency from a colocated reth node — **designed and stubbed** with the push→pull bridge
  (reth's `CanonStateNotification` push → the loop's pull) implemented and tested; the reth wiring
  itself is deferred to a node environment (reth is an enormous compile that needs a synced node).
  See [`docs/exex-design.md`](docs/exex-design.md). No `#[cfg]` forks of business logic — adding ExEx
  is one new impl. Verified: 18 default tests + the exex stub's bridge test green; live indexing still
  works through the trait. _Deferred: reth wiring; scaled Postgres mode (a `HotStore` trait, same pattern)._
- **2026-07-14 — Slice 5: MCP server + AI surface.** `nuthatch mcp` speaks the Model Context
  Protocol over stdio (newline-delimited JSON-RPC), so a coding agent can query a running index
  directly. Six tools — `status`, `schema`, `sql`, `entity`, `balance`, `top_balances` — not a thin
  one-endpoint wrapper; `schema` returns a semantic hint (the seed of the governed semantic layer).
  It's a thin **offline** bridge to the local `nuthatch dev` HTTP API, so it never contends with the
  single-writer store and nothing phones home. `nuthatch init` now scaffolds `llms.txt` and a
  `.claude/skills/nuthatch/` skill into the project so agents learn the real query surface instead of
  hallucinating it. Verified: 18 tests green; a live MCP session (initialize → tools/list → tools/call)
  bridged `status`/`sql`/`top_balances` to a running index. _Deferred: the governed semantic layer
  + NL queries, streaming subscribe, Ollama/BYO-key AI authoring._
- **2026-07-14 — Slice 4 (first cut): WASM transform runtime.** Ported from
  [liminal](https://github.com/lodestar-team/liminal) with the brief's key change — **the WIT call
  boundary is a whole batch (Arrow IPC), not one event** (liminal was per-event; that can't keep up
  with backfill). A transform is a `wasm32-wasip2` component exporting `nuthatch:transform/stage`;
  the host (wasmtime 44) loads it with **zero capabilities** — base WASI only, no http/kv/filesystem
  — so it's deterministic by construction and its purity is checkable from the component's imports
  alone (`wasm-tools component wit`), no code inspection. Ships a pure example component
  (`large-transfers`: keeps transfers ≥ 1,000 USDC) and a `nuthatch transform <component.wasm>` CLI.
  Verified: 16 tests green incl. an end-to-end host-loads-real-wasm test; live run fed 2,470 USDC
  transfers → 525 filtered facts, deterministic. _Deferred: effectful worlds (http/kv-granted,
  annotations-only), wiring transforms as a live indexing stage, and signed pipeline manifests._
- **2026-07-14 — Slice 3: DBSP declarative views (the IVM core).** The first derived entity —
  per-address token balances — is now a **declarative incremental view**, not a hand-rolled handler.
  Balance is stated as Σ(in) − Σ(out) and maintained by a DBSP circuit: a new transfer is a +1 delta,
  and a **reorg is the same transfer re-fed with weight −1** (a retraction) — the identical circuit
  serves backfill and tip. Served at `/balances` and `/balance/{address}`. Verified: a deterministic
  golden test proves incremental maintenance + retraction convergence; live run derived 2,257 holder
  balances (top holder correctly the zero/burn address), **peak RAM 36.9 MB**. 14 tests green.
  _Known limits (this slice): balances accumulate in i64 base units (fine for USDC-class tokens); the
  view is in-memory and rebuilt per process — a warm restart resumes indexing but does not yet replay
  prior balances (persistence/replay is a later slice)._
- **2026-07-14 — Slice 2 complete: DuckDB SQL + hot-store pruning.** A read-only `/sql` endpoint
  runs analytical queries over the sealed segments via an embedded, memory-capped DuckDB (segments
  attached read-only; ingestion never writes DuckDB). Once a range is sealed and catalogued, its
  rows are pruned from the redb hot store — and `/entity/{id}` transparently falls back to DuckDB for
  pruned rows, so point-reads work seamlessly across the hot→cold seam. Verified live: sealed +
  pruned a 2,497-row segment, `/sql` aggregations correct, a pruned id resolved via the cold path;
  **peak RAM 37 MB** with the full pipeline. Binary is now 44 MB (DuckDB bundled). 13 tests green.
- **2026-07-14 — Slice 2 (in progress): Parquet sealing.** Once a block range passes finality
  (a conservative 64-block depth for now), its entities are sealed to an immutable, content-addressed
  (sha256) Snappy Parquet segment under `segments/`, catalogued in `manifest.json` with block bounds
  and row count; a monotonic `sealed_through` watermark advances so each block seals exactly once. The
  hot store is deliberately *not* pruned yet — point-reads keep hitting redb until the DuckDB serving
  path lands. Verified live: sealed a 2,355-row segment for finalized mainnet USDC; round-trips through
  Arrow in tests (10 tests green). The append-only cold layer never sees a reorg, by construction.
- **2026-07-14 — Slice 2 (in progress): reorg safety.** Block-hash checkpoints + `rollback_to`
  in the hot store; the indexer detects when its last committed block falls off the canonical
  chain and rolls back to the deepest surviving checkpoint. Reorgs land *only* in the mutable hot
  store — the invariant that lets later slices seal to immutable Parquet strictly past finality. A
  proptest asserts convergence: any random fork depth + alternate branch reaches the same state as
  indexing the winning branch directly (7 tests green). Verified live: no false reorgs on mainnet.
- **2026-07-14 — Slice 1 gate closed.** 5 deterministic golden decode tests (fixed USDC-transfer
  fixture → exact output) pass; measured peak RAM **~33 MB** indexing 7,013 transfers — 1.6% of the
  2 GB budget. Both non-negotiables (tests + footprint) met, so slice 2 is unblocked.
- **2026-07-14 — Slice 1: walking skeleton.** `init` (ABI via Sourcify v2, Etherscan fallback) →
  `dev` (RPC log polling with round-robin failover) → deterministic ERC-20 `Transfer` decode →
  redb hot store → axum HTTP API. Verified alive against live mainnet USDC, keyless: 170+ transfers
  indexed in ~1.5s with correct decimal values. Scope: one chain, Transfer-only, RPC-poll, redb-only.

_Next: consolidation — a `HotStore` trait for scaled Postgres mode, CI (test + RAM-budget gate), and closing known gaps (IVM restart-replay, i128 balances). reth ExEx wiring lands in a node environment._

## Licence

[AGPL-3.0-only](LICENSE).
