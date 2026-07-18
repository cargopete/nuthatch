# Nuthatch RFCs

Design documents for the post-skeleton phase. Numbered in build order; each states its
dependencies. Status lifecycle: **Draft → Accepted → Implemented → (Superseded / Parked)**.
Statuses last reconciled against the [progress log](../progress-log.md) on 2026-07-18.

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
| [0012](0012-multi-nest-runtime-and-nest-packaging.md) | Multi-nest runtime and content-addressed nest packaging | 0001, 0009, 0004 | Accepted — implementing (pack/mount done; roost pending) |
| [0013](0013-storage-and-query-engine-direction.md) | Storage and query-engine direction (DataFusion convergence, Turso deferred) | 0001, 0004 | Draft |
| [0014](0014-firehose-class-extraction-traces-and-state.md) | Firehose-class extraction — traces and state diffs via ExEx | 0003, 0001, 0004 | Draft (deferred) |

## Conventions

All RFCs honor [`CLAUDE.md`](../../CLAUDE.md) non-negotiables: single static binary, ≤2 GB RAM
CI budget, no phone-home, determinism in the core, AGPL-3.0. Measured numbers are cited from the
[README progress log](../../README.md); targets are labeled as targets, never as results.

Each RFC carries: status, author, date, dependencies, what it blocks, and the standard
Abstract / Motivation / Goals / Non-goals / Design / Implementation / Testing / Risks /
Alternatives / Open-questions structure (adapted where a doc is process rather than engineering).
