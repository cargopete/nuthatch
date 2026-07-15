# nuthatch

> **Be your own indexer.** One Rust binary, one command, live indexed API in under two minutes —
> AI-native, and with no mandatory third-party data API to trust or pay. Ever.

[![ci](https://github.com/cargopete/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/cargopete/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

Self-hosted-first, AI-native blockchain indexer. Embedded mode runs as a single process with no
external services — no Postgres, no Docker, no IPFS. See [`CLAUDE.md`](CLAUDE.md) for the standing
design brief.

## Status: embedded mode built end-to-end; scaled mode + reth ExEx outstanding

The embedded single-binary path works from `init → dev → live API`, with multi-contract ABI-driven
decode, reorg-safe storage, finality-sealed Parquet, DuckDB SQL, an incrementally-maintained balance
view, sandboxed WASM transforms, and an MCP server — all in one process, no external services. What
remains is the scaled (Postgres / DataFusion) mode and wiring reth ExEx to a node.

| Working now | Outstanding |
|---|---|
| `init` → multi-contract ABI resolve (Sourcify → Etherscan, EIP-1967/legacy-OZ proxy) → scaffold (+ `schema.json`, `llms.txt`, skills) | reth ExEx wiring — `Source` trait ready; needs a synced node |
| RPC log polling with round-robin failover, behind a `Source` trait | scaled Postgres mode (`HotStore` trait) + DataFusion federation |
| Deterministic decode of **every declared event of every contract** (topic0-keyed registry → one table per `{alias}__{event}`) | effectful transform worlds + signed pipeline manifests |
| Reorg self-healing (block-hash checkpoints → hot-store rollback) | governed semantic layer + natural-language queries |
| Per-table finality-gated content-addressed Parquet sealing + hot-store pruning | IVM generalisation (derived views are DuckDB SQL over sealed data today) |
| Read-only analytical SQL (DuckDB) — one view per table over sealed segments | GraphQL compatibility layer |
| `GET /tables` + `GET /table/{name}` (hot+cold merged) — the full data model | |
| IVM balance view (DBSP) — **i128** base units, reorg = retraction | |
| IVM restart-replay — the view rebuilds from stored facts on restart | |
| WASM transform runtime (pure, sandboxed, batched Arrow) | |
| MCP server (stdio, 8 tools, offline) + `schema.json` + `llms.txt` + `.claude/skills` scaffold | |
| redb hot store, entity point-reads with cold (DuckDB) fallback | |

Scope today: **one chain (Ethereum), all contract events decoded across a multi-contract nest, RPC
polling (reth ExEx designed + stubbed), embedded storage (redb hot + DuckDB/Parquet cold).**
Multi-chain and the scaled mode are not built yet.

### Measured footprint (the number nobody else publishes)

| | |
|---|---|
| **Peak RAM** | **~58 MB** (3-contract nest, 23 tables — hot indexing + per-table sealing + DuckDB SQL + IVM, live mainnet) |
| Single contract | ~37 MB (USDC alone) |
| Binary size | 67 MB (release; DuckDB + DBSP + wasmtime statically bundled — 5.8 MB without them) |
| Budget | ≤2 GB RAM — **using 2.8%** of it |

Honest and reproducible: `nuthatch init 0xA0b8…eB48 0xC02a…6Cc2 0x6B17…71d0F && nuthatch dev
--backfill 400`, sampled with `ps -o rss`. Measured on the release build with the full embedded
pipeline active — the run above sealed 16,986 rows across 11 tables of the three contracts (USDC,
WETH, DAI; 23 tables total) and pruned the hot store, while the IVM view tracked 5,005 holders. The
RAM budget is enforced in CI (a `footprint` job fails the build above 256 MB — generous headroom over
the measured ~58 MB); the binary is large because DuckDB, DBSP, and wasmtime are statically bundled
(still a single file — the embedded-mode non-negotiable). Hot layer stays bounded by pruning sealed
rows to Parquet past finality.

## Quickstart

```sh
cargo build --release

# Index USDC on mainnet (uses public RPC defaults; no key needed)
./target/release/nuthatch init 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --chain mainnet
./target/release/nuthatch dev

# in another shell
curl localhost:8288/
curl localhost:8288/tables
curl 'localhost:8288/table/c0__transfer?limit=5'
```

`init` writes `nuthatch.toml` (config), the resolved ABIs under `abis/`, and `schema.json` (the
decoded tables + columns). `dev` polls logs, decodes every declared event of every contract into an
embedded `nuthatch.redb`, and serves the API on `127.0.0.1:8288`. Pass several addresses to `init`
(optionally with `--alias`) to index a multi-contract nest in one process.

### AI-native, offline

`init` also scaffolds an `llms.txt` and a `.claude/skills/nuthatch/` skill so coding agents learn
the real query surface. Expose a running index to an agent over the Model Context Protocol:

```sh
nuthatch mcp                 # stdio MCP server: status, schema, tables, table, sql, entity, balance, top_balances
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

- **2026-07-15 — RFC-0002: robustness fixes from Horizon dogfooding.** Authoring the Horizon nest
  (three real Arbitrum contracts, derived views) surfaced two engine bugs, both now fixed and
  regression-tested. **(1)** The read-only `/sql` guard checked `starts_with("select"/"with")` on raw
  text, so a query opening with a `-- comment` (or `/* … */`) was wrongly rejected — it now skips
  leading SQL comments before checking. **(2)** A nest view that UNIONs several event tables
  cascade-failed when one of them had no sealed data yet (common on a sparse/low-traffic contract):
  the whole view silently vanished. Now every *declared* table resolves — a sealed one as a real
  view, an unsealed one as an **empty typed view** (columns and their `*_dec`/`*_overflow` siblings
  reconstructed from `schema.json`, `WHERE false`) — so derived views compute over sparse data instead
  of disappearing. Verified live: the full Horizon model (`allocations`, `indexers`, `global`,
  time-bucketed rollups) computes on real Arbitrum data — 390 active allocations, 5 indexers, 70,030
  GRT indexing rewards — with the empty `operators`/`delegations` views present, not fatal. 44 tests.
- **2026-07-15 — RFC-0002 step 5: `nuthatch check` (invariant/parity framework).** A nest ships
  `checks/*.sql` — each a read-only query over its sealed data (per-event tables + derived views) —
  and `nuthatch check [name]` runs them, comparing each result to a recorded expected fixture
  (`checks/expected/<name>.json`), printing a row-level diff on mismatch and exiting non-zero. For the
  Horizon nest those fixtures are the deployed subgraph's answers at a pinned block, so this *is* the
  parity check; the framework is generic (any nest can ship invariants). Hermetic by design — it
  compares committed fixtures, not a live endpoint, so it runs in CI with no network. `--update`
  re-records fixtures from current results (authoring). Verified live: recorded 5-row fixtures on USDC,
  a matching run passed (exit 0), a tampered fixture failed with a clear diff (exit 1). 43 tests.
- **2026-07-15 — RFC-0002 step 4a: nest-defined derived-entity views.** A nest can ship
  `views/*.sql` — DuckDB views over its per-event tables (e.g. fold Created/Resized/Closed into a
  current-allocation view) — and the analytical `/sql` surface now loads them, in sorted filename
  order (so `20-*.sql` can build on `10-*.sql`), after the per-event table views. Best-effort: a view
  over a not-yet-sealed table, or a bad statement, is skipped with a debug log rather than failing
  the surface. Point-reads deliberately skip them (they touch only raw tables). This is the serving
  side of the Horizon nest's derived entities; DuckDB views read *sealed* data, so derived entities
  lag the tip by the finality window (raw tables stay tip-fresh) — the honest freshness tradeoff the
  RFC documents, and the concrete motivation for IVM generalisation later. 42 tests.
- **2026-07-15 — RFC-0002 step 3: `init --from` + config schema versioning.** A nest is just a repo
  (committed `nuthatch.toml` + vendored ABIs), so publishing one is `git push` and consuming one is
  `nuthatch init --from <git-url | ./dir>` — no registry service, deliberately. `--from` clones (shallow)
  or copies the nest, strips the clone's `.git`, and **validates** it: the toml parses at a supported
  schema version and the decode registry builds from the vendored ABIs (nothing is re-resolved — the
  nest is self-contained). New `schema_version` in `[nest]` (default 1); a nest declaring a newer
  version is rejected with a clear upgrade message — the guard that makes consuming third-party nests
  safe. Verified live: `init --from` over both a local dir and a git repo produced a runnable nest
  (`dev` indexed it with no ABI resolution); the version guard and the `addresses`/`--from` conflict
  both fire. 41 tests (+2).
- **2026-07-15 — RFC-0002 step 2: `block_timestamp` implicit column.** Every row now carries
  `block_timestamp` (u64 unix seconds) from the block header — the RFC-0001 amendment the time-bucketed
  aggregation views need. It's batch-fetched: after decoding a window the indexer collects the distinct
  blocks that produced rows and asks for their timestamps in a *single* JSON-RPC batch (one round-trip
  even for a dense window), via new `RpcClient::block_timestamps` / `Source::block_timestamps`.
  Best-effort — a block the endpoint can't answer stores 0. Verified live on USDC: hot rows carry a
  current timestamp, and `date_trunc('minute', to_timestamp(block_timestamp))` yields clean per-minute
  rollups over sealed data. 39 tests.
- **2026-07-15 — RFC-0002 step 1: chain registry + Arbitrum One + L2 finality.** The chain registry
  generalises beyond mainnet — each chain now carries a **finality policy** and an `eth_getLogs`
  window, so an L2 is a data entry, not a fork of the indexing loop. New `arbitrum-one` (chain 42161,
  keyless RPCs) uses a `FinalizedTag` policy: it prefers the node's L1-aware `finalized` block tag
  (correct by construction on an L2), falling back to a fixed depth (~7.5 min) when an endpoint
  doesn't serve the tag. `Source` gained `finalized()`; the seal ceiling is now a pure, unit-tested
  `seal_ceiling(finality, tip, tag)`. Mainnet keeps `Depth(64)`/window 20; Arbitrum uses window 2000
  (sparse events, fast blocks). Verified live: `init 0x00669A…eF03 --chain arbitrum-one` resolved the
  Horizon staking proxy via Sourcify (28 tables); `dev` sealed exactly up to Arbitrum's live
  `finalized` block (484091237, *not* the depth fallback), 2000-block windows. 39 tests (+5).
- **2026-07-15 — RFC-0001 finished to the letter.** Closed the last deviations between the shipped
  indexer and RFC-0001's design. **u256 SQL ergonomics (§2):** every big-integer column now gets two
  derived DuckDB view columns — `{col}_dec` (the value as `DECIMAL(38,0)` when it fits, else NULL) and
  `{col}_overflow` (true when the exact value exceeds 38 digits) — so analytics can `SUM(value_dec)`
  without hand-casting text. **Implicit provenance columns (§2):** every table now carries `block_hash`
  and `_seq` (a deterministic monotonic ordering key = `block << 20 | log_index`, not a mutable
  counter — re-executable by construction) alongside the existing `block_number/tx_hash/log_index/
  address`. **Indexed dynamic types** get a `_hash`-suffixed column name (the topic holds
  `keccak(value)`, not the value). Added the acceptance tests the RFC named: golden decodes for an
  address-heavy event (Uniswap V3 `PoolCreated`) and an indexed-string event, plus a cross-table
  `/sql` JOIN. Verified live on USDC: `SUM(value_dec)` over 8,736 transfers, `block_hash`/`_seq`
  present on every row. 34 tests green (+4). **RFC-0001 is now complete in spirit and letter.**
- **2026-07-15 — Correctness gaps closed: i128 balances + IVM restart-replay.** Two teeth-baring
  fixes to the balance view. **(1) i128 base units.** The view accumulated in i64, so any transfer
  above ~9.2e18 base units — barely ~9.2 tokens of an 18-decimal token — was *silently dropped*. The
  circuit, deltas, and storage now use i128 (max ~1.7e38); balances serialise as decimal strings
  (JSON numbers can't carry i128, and a client parsing a huge balance as f64 would corrupt it). On
  live WETH, **34 holders exceed i64::MAX** (top ~10,001 WETH = 1.0e22 base units) — every one of
  them previously mis-counted. **(2) Restart-replay.** The view is derived, not persisted, so it's now
  reconstructed from the durable facts on a warm restart, using the same circuit that maintains it
  live: sealed (immutable) segments fold to one net-per-address row directly in DuckDB (`HUGEINT` =
  i128 — no replaying millions of transfers), and only the small un-sealed hot tail is replayed. Both
  paths verified live: a cold-only restart reproduced 791/791 holders exactly; a hot-only restart
  replayed 840 transfers to reproduce 309/309. Transfer column names are read from the registry
  (USDC `from/to/value`, WETH `src/dst/wad`), never hardcoded. 30 tests green (+3). _RFC-0008 P0 for
  the compliance angle; both were the last known correctness gaps._
- **2026-07-15 — RFC-0001 step 6: multi-contract footprint re-measure (RFC-0001 complete).** Measured
  the full embedded pipeline on a genuine three-contract nest — USDC + WETH + DAI, **23 tables** — with
  everything live at once: combined `eth_getLogs`, per-table decode, per-table Parquet sealing + hot
  pruning, DuckDB SQL, and the IVM balance view (5,005 holders). **Peak RAM ~58 MB** (vs ~37 MB for a
  single contract), sealing 16,986 rows across 11 tables and pruning the hot store — still **2.8%** of
  the 2 GB budget, well under the 256 MB CI gate. Confirmed cross-contract serving: `/tables` returns
  all 23, WETH `c1__deposit` and DAI `c2__transfer` serve and query by their own columns. README status
  table + footprint section refreshed to the generalised (multi-contract, 8-tool) reality. This closes
  RFC-0001 — the transfer-only indexer is now a general ABI-driven multi-contract one, end to end.
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
