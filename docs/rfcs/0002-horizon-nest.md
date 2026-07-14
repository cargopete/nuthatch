# RFC-0002: The Horizon nest — first real-world nest

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: RFC-0001
- Blocks: RFC-0005 (v0.1.0 requires one real nest), RFC-0006 (grant demo), RFC-0007 (launch demo)

## Abstract

Ship `horizon-nest`: a published, reusable nest indexing Graph Protocol Horizon staking
activity on Arbitrum One — three contracts, derived entities (operators, allocations,
delegations, indexer totals, global stats, hourly/daily aggregations) — validated for
correctness against the deployed community subgraph it mirrors, and consumed by Lodestar
as its production data source for these metrics. This makes the website's /example page
literally true and establishes the nest publishing/consuming workflow
(`nuthatch init --from <git-url>`).

## Motivation

Three birds (fittingly): the missing multi-contract dogfood for RFC-0001; real data
Lodestar needs anyway (indexer performance metrics without Studio/gateway dependency);
and the launch demo that lands with the exact audience (Graph ecosystem engineers
watching their own network indexed by a 37 MB binary). Credits and mirrors
PaulieB14/horizon-indexer-subgraph.

## Goals

1. `nuthatch init --from https://github.com/cargopete/horizon-nest` → indexing Arbitrum
   Horizon activity with zero manual steps beyond an RPC choice.
2. Derived entities matching the source subgraph's semantics, with a documented,
   automated parity check against the deployed subgraph.
3. Arbitrum One chain support (chain registry entry, finality policy).
4. Lodestar consuming the nest's `/sql` endpoint in production for at least one
   dashboard panel.

## Non-goals

- Generalized IVM for the derived entities (views are DuckDB SQL in v1; see §4 and the
  freshness tradeoff). DBSP generalization is the semantic-layer slice.
- Indexing the full Graph protocol surface (curation, GNS, escrow). Scope is exactly the
  source subgraph's three contracts.
- Tip-latency guarantees on Arbitrum (RFC-0003 covers Ethereum ExEx; Arbitrum runs RPC
  polling).

## Design

### 1. Chain registry: Arbitrum One

New `chains.rs` registry entry (the first beyond mainnet — the registry itself is the
deliverable, Arbitrum is its first proof):

```
ChainSpec {
  id: 42161, name: "arbitrum-one",
  default_rpcs: [<3-4 public endpoints>],   // round-robin as on mainnet
  block_time_hint_ms: 250,
  finality: Depth(1800),                    // §2
  explorer_abi: Arbiscan (Etherscan v2 multi-chain API),
}
```

`--chain arbitrum-one` selects it; Sourcify already covers chain 42161 for ABI
resolution.

### 2. Finality policy on an L2

Arbitrum's true finality is L1 confirmation of the batch (~10–20 min); sequencer
reorgs are rare but the chain can reorganize until L1 posting. Options:
(a) fixed conservative depth; (b) track `finalized` block tag (Arbitrum nodes expose
L1-aware `finalized`); (c) track the L1 bridge.

**Decision: (b) when the RPC supports the `finalized` tag (probe at startup: if
`eth_getBlockByNumber("finalized")` succeeds and advances, use it), falling back to
(a) Depth(1800) (~7.5 min at 250 ms blocks).** Rationale: (b) is correct by
construction and free where available; the fallback keeps public-RPC compatibility.
The sealing invariant is unchanged: the cold layer never sees a reorg. Hot-store
memory holds more unsealed blocks than mainnet (1800 × sparse Horizon events — measured
in acceptance; Horizon emits a few events per minute, so this is trivially within
budget).

### 3. The nest definition

Published repo `cargopete/horizon-nest`:

```
nuthatch.toml        # three contracts, aliases, start blocks (vendored below)
abis/                # vendored ABIs (frozen; registry hash stable)
views/               # derived-entity SQL (§4)
checks/parity.sql    # parity queries (§5)
README.md            # provenance, credit to Paul Barba, freshness caveat
```

Contracts (Arbitrum One):
- `staking` — HorizonStaking `0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03` → `OperatorSet`
- `service` — SubgraphService `0xb2Bb92d0DE618878E438b55D5846cfecD9301105` →
  `AllocationCreated`, `AllocationClosed`, `AllocationResized`,
  `IndexingRewardsCollected`, `QueryFeesCollected`
- `extension` — StakingExtension `0x3bE385576d7C282070Ad91BF94366de9f9ba3571` →
  `StakeDelegated`, `StakeDelegatedLocked`, `StakeDelegatedWithdrawn`

`init --from <git-url>` clones (or `--from ./dir` copies), validates the toml against
the current schema version, resolves nothing (ABIs vendored), and writes the project.
Nests are plain repos — publishing is `git push`, consuming is one flag. No registry
service, deliberately.

### 4. Derived entities as SQL views (and the freshness tradeoff)

`views/*.sql` are DuckDB views created over the per-event tables at serve time:

- `operators.sql` — latest `OperatorSet` per (indexer, verifier): window over
  `staking__operator_set` by `(indexer, verifier)` ordered by `(block_number, log_index)`.
