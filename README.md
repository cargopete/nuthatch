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
| redb hot store, entity point-reads | MCP server + `llms.txt` + skills (slice 5) |
| HTTP API (`/`, `/entities`, `/entity/{id}`) | reth ExEx tip mode, scaled Postgres mode (slice 6) |

Scope of the skeleton: **one chain (Ethereum), Transfer events only, RPC polling, redb-only.**
No IVM, no DuckDB/Parquet, no MCP yet.

### Measured footprint (the number nobody else publishes)

| | |
|---|---|
| **Peak RAM** | **~33 MB** (indexing 7,013 USDC Transfers, live mainnet) |
| Binary size | 5.8 MB (release, stripped) |
| Budget | ≤2 GB RAM — **using 1.6%** of it |

Honest and reproducible: `nuthatch init 0xA0b8…eB48 && nuthatch dev --backfill 500`, sampled with
`ps -o rss`. Measured on the release build. This is the embedded-mode skeleton; the budget holds
as later slices land (Parquet/DuckDB attach read-only, redb hot layer stays bounded).

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

## Design principles (non-negotiable)

- **Single static binary**, zero external services in embedded mode.
- **≤2 GB RAM** for single-chain tip-following + serving (a CI-enforced budget, not a hope).
- **No phone-home** — no telemetry, no mandatory tokens. AI features are local-first (Ollama / BYO-key).
- **Determinism in the core** — decode, reorg, and entity derivation are deterministic and
  re-executable. LLMs write code and tests; they never sit in the runtime data path.

## Progress log

Newest first. One entry per push, tracking the [build order](CLAUDE.md#build-order-vertical-slices-each-ends-runnable).

- **2026-07-14 — Slice 1 gate closed.** 5 deterministic golden decode tests (fixed USDC-transfer
  fixture → exact output) pass; measured peak RAM **~33 MB** indexing 7,013 transfers — 1.6% of the
  2 GB budget. Both non-negotiables (tests + footprint) met, so slice 2 is unblocked.
- **2026-07-14 — Slice 1: walking skeleton.** `init` (ABI via Sourcify v2, Etherscan fallback) →
  `dev` (RPC log polling with round-robin failover) → deterministic ERC-20 `Transfer` decode →
  redb hot store → axum HTTP API. Verified alive against live mainnet USDC, keyless: 170+ transfers
  indexed in ~1.5s with correct decimal values. Scope: one chain, Transfer-only, RPC-poll, redb-only.

_Next: Slice 2 — Parquet sealing past finality + DuckDB read-only analytical SQL + reorg property tests._

## Licence

[AGPL-3.0-only](LICENSE).
