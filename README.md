# nuthatch

> **Turn any contract into a local SQL database.**
> One command. One tiny binary. Your box, your data — no subgraph to author, no Postgres to run, no
> monthly bill, no third-party API.

[![ci](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml/badge.svg)](https://github.com/nuthatch-indexer/nuthatch/actions/workflows/ci.yml)
· Website: [www.nuthatch-indexer.com](https://www.nuthatch-indexer.com)

```sh
cargo install --git https://github.com/nuthatch-indexer/nuthatch nuthatch

nuthatch init 0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48   # USDC — chain auto-detected
nuthatch dev            # backfills from deployment, follows the tip, serves an API
nuthatch sql "SELECT count(*), sum(CAST(value AS DECIMAL(38,0))) FROM usdc__transfer"
```

That's the whole thing. You had an address ninety seconds ago; now you're running `SELECT` over its
on-chain activity, on your own machine.

---

## Why nuthatch

Every other way to get your contract's data fails the solo dev *somewhere*:

| | author a subgraph? | infra to run | query | yours? | pay? |
|---|---|---|---|---|---|
| **The Graph** | yes — schema + manifest + AS mappings | — (decentralised) | GraphQL | no | query fees |
| **Goldsky / hosted** | no | — (their servers) | SQL/GraphQL | **no** | **monthly** |
| **Ponder** | yes — TS handlers | Node + Postgres | SQL | yes | free |
| **Subsquid** | yes | archive + Postgres | GraphQL | yes | free |
| **nuthatch** | **no — init from an address** | **one static binary** | **SQL (DuckDB)** | **yes** | **free** |

Nobody else hits all four of *zero authoring*, *zero infra*, *it's just SQL*, and *it's yours and it's
tiny*. That combination is the point — not any single feature.

- **Zero authoring.** `init 0xAddr` resolves the ABI (Sourcify → Etherscan), generates the schema and
  decoders, and scaffolds the project. You write nothing.
- **Zero infra.** A single static Rust binary. Embedded mode needs no Postgres, no Docker, no IPFS.
- **It's just SQL.** Your contract's events become per-event tables you query with real analytical SQL —
  the live tip *and* sealed history, one surface.
- **It's yours, and it's tiny.** ≤2 GB RAM for single-chain tip-following, CI-enforced. No telemetry, no
  phone-home, no mandatory API token, ever.

---

## Install

```sh
# from source (any platform with a Rust toolchain)
cargo install --git https://github.com/nuthatch-indexer/nuthatch nuthatch
```

Prebuilt binaries are attached to CI builds; a `curl | sh` installer is on the roadmap
(RFC-0015). Chains: Ethereum, Arbitrum One, and Base — **omit `--chain` and nuthatch probes each
for your contract's bytecode and picks the one it lives on.** Point at your own node with `--rpc`.

---

## Querying your data — the whole point

Every declared event becomes a table named `{alias}__{event}` (e.g. `usdc__transfer`), carrying the
event's fields plus `block_number`, `block_timestamp`, `tx_hash`, `log_index`, `address`.

```sh
# one-shot from the terminal (prints an aligned table; --json to pipe to jq)
nuthatch sql 'SELECT "from" AS sender, count(*) AS n FROM usdc__transfer GROUP BY 1 ORDER BY n DESC LIMIT 5'

# or over HTTP, against a running `nuthatch dev`
curl 'localhost:8288/sql?q=SELECT%20count(*)%20FROM%20usdc__transfer'
```

- **`nuthatch sql`** queries the local store when `dev` is stopped, and transparently falls back to the
  running instance's API when `dev` holds it — the same command works either way.
- **Hot + cold in one surface.** Queries span the live unsealed tip (redb) *and* sealed history
  (Parquet), transparently — you never think about the boundary.
- **Big-int friendly.** `uint256` values are exact text; each also gets a `{col}_dec` DECIMAL view, so
  `SUM(value_dec)` just works.
- **AI-native.** A Model Context Protocol server is compiled in (`nuthatch mcp`) — point Claude (or any
  MCP client) at your indexer and ask your contract's data in plain English, fully offline.

---

## How it works (the 30-second version)

```
RPC (or colocated reth ExEx)  →  deterministic decode  →  redb hot store (tip)
                                                            │
                                        past finality  →  content-addressed Parquet segments
                                                            │
                                        DuckDB attaches segments read-only  →  SQL (hot ∪ cold)
```

- **Deterministic core.** Decode, reorg handling, and entity derivation are deterministic and
  re-executable — same inputs, same content-addressed output. No LLM ever sits in the data path.
- **Reorg-safe by construction.** Reorgs only ever touch the mutable hot store; sealed segments are
  strictly past finality and immutable.
- **Single writer.** One ingestion thread writes; queries only ever attach read-only.

---

## Point an AI at it

nuthatch has a Model Context Protocol server compiled in, so a coding agent can query your contract's
data in plain English — offline, no phone-home. Wiring it is one step:

```sh
nuthatch dev &                  # the index the agent will query
nuthatch mcp --print-config     # prints a copy-paste config for Claude Code / any MCP client
```

Or add it to Claude Code directly:

```sh
claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288
```

Then just ask: *"what are the top USDC holders?"* — the agent writes the SQL and runs it against your
nest. (Making that correct on the first try is the [semantic-layer work](docs/rfcs/0016-governed-semantic-layer-and-agent-grade-mcp.md).)

**Teach your agent to *build* nests too.** Install the builder skill and an agent can drive nuthatch
itself — `init`, config, factories, compliance, roosts, troubleshooting — before you even have a nest:

```sh
cp -r skills/nuthatch-builder ~/.claude/skills/   # or your repo's .claude/skills/
```

Its CLI/config references are generated from the binary and CI-checked for drift, so the skill never
lies about a flag ([RFC-0017](docs/rfcs/0017-builder-skill.md)).

---

## Everything else it can do

The core is "your contract → SQL." Beyond that, nuthatch has a full feature set for teams and operators
who need more — none of it in the way of the happy path:

- **Many contracts, one nest.** Declare several contracts in `nuthatch.toml`; index them together.
- **Factory / dynamic contracts** (RFC-0009). Watch a factory (e.g. a pool factory); children are
  discovered at runtime and indexed into shared `{template}__*` tables — no redeploy per child.
- **Declarative + imperative derivation.** Incremental views maintained by DBSP (reorgs become
  retractions), plus a WASM transform layer for custom pure-function pipelines.
- **Compliance pack** (RFC-0008). Address labels, sanctions/watch-list screening, threshold & velocity
  flags, counterparty-exposure views, and a signed, replayable audit manifest.
- **Alerts & webhooks** (RFC-0010). HMAC-signed egress with a durable at-least-once outbox; a slow
  endpoint never blocks indexing.
- **Built-in admin UI.** A self-contained page at `/_admin/` — status, tables, view/nest inspector.
  Localhost-open; off-localhost it requires a token per request.
- **Roost — many nests, one runtime** (RFC-0012). Host many contracts/nests on the same chain in one
  process, sharing a single cursor and one `getLogs` per window — N nests for roughly one nest's RPC
  cost, with per-nest isolation and a per-runtime footprint budget.
- **Nest bundles + registry — bundle one, publish it, load it anywhere.** `nuthatch nest bundle` packs
  a nest's authored inputs into one portable, content-addressed `.bundle`; `nest load <bundle-or-url>`
  verifies and installs it — regenerating the decode registry and asserting it matches — so anyone runs
  your *exact* nest, hash-verified. Share at scale with a **registry** (RFC-0019): `nest publish <bundle>
  --registry <path|s3://…> --as name@version`, then `nest load name@version --registry …` — a filesystem
  path or any S3-compatible bucket (MinIO/S3/R2, via `AWS_*` env), with **private nests** behind your
  bucket's auth. Self-hosted-first: the registry is decoupled and never mandatory — a self-built bundle
  and `load <file|dir>` need no registry at all. (S3 backend: build with `--features object-store`.)
- **Safe upgrades — no resync tax** (RFC-0020). `nuthatch nest diff <old> <new>` classifies a nest
  update as *compatible* (additive only) or *breaking* (a consumer-observable change); `nuthatch nest
  upgrade --to <new>` then handles either kind. A **compatible** update is **hot-swapped with zero
  downtime** — it serves the old version, indexes the new one concurrently, and atomically flips the
  endpoint the moment the new one catches up, so the served address never changes. A **breaking** update
  instead serves the new version on a new endpoint (`/next`) alongside the old — which keeps working, now
  carrying a `Deprecation` header — so downstream migrate on their own clock before the old is sunset.
  Either way, updating a nest stops being a subgraph-style genesis resync — and when a compatible
  update's decode is unchanged, the new version **mounts the old's sealed content-addressed segments**
  instead of re-indexing history at all: a true no-re-index upgrade subgraphs structurally can't do.
- **Metrics.** Prometheus `/metrics` — tip lag, rows decoded/sealed, reorgs, query counts, RSS.

---

## Running it in production

nuthatch is built to be **fronted**, not exposed raw — gateways, auth, and metering are the operator's
layer; nuthatch ships the *guards* (query timeout, row cap, concurrency limit, a filesystem-access
denylist on `/sql`) and *signals* (`/metrics`) that make fronting it safe. It binds `127.0.0.1` by
default; `--listen` elsewhere and put a gateway in front. See [`docs/operators.md`](docs/operators.md).

- **Footprint:** ≤2 GB RAM single-chain, single static binary, graceful SIGTERM shutdown with
  checkpointed resume.
- **Durability:** content-addressed segments are safe to copy while running; back up the nest directory.
- **`dev` is the serve command** — it backfills, follows the tip, and serves in one process.
  Copy-paste **systemd** and **Docker** recipes are in [`docs/operators.md`](docs/operators.md#deploy-recipes-copy-paste).

---

## Project

- **Design** lives in [RFCs](docs/rfcs/) (0001–0015); the north star and the CLI/UX direction are
  [RFC-0015](docs/rfcs/0015-the-delightful-core.md). Deferred/leftover work is in
  [`docs/backlog.md`](docs/backlog.md); the running log is [`docs/progress-log.md`](docs/progress-log.md).
- **Governance:** a grant-funded public good (NLnet / EF-ESP), AGPL-3.0. No hosted service, no token, no
  phone-home. See [`GOVERNANCE.md`](GOVERNANCE.md) and the standing design brief [`CLAUDE.md`](CLAUDE.md).
- **Out of scope:** a hosted/metered service, non-EVM chains before EVM is airtight, or any deployment
  story beyond binary + compose.

---

<p align="center"><i>be your own indexer.</i></p>