- `allocations.sql` — fold Created/Resized/Closed into current state + status per
  allocation id (`COALESCE` chain over three tables, latest-event-wins).
- `delegations.sql` — Σ delegated − Σ locked/withdrawn per (indexer, delegator), plus
  a `delegators_active` count view.
- `indexers.sql` — totals per indexer: rewards, query fees, active/closed allocation
  counts (join allocations + `service__indexing_rewards_collected` +
  `service__query_fees_collected`).
- `aggregations.sql` — `time_bucket`-style daily/hourly rollups of rewards, fees, and
  delegation events (DuckDB `date_trunc` over block timestamps — requires the implicit
  `block_timestamp` column: **addition to RFC-0001's implicit columns, u64 seconds,
  taken from the block header during ingestion**).
- `global.sql` — one-row totals view over the above.

**Freshness tradeoff, stated honestly:** DuckDB views read sealed segments only, so
derived entities lag the tip by the finality window (§2). Raw event tables remain
tip-fresh via the hot path. v1 documents this ("derived views: finalized data; raw
events: tip") — acceptable for analytics dashboards, which is the consumer here.
Follow-up (not this RFC): register hot rows as an Arrow-backed DuckDB table per query
to close the gap. This limitation is the concrete motivation for IVM generalization,
and gets written down as such.

### 5. Parity validation against the source subgraph

`nuthatch check parity` (nest-local command) runs `checks/parity.sql` queries and
compares against the deployed subgraph's GraphQL over a pinned block range:

- Total rewards per indexer (top 20) — exact match expected.
- Allocation count by status at block B — exact.
- Delegation totals for 10 sampled (indexer, delegator) pairs — exact.
- Daily aggregation for 7 sampled days — exact.

Comparisons are at a fixed `block <= B` where B is sealed on our side and indexed on
theirs, eliminating freshness skew. Discrepancies fail loudly with row-level diffs.
This doubles as the first external-correctness fixture in CI (run on a recorded
response fixture, not live GraphQL, to keep CI hermetic; a `--live` flag hits the real
endpoint for manual runs).

### 6. Lodestar integration

Lodestar gains a data-source adapter hitting the nest's `/sql` with the `indexers` and
`aggregations` views for one production panel (indexer rewards over time). Runs on the
existing Hetzner box alongside the Graph indexer stack. Success is boring: the panel
serves from Nuthatch for 30 consecutive days.

## Implementation plan

1. Chain registry + Arbitrum entry + `finalized`-tag probe + `block_timestamp` implicit
   column (RFC-0001 amendment).
2. `init --from` (git + local dir), toml schema versioning.
3. Author the nest repo: toml, vendored ABIs, views, README with credit.
4. Backfill from each contract's deployment block on the Hetzner box; record backfill
   wall-clock and events/sec (feeds RFC-0004's baseline).
5. `nuthatch check parity` + fixtures; fix discrepancies (expect edge cases in
   allocation-status folding — Closed-after-Resized ordering).
6. Lodestar adapter + panel.
7. Update website /example: link the real nest repo, replace any remaining illustrative
   snippets with real view SQL, keep the "based on Paul Barba's subgraph" credit.

## Testing and acceptance

- All three ABIs decode with zero skipped non-anonymous events (RFC-0001 gate).
- Parity: all `checks/parity.sql` queries match the source subgraph at pinned block B.
- `init --from` on a clean machine → indexing within 2 minutes of RPC availability.
- Footprint: peak RSS during Arbitrum tip-following + serving ≤ 256 MB CI-scenario
  threshold; publish the measured number in the nest README (expected: low tens of MB —
  Horizon is sparse).
- Lodestar panel live for 30 days (acceptance completes retroactively; does not block
  v0.1.0 tagging, which requires only parity + publish).

## Risks

- **Subgraph semantic drift**: the source subgraph may have handler-level nuances not
  visible from its schema (e.g., how Resized affects totals). Mitigation: parity checks
  are the arbiter; where behavior is genuinely ambiguous, document the chosen semantics
  in the nest README rather than silently matching bugs.
- **Public Arbitrum RPC quality**: backfill from deployment blocks may be slow/flaky on
  public endpoints. The Hetzner box uses a paid/own endpoint; the nest README sets
  expectations for public-RPC users.
- **`getLogs` range limits** differ per provider on Arbitrum; the adaptive chunker from
  RFC-0004 lands after this — v1 uses a conservative fixed chunk (2k blocks) that works
  on the majors.

## Alternatives considered

- **Ship it as a mainnet-Ethereum nest instead** (avoid the L2 finality question):
  rejected — Horizon lives on Arbitrum; the L2 work is unavoidable and better done for
  the flagship nest than improvised later.
- **Materialize derived entities into tables at seal time** instead of serve-time views:
  better query latency, but couples sealing to view logic and complicates reorgs at the
  seam; rejected for v1, revisit with IVM generalization.

## Open questions

1. Should `check parity` become a general `nuthatch check` framework (nest-defined
   invariant queries run in CI)? Likely yes; keep the interface generic
   (`checks/*.sql` + expected-results fixtures) so it generalizes for free.
2. Nest versioning/upgrade story when views change (re-create views is trivial; toml
   contract changes require re-init) — document, don't engineer, for now.
