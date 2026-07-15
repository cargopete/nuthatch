# nuthatch

> **Be your own indexer.** One Rust binary, one command, live indexed API in under two minutes —
> AI-native, and with no mandatory third-party data API to trust or pay. Ever.

[![ci](https://github.com/cargopete/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/cargopete/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

Self-hosted-first, AI-native blockchain indexer. Embedded mode runs as a single process with no
external services — no Postgres, no Docker, no IPFS. See [`CLAUDE.md`](CLAUDE.md) for the standing
design brief.

## Status: embedded mode built end-to-end; scaled mode + reth ExEx outstanding

The embedded single-binary path works from `init → dev → live API`, with reorg-safe storage,
finality-sealed Parquet, DuckDB SQL, an incrementally-maintained balance view, sandboxed WASM
transforms, and an MCP server — all in one process, no external services. What remains is the
scaled (Postgres / DataFusion) mode and wiring reth ExEx to a node.

| Working now | Outstanding |
|---|---|
| `init` → ABI resolve (Sourcify → Etherscan) → scaffold (+ `llms.txt`, skills) | reth ExEx wiring — `Source` trait ready; needs a synced node |
| RPC log polling with round-robin failover, behind a `Source` trait | scaled Postgres mode (`HotStore` trait) + DataFusion federation |
| Deterministic ERC-20 `Transfer` decode | effectful transform worlds + signed pipeline manifests |
| Reorg self-healing (block-hash checkpoints → hot-store rollback) | governed semantic layer + natural-language queries |
| Finality-gated content-addressed Parquet sealing + hot-store pruning | IVM restart-replay (persist/rebuild balances across restarts) |
| Read-only analytical SQL (DuckDB) over sealed segments | i128 balances (the view accumulates in i64 base units today) |
| IVM balance view (DBSP) — reorg = retraction | GraphQL compatibility layer |
| WASM transform runtime (pure, sandboxed, batched Arrow) | |
| MCP server (stdio, 6 tools, offline) + `llms.txt` + `.claude/skills` scaffold | |
| redb hot store, entity point-reads with cold (DuckDB) fallback | |

Scope today: **one chain (Ethereum), ERC-20 `Transfer` events, RPC polling (reth ExEx designed +
stubbed), embedded storage (redb hot + DuckDB/Parquet cold).** Multi-chain, non-Transfer decode,
and the scaled mode are not built yet.

### Measured footprint (the number nobody else publishes)

| | |
|---|---|
| **Peak RAM** | **~37 MB** (hot indexing + sealing + DuckDB SQL, live mainnet) |
| Binary size | 67 MB (release; DuckDB + DBSP + wasmtime statically bundled — 5.8 MB without them) |
| Budget | ≤2 GB RAM — **using 1.8%** of it |

Honest and reproducible: `nuthatch init 0xA0b8…eB48 && nuthatch dev --backfill 200`, sampled with
`ps -o rss`. Measured on the release build with the full embedded pipeline active. The RAM budget is
enforced in CI (a `footprint` job fails the build above 256 MB — generous headroom over the measured
~37 MB); the binary grew because DuckDB, DBSP, and wasmtime are statically bundled (still a single
file — the embedded-mode non-negotiable). Hot layer stays bounded by pruning sealed rows to Parquet
past finality.

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

- **2026-07-15 — RFC-0001 step 5: generalised serving from the registry.** The API and AI surface
  now describe the *whole* data model, not just transfers. `GET /tables` lists every decoded table
  with its columns, Solidity types and topic0; `GET /table/{name}?limit=N` returns recent rows merged
  across the hot tip and the sealed segments (deduped by `(block, log_index)`, hot wins), with optional
  `from_block`/`to_block`. Two matching MCP tools (`tables`, `table`) bridge the same endpoints — the
  tool count is now 8. `init` builds the registry up front and writes `schema.json` (`{registry_hash,
  tables}`); `llms.txt` and the Claude skill enumerate the real tables instead of hand-waving at them.
  Verified live on USDC (17 tables): `/tables` and both MCP tools return the full schema, `/table`
  serves merged hot+cold rows and 404s on an unknown table. 27 tests green. _Remaining: step 6
  (footprint re-measure on a multi-contract nest + README table refresh)._
- **2026-07-15 — RFC-0001 step 4: per-table cold storage.** Sealing generalises from transfer-only
  to every table: rows are grouped by their `table` field and each becomes its own content-addressed
  Parquet segment; `manifest.json` is now `{tables: {name: [segments]}}`. DuckDB exposes one view per
  table (`{alias}__{event}`); `/sql` queries any table and `/entity` point-reads search all tables
  across the hot→cold seam. **Hot-store pruning is restored** — the whole finalized range is pruned
  once every table's segment is durable (single global watermark). Row storage is unified (all rows
  are typed JSON with a `table` field; big ints render as decimal when they fit u128). Verified live
  on USDC: 2,893 rows sealed across 5 tables (transfer/approval/mint/burn/authorization_used) and
  pruned; `/sql` per-table (2,737 transfers, 292 approvals); a pruned row served via the DuckDB
  fallback. 27 tests green. _Remaining: step 5 (generalised `/tables` + `/table/{name}` serving,
  MCP + `llms.txt` regenerated from a schema manifest); step 6 (footprint re-measure)._
- **2026-07-15 — RFC-0001 step 3: multi-contract decode wired end-to-end.** `dev` now drives the
  `DecodeRegistry`: one combined `eth_getLogs` (all addresses × all topic0s) → decode *every* declared
  event of *every* contract → per-table rows in the hot store. The hardcoded Transfer path is retired
  (`decode.rs` deleted). Transfer-shaped rows keep the balance view, sealing, and the `transfers` SQL
  view working unchanged; non-transfer rows are stored generically (visible via `/entities`; per-table
  sealing + SQL land in step 4). Reorg rollback is table-agnostic (multi-table convergence test).
  **Proxy resolution at init** — EIP-1967 + legacy-OZ implementation slots — so USDC resolves to its
  FiatToken implementation (17 tables) instead of the bare proxy. Verified live: 2,844 rows across
  `usdc__transfer`/`approval`/`burn`/`authorization_used`, 1,444 holders. 28 tests green. _Step-3 limit:
  the hot store isn't pruned yet (step 4 does per-table seal + prune); only the transfer table is in `/sql`._
- **2026-07-15 — RFC-0001 step 2: multi-contract `init` + `nuthatch.toml` v2.** `init` now takes N
  addresses (+ optional `--alias`), resolves each ABI to `abis/{alias}.json`, and auto-detects each
  deployment block via an `eth_getCode` binary search (~25 calls — verified live: USDC→6,082,465,
  WETH→4,719,568). Config is now a `[nest]` header + `[[contracts]]` array; v1 single-contract files
  migrate transparently on load. `dev` runs the existing single-contract Transfer path on the nest's
  primary contract (and warns about the rest) until step 3 generalises decode + storage to every
  contract via the `DecodeRegistry`. 30 tests green (config migrate/roundtrip, alias validation,
  deploy binary-search, address normalisation).
- **2026-07-14 — RFC-0001 step 1: ABI-driven decode engine.** New `src/registry.rs` — a
  `DecodeRegistry` built from N contract ABIs (via alloy-json-abi / alloy-dyn-abi) maps topic0 →
  per-`{alias}__{event}` tables, filters by emitting address, and decodes any log into typed rows
  using the RFC-0001 type mapping (address / uint & int by width / bytesN / string / arrays→JSON /
  indexed-dynamic→hash). Records a stable, order-independent content hash for verifiability, and
  skips+counts anonymous events. 7 golden/property tests (real USDC Transfer, multi-contract table
  routing, type mapping, registry-hash stability, anonymous skip). Foundation only — not yet wired
  into the pipeline (steps 2-6: multi-contract init, generic storage, per-table sealing, serving);
  `dead_code` allowed on the module until integration removes it.
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
