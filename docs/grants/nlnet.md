# NLnet / NGI application — nuthatch

_Draft. Public, PR-reviewed (RFC-0006 §Implementation-plan step 2). Verify the open call and re-check
the budget against the [progress log](../../README.md#progress-log) the week of submission (Rules 1–2:
anything shipped moves from "budget" to "evidence")._

- **Fund / call:** NGI Zero (Commons / Assure lineage — verify which is open at submission).
- **Requested:** €38,400.
- **Applicant:** Pete (cargopete), sole maintainer. Routed via Nixum Ltd (see RFC-0006 Q1).
- **Project:** nuthatch — https://github.com/nuthatch-indexer/nuthatch · https://www.nuthatch-indexer.com
- **License:** AGPL-3.0-only (core). Free, self-hosted-first public good; no direct monetization.

## Abstract (the sovereignty pitch)

Blockchain applications almost universally depend on a handful of hosted data providers to read the
chain — indexing services and RPC gateways that meter, gate, and can revoke access. **nuthatch lets
anyone be their own indexer**: one Rust binary, one command, a live queryable API in under two
minutes, with **no mandatory third-party data dependency, ever**. It runs in ~40 MB of RAM on a
laptop, decodes any contract from its ABI, serves SQL + a point-read API + an offline AI (MCP)
surface, and — uniquely among self-hosted indexers — has **zero phone-home**: no telemetry, no
tokens, no gated service in the data path. This is data sovereignty for the read side of web3, the
same way Plausible and Caddy are sovereignty for analytics and TLS.

This is framed as **trustworthy, self-hostable public infrastructure**, not "blockchain" — the
funded work (a governed semantic layer, incremental-view maintenance, an open-standards
compatibility path, and a security audit) is general data-infrastructure engineering that happens to
target public ledgers.

## What already exists (built and shipped — v0.1.0, 2026-07)

Not a proposal for vapourware. As of v0.1.0 (on crates.io and as prebuilt binaries), nuthatch:

- **Decodes any contract** across **Ethereum, Arbitrum One, and Base** — full multi-contract,
  full-ABI decode with automatic proxy resolution; not a fixed schema.
- **Reorg-safe storage**: mutable hot store (redb) at the tip; content-addressed **Parquet**
  segments past finality; DuckDB read-only analytical SQL over them.
- **Incremental view maintenance** (Feldera/DBSP): a per-address balance view where a reorg is a
  retraction, not a recompute — the differentiator vs imperative indexers.
- **Honest, published numbers** (the figure nobody else publishes): **~40 MB peak RAM**, CI-enforced.
  Backfill throughput measured and optimised in the open — **~289 → ~5,837 events/sec (~20× stacked)**
  with byte-identical determinism proven across paths; full methodology and artifacts in-repo.
- **AI-native, offline**: a Model Context Protocol server compiled into the binary, so a coding
  agent queries a running index with real schema instead of hallucinating — fully local.
- **Operator surface**: Prometheus `/metrics`, `/sql` resource guards, graceful shutdown.

## What this grant funds (commons-facing roadmap; Rule 1 — none of it started)

| Milestone | Deliverable | Effort | € |
|---|---|---|---|
| **M1** | **Governed semantic layer** + reliable natural-language / agent queries over it — a reviewed, versioned semantic model so an AI (or human) queries *meaning*, not raw tables. | 8 wk | 12,800 |
| **M2** | **IVM generalization**: user-declared entities as declarative views compiled to DBSP circuits; hot/cold view-freshness across the finality seam. Backfills and reorgs become batch/retraction runs of the same circuit. | 6 wk | 9,600 |
| **M3** | **GraphQL compatibility layer + subgraph-migration path** — lets the large existing base of Graph subgraph consumers move to self-hosted infrastructure without rewriting their queries. The open-standards continuity story. | 6 wk | 9,600 |
| **M4** | **Security review remediation** (the WASM host boundary in particular, via NLnet's Request-for-Services audit), a published **threat model**, a hardening release, and operator docs. | 4 wk | 6,400 |

Rate €40/h. **We explicitly request an NLnet ROS security audit** for M4. Verifiability is by
deterministic re-execution over content-addressed segments — no heavier cryptography needed, which
keeps the audit surface small and reviewable.

## Comparison

- **The Graph / hosted indexers, Alchemy/Infura-class RPC**: powerful, but a metered third party you
  must trust and can be cut off from. nuthatch removes the dependency entirely.
- **Ponder, Envio**: good developer experience, but Envio's self-host now requires a token for its
  HyperSync data service (phones home), and Ponder still needs a capable (often paid) RPC. nuthatch's
  wedge is Rust single-binary ops + **zero mandatory third-party API** + IVM correctness + AI-native.
- **TrueBlocks**: closest in spirit (local-first Ethereum indexing; EF-funded precedent) — nuthatch
  generalises the idea to arbitrary contracts, multiple chains, a declarative view layer, and an SQL
  + AI surface.

## Disclosure (RFC-0006 Rule 4)

An independent infrastructure operator (GraphOps) is preparing a hosted offering of nuthatch under
AGPL and shares revenue with the maintainer; **this application funds the commons-facing roadmap that
hosting revenue would not prioritize.** The project remains neutral — the AGPL license means any
operator may host it; no partner has exclusivity, a private fork, or roadmap veto.

## Who

Solo maintainer; the top project risk is bus-factor, mitigated by obsessive architecture docs
(RFCs 0001–0008 in-repo), the AGPL license, and funder diversity. Grant milestones map 1:1 to public
RFCs so progress is externally checkable.
