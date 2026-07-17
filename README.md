# nuthatch

> **Be your own indexer.** One Rust binary, one command, live indexed API in under two minutes -
> AI-native, and with no mandatory third-party data API to trust or pay. Ever.

[![ci](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

Self-hosted-first, AI-native blockchain indexer. Embedded mode runs as a single process with no
external services - no Postgres, no Docker, no IPFS. See [`CLAUDE.md`](CLAUDE.md) for the standing
design brief, [`GOVERNANCE.md`](GOVERNANCE.md) for sustainability + neutrality, and
[`docs/operators.md`](docs/operators.md) for running it as a service.

## Status: embedded mode built end-to-end; scaled mode + reth ExEx outstanding

The embedded single-binary path works from `init → dev → live API`, with multi-contract ABI-driven
decode, reorg-safe storage, finality-sealed Parquet, DuckDB SQL, an incrementally-maintained balance
view, sandboxed WASM transforms, and an MCP server - all in one process, no external services. What
remains is the scaled (Postgres / DataFusion) mode and wiring reth ExEx to a node.

| Working now | Outstanding |
|---|---|
| `init` → multi-contract ABI resolve (Sourcify → Etherscan, EIP-1967/legacy-OZ proxy) → scaffold (+ `schema.json`, `llms.txt`, skills) | reth ExEx wiring - `Source` trait ready; needs a synced node |
| RPC log polling with round-robin failover, behind a `Source` trait | scaled Postgres mode (`HotStore` trait) + DataFusion federation |
| Deterministic decode of **every declared event of every contract** (topic0-keyed registry → one table per `{alias}__{event}`) | effectful transform worlds + signed pipeline manifests |
| Reorg self-healing (block-hash checkpoints → hot-store rollback) | governed semantic layer + natural-language queries |
| Per-table finality-gated content-addressed Parquet sealing + hot-store pruning | IVM generalisation (derived views are DuckDB SQL over sealed data today) |
| Read-only analytical SQL (DuckDB) - one view per table over sealed segments | GraphQL compatibility layer |
| `GET /tables` + `GET /table/{name}` (hot+cold merged) - the full data model | |
| IVM balance view (DBSP) - **i128** base units, reorg = retraction | |
| IVM restart-replay - the views rebuild from stored facts on restart | |
| Labels + direct counterparty-exposure view (DBSP) - content-addressed label snapshots, `/exposure/{addr}` | threshold/velocity flags, effectful worlds, alert webhooks (RFC-0008 C3–C6) |
| Pure sanctions screening - content-addressed list snapshots × a zero-capability WASM component → sealed `sanction_hit` annotations, replayable `nuthatch screen` | signed pack manifest + `pack verify` / `audit replay` (RFC-0008 C6) |
| Threshold & velocity flags - per-transfer `threshold_flag` annotations + a DBSP windowed velocity view (i128, reorg = retraction), served at `/flags` | alert webhooks (RFC-0008 C5) |
| Effectful WASM stages - per-component capability grants (`kv` now, HTTP next), imports checked against the grant at load, annotations-only output | wasi:http-sandboxed egress variant (optional) |
| Alert webhooks - flag/hit annotations (and reorg `flag_retracted`) POSTed at-least-once via a durable outbox that never blocks the indexer | |
| Signed compliance-pack manifest (`pack build`/`verify`, ed25519) + `audit replay`/`report` (re-prove sealed annotations) + MCP `flags`/`exposure`/`screen_status` | **RFC-0008 complete - all 8 RFCs shipped** |
| Factories & dynamic contract discovery ([RFC-0009](docs/factories.md)) - `[[templates]]`/`[[factories]]`, tip + backfill + reorg-safe, address-list→topic0 flip at scale, `{template}__children` view. **It indexes Uniswap.** | |
| Built-in admin UI at `/_admin/` (RFC-0010 Part A) - embedded, no framework/CDN, status + table browser + SQL runner + nest inspector, read-only, localhost-gated | tip-finality webhook path + retractions (RFC-0010, deferred - needs hot-row SQL) |
| User webhooks (RFC-0010 Part B) - `[[webhooks]]` POST sealed rows matching a `where` to a URL via the shared at-least-once outbox; `since` suppresses backfill history | |
| Signed webhook egress (RFC-0010 Part B) - a webhook `secret` signs each delivery with `X-Nuthatch-Signature: sha256=<hmac>` over the exact bytes sent (HMAC-SHA256, no new dep) | |
| Per-contract event allowlist (RFC-0011) - `events = ["Transfer", ...]` in `[[contracts]]` decodes only the listed events (default = all); narrows both decode and the getLogs fetch, typo = loud build error | the `graph-network-nest` + Lodestar migration (cross-repo) |
| WASM transform runtime (pure, sandboxed, batched Arrow) | |
| MCP server (stdio, 8 tools, offline) + `schema.json` + `llms.txt` + `.claude/skills` scaffold | |
| redb hot store, entity point-reads with cold (DuckDB) fallback | |

Scope today (**v0.1.0**): **Ethereum + Arbitrum One + Base**, all contract events decoded across a
multi-contract nest, RPC polling (reth ExEx designed + stubbed), embedded storage (redb hot +
DuckDB/Parquet cold), ~20× faster seal-direct backfill, and an operator surface (`/metrics`, `/sql`
guards, graceful shutdown). The scaled (Postgres/DataFusion) mode and reth ExEx tip-following are the
main things not built yet.

### Measured footprint (the number nobody else publishes)

| | |
|---|---|
| **Peak RAM** | **~58 MB** (3-contract nest, 23 tables - hot indexing + per-table sealing + DuckDB SQL + IVM, live mainnet) |
| Single contract | ~37 MB (USDC alone) |
| Binary size | 67 MB (release; DuckDB + DBSP + wasmtime statically bundled - 5.8 MB without them) |
| Budget | ≤2 GB RAM - **using 2.8%** of it |

Honest and reproducible: `nuthatch init 0xA0b8…eB48 0xC02a…6Cc2 0x6B17…71d0F && nuthatch dev
--backfill 400`, sampled with `ps -o rss`. Measured on the release build with the full embedded
pipeline active - the run above sealed 16,986 rows across 11 tables of the three contracts (USDC,
WETH, DAI; 23 tables total) and pruned the hot store, while the IVM view tracked 5,005 holders. The
RAM budget is enforced in CI (a `footprint` job fails the build above 256 MB - generous headroom over
the measured ~58 MB); the binary is large because DuckDB, DBSP, and wasmtime are statically bundled
(still a single file - the embedded-mode non-negotiable). Hot layer stays bounded by pruning sealed
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

It bridges to the local `nuthatch dev` - no external calls, no telemetry, no gated data API.

## Design principles (non-negotiable)

- **Single static binary**, zero external services in embedded mode.
- **≤2 GB RAM** for single-chain tip-following + serving (a CI-enforced budget, not a hope).
- **No phone-home** - no telemetry, no mandatory tokens. AI features are local-first (Ollama / BYO-key).
- **Determinism in the core** - decode, reorg, and entity derivation are deterministic and
  re-executable. LLMs write code and tests; they never sit in the runtime data path.

## Progress log

The per-push build log lives in [`docs/progress-log.md`](docs/progress-log.md) - one entry per push,
newest first, tracking the [build order](CLAUDE.md#build-order-vertical-slices-each-ends-runnable).

## Licence

[AGPL-3.0-only](LICENSE).
