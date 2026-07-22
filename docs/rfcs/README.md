# Nuthatch RFCs

Design documents for the post-skeleton phase. Numbered in build order; each states its
dependencies. Status lifecycle: **Draft → Accepted → Implemented → (Superseded / Parked)**.
For everything *deferred or not-yet-done* across the whole series (the infra track + leftovers),
see **[backlog.md](../backlog.md)**. For the bar a release must clear before it's pointed at a real
workload unattended, see the **[production-readiness checklist](../prod-readiness.md)**.
RFCs **0019–0023** derive from the **[Jul–Aug 2026 roadmap](../high-level-roadmap-jul-aug-2026.md)**
(strategy agreed 2026-07-21); their decisions log is the authority for the choices they encode.
Statuses last reconciled against the [progress log](../progress-log.md) on 2026-07-21.

| RFC | Title | Depends on | Status |
|-----|-------|-----------|--------|
| [0001](0001-generalized-decode-and-nests.md) | Generalized event decode and multi-contract nests | — | **Implemented** |
| [0002](0002-horizon-nest.md) | The Horizon nest: first real-world nest | 0001 | **Implemented** |
| [0003](0003-reth-exex-tip-mode.md) | reth ExEx tip mode: wiring and latency measurement | — (parallel to 0001/0002) | Accepted — groundwork landed; ExEx mode deferred |
| [0004](0004-backfill-throughput.md) | Backfill throughput: measurement and optimization | 0001 | **Implemented** |
| [0005](0005-release-engineering-v0.1.0.md) | Release engineering: v0.1.0 | 0001, 0002 | **Implemented** (v0.3.0 shipped) |
| [0006](0006-grant-funding.md) | Grant funding: NLnet and EF ESP applications | 0002 (demo), 0003–0005 (roadmap) | Accepted — drafts + governance shipped; process ongoing |
| [0007](0007-launch-and-validation.md) | Launch and validation | 0005 | Accepted — launch kit shipped; launch ongoing |
| [0008](0008-compliance-pack.md) | Compliance pack (screening, flags, exposure, audit) | P0 (i128), slice 4, 0006 M1 | **Implemented** |
| [0009](0009-factory-and-dynamic-contract-discovery.md) | Factory and dynamic contract discovery | 0001, 0004 | **Implemented** |
| [0010](0010-admin-ui-and-webhooks.md) | The admin UI and webhooks — ease-of-use parity | 0001, 0005 | **Implemented** |
| [0011](0011-graph-network-nest-lodestar-migration.md) | The graph-network nest and the Lodestar migration | 0001, 0002, 0004, 0005 | **Parked after pilot** — wedge proven in prod; full migration not done |
| [0012](0012-multi-nest-runtime-and-nest-packaging.md) | Multi-nest runtime and content-addressed nest packaging | 0001, 0009, 0004 | **Implemented** (all 7 slices; verified live on Arbitrum) |
| [0013](0013-storage-and-query-engine-direction.md) | Storage and query-engine direction (DataFusion convergence, Turso deferred) | 0001, 0004 | Accepted — §3 SQL-over-the-tip shipped (DuckDB union); DataFusion deferred/gated |
| [0014](0014-firehose-class-extraction-traces-and-state.md) | Firehose-class extraction — traces and state diffs via ExEx | 0003, 0001, 0004 | Draft (deferred) |
| [0015](0015-the-delightful-core.md) | The delightful core — CLI/UX for the solo dev (the 0.5 north star) | — (polish over 0.1–0.4) | **Implemented** (all 6 slices: REPL, magical init, live feedback, `add`, MCP one-liner, prod recipes) |
| [0016](0016-governed-semantic-layer-and-agent-grade-mcp.md) | The governed semantic layer and the agent-grade MCP experience | 0001, 0012, 0015 | **Implemented** (S1 eval harness → S2 semantic layer → S3 errors-as-prompts + explain → S4 result shaping → S5 resources/prompts; Tier-B baseline pending a keyed run) |
| [0017](0017-builder-skill.md) | The builder skill — teaching coding agents to drive nuthatch | 0016, 0015 | **Implemented** (generated CLI ref + drift-checked authored skill; authoring eval pending a keyed run) |
| [0018](0018-what-a-nest-is-authored-logic-and-composable-nests.md) | What a nest is — first-class authored logic (SQL views); Starlark composition (retired) | 0001, 0012, 0013, 0016, 0017 | **§1 Implemented · §2 retired · §3 deferred** (2026-07-21) — §1 authored SQL views shipped (horizon-nest is the exemplar); §2 Starlark front-end **retired** (single graph nest, plain TOML — `graph-network-nest` fork binned, code shipped-but-unused); §3 hot incremental views deferred |
| [0019](0019-nest-registry-and-distribution.md) | The nest registry and distribution — publish, pull, private nests | 0012, 0001 | **Implemented** (2026-07-21) — all 3 slices (FsStore, S3 `ObjectStore`, private-nest auth); live S3 verification pending a VPS run |
| [0020](0020-nest-lifecycle-and-the-n-1-upgrade.md) | Nest lifecycle and the N-1 upgrade — kill the subgraph resync tax | 0012, 0019, 0018 §1, 0013 §3 | **Implemented** (2026-07-21) — roadmap thread 4 — **all 4 slices** (`nest diff` · `nest upgrade`: compatible hot-swap, breaking → new endpoint + deprecation, and segment reuse for decode-unchanged updates). The N-1 problem is solved |
| [0021](0021-the-multichain-roost.md) | The multichain roost — one runtime, many chains, one cursor each | 0012, 0009 | **Accepted** (2026-07-21); §0 amendment applied — **slice 1 shipped**: `[[chains]]` config, per-chain grouping, per-cursor runtime + budget; single-chain parity holds |
| [0022](0022-distributed-scaled-mode.md) | Distributed scaled mode — read/write planes, writer pool, dynamic placement | 0013, 0019, 0021 | **Accepted** (2026-07-21) — roadmap thread 2B; §0 brief amendment applied; design-now-build-later |
| [0023](0023-contract-state-eth-call-derive-first.md) | Contract state (eth_call) — derive-first, with a verifiable fallback | 0018 §1, 0001, 0013 §3, 0019 | **Accepted** (2026-07-21) — roadmap thread 1; **tier 1 opened**: recipe library + `nuthatch recipe` (`total_supply` derived, no eth_call); tiers 2–4 pending |

## Conventions

All RFCs honor [`CLAUDE.md`](../../CLAUDE.md) non-negotiables: single static binary, ≤2 GB RAM
CI budget, no phone-home, determinism in the core, AGPL-3.0. Measured numbers are cited from the
[README progress log](../../README.md); targets are labeled as targets, never as results.

Each RFC carries: status, author, date, dependencies, what it blocks, and the standard
Abstract / Motivation / Goals / Non-goals / Design / Implementation / Testing / Risks /
Alternatives / Open-questions structure (adapted where a doc is process rather than engineering).
