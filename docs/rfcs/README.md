# Nuthatch RFCs

Design documents for the post-skeleton phase. Numbered in build order; each states its
dependencies. Status lifecycle: **Draft → Accepted → Implemented → (Superseded)**.

| RFC | Title | Depends on | Status |
|-----|-------|-----------|--------|
| [0001](0001-generalized-decode-and-nests.md) | Generalized event decode and multi-contract nests | — | Draft |
| [0002](0002-horizon-nest.md) | The Horizon nest: first real-world nest | 0001 | Draft |
| [0003](0003-reth-exex-tip-mode.md) | reth ExEx tip mode: wiring and latency measurement | — (parallel to 0001/0002) | Draft |
| [0004](0004-backfill-throughput.md) | Backfill throughput: measurement and optimization | 0001 | Draft |
| [0005](0005-release-engineering-v0.1.0.md) | Release engineering: v0.1.0 | 0001, 0002 | Draft |
| [0006](0006-grant-funding.md) | Grant funding: NLnet and EF ESP applications | 0002 (demo), 0003–0005 (roadmap) | Draft |
| [0007](0007-launch-and-validation.md) | Launch and validation | 0005 | Draft |
| [0008](0008-compliance-pack.md) | Compliance pack (screening, flags, exposure, audit) | P0 (i128), slice 4, 0006 M1 | Draft |

## Conventions

All RFCs honor [`CLAUDE.md`](../../CLAUDE.md) non-negotiables: single static binary, ≤2 GB RAM
CI budget, no phone-home, determinism in the core, AGPL-3.0. Measured numbers are cited from the
[README progress log](../../README.md); targets are labeled as targets, never as results.

Each RFC carries: status, author, date, dependencies, what it blocks, and the standard
Abstract / Motivation / Goals / Non-goals / Design / Implementation / Testing / Risks /
Alternatives / Open-questions structure (adapted where a doc is process rather than engineering).
