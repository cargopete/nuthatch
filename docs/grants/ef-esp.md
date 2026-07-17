# Ethereum Foundation ESP inquiry — nuthatch

_Draft. Public, PR-reviewed (RFC-0006). ESP starts with a short inquiry, not a full proposal — this
is the inquiry text plus the milestone table it leads to. Co-funding with NLnet is disclosed with
distinct milestone ownership (Rule 1: no milestone funded twice)._

- **Program:** Ethereum Foundation Ecosystem Support Program (ESP), small-grants / inquiry track.
- **Requested:** $50–90K over 12 months.
- **Applicant:** Pete (cargopete), sole maintainer.
- **Project:** nuthatch — https://github.com/nuthatch-indexer/nuthatch (AGPL-3.0-only).

## The pitch (the TrueBlocks lane)

Ethereum's read side is quietly centralised: most apps reach the chain through a few hosted indexers
and RPC providers. **nuthatch is local-first Ethereum indexing** — one Rust binary, `init 0xAddr
--chain mainnet`, live queryable API in under two minutes, ~40 MB RAM, no mandatory third party in
the data path. It is the same public-good lane the EF funded with **TrueBlocks** (local-first
indexing, 2018), generalised: arbitrary contracts from their ABI, a declarative incremental-view
layer, an SQL + point-read + offline-AI (MCP) surface, and content-addressed sealed history for
verifiable re-execution.

**Already shipped (v0.1.0):** multi-contract full-ABI decode across **Ethereum, Arbitrum One, and
Base**; reorg-safe redb-hot / Parquet-cold storage with DuckDB SQL; a DBSP incremental balance view
(reorg = retraction); a compiled-in MCP server; **measured ~40 MB RAM** and a **~20×** in-the-open
backfill-throughput progression (~289 → ~5,837 ev/s) with byte-identical determinism proven across
paths. On crates.io and as prebuilt binaries.

## Why now — two facts that de-risk this

1. **Adoption signal.** An independent Ethereum-infrastructure operator (GraphOps, ~8,000 physical
   cores) is preparing a hosted offering of nuthatch under AGPL and shares revenue with the
   maintainer — the ecosystem-adoption signal ESP weights most, disclosed per RFC-0006 Rule 4. This
   application funds the ecosystem-facing roadmap hosting revenue wouldn't prioritize; nothing here
   depends on that revenue materialising.
2. **The hard blocker is already cleared.** The headline ExEx milestone (below) needs reth embedded
   in-process. The two things that could have killed it are done: the toolchain needle is threaded
   (rustc 1.95 compiles both reth and our dbsp dependency), and **reth v2.4 resolves cleanly
   alongside the core** (913-package graph, no version conflict). So this is de-risked engineering,
   not speculation.

## Milestones (ESP-owned; distinct from the NLnet set)

| Milestone | Deliverable | Effort |
|---|---|---|
| **E1** | **reth ExEx tip mode** — a `nuthatch-node` binary embedding reth with the indexing core installed as an Execution Extension, so tips arrive in-process (no RPC, no third party) at native block time. The `Source` trait and push→pull bridge are already built and tested; this wires the real node and publishes an honest p50/p99 notification→queryable latency number with methodology. | 6 wk |
| **E2** | **OP-stack multi-chain ExEx** — extend E1 to Base / OP-mainnet (OP-stack reth), the high-leverage L2 story; registry entries already exist. | 4 wk |
| **E3** | **GraphQL compatibility layer** — a migration path for existing Ethereum subgraph consumers to self-hosted infrastructure without rewriting queries (open-standards continuity). _Co-funded milestone: if awarded by NLnet as M3, ESP funds E1/E2 instead — no double funding._ | 6 wk |
| **E4** | **Benchmarks + verifiability writeup** — CI-gated backfill/tip-latency/RSS benchmarks as public artifacts; a plain-language account of the "verifiability = deterministic re-execution over content-addressed segments" model. | 2 wk |

The commons-flavoured milestones (semantic layer, IVM generalization, security audit) are the NLnet
set (see `nlnet.md`); ESP owns the Ethereum-specific ExEx/latency/OP-stack work. Co-funding is
disclosed to both; each milestone has a single owner.

## Scope integrity (what we will not do — RFC-0006)

No token, no decentralised-network features, no telemetry, no relicensing, no partner-only features
in the AGPL core. Verifiability stays deterministic re-execution — no zk/TEE. If any term requires an
item on this list, we decline it.
