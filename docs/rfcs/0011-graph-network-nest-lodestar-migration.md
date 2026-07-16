# RFC-0011: The graph-network nest and the Lodestar migration

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-16
- Depends on: RFC-0001 (Implemented), RFC-0002 (Implemented — chain registry with
  Arbitrum One + FinalizedTag, `init --from`, nest views, `nuthatch check`,
  `block_timestamp`), RFC-0004 (Implemented — `dev --seal-direct` + pipelined
  backfill; effectively required at this nest's history size), RFC-0005 v2 (the rc
  is the pilot artifact; §6 guards/metrics are what makes hosted fronting safe)
- Blocks: nothing hard; it is the agreed first target of the GraphOps pilot
  (RFC-0007 v2 Phase 1.5) and the strongest launch narrative available
- Note: this design predates its number — it was drafted alongside the v1 RFC set,
  then 0008–0010 took compliance/factories/UI. Committed now because the pilot
  ("GraphOps runs Nuthatch for Lodestar") needs its design doc in-tree.

## Abstract

Extend the Horizon nest into `graph-network-nest`: a published nest covering the
Graph Protocol contract suite on Arbitrum One, sufficient for Lodestar to stop paying
for event-derived data — The Graph gateway (`GRAPH_API_KEY`) for everything
event-derived, the Amp node (`AMP_ENDPOINT`), and the token-metadata API
(`TOKEN_API_KEY`) — leaving a free-tier residual (the QoS oracle subgraph) and the
off-chain sources that were never indexing's job. Migration proceeds panel-by-panel,
each gated on a pinned-block parity check against the gateway via the existing
`nuthatch check` framework. Deployment target: **GraphOps-hosted Nuthatch, with
Lodestar as the first tenant** (the agreed pilot), with the Hetzner self-hosted
instance as shadow/fallback. End state: Lodestar's recurring data spend ≈ $0, its
gateway-ingest cron deleted, and the launch narrative "Lodestar runs on Nuthatch,
operated by GraphOps" made literally true.

## Motivation

Lodestar today pays or depends on: gateway queries against the network subgraph,
Alchemy RPC, an ampd deployment for the Horizon activity feed, and a token-metadata
API. Everything *paid* is event-derived — exactly what Nuthatch produces. Beyond
cost, this migration is: (a) the production soak of RFCs 0001/0002/0004 at real
scale (~10 contracts, years of history, a live product with users on top); (b)
external correctness pressure (wrong APRs get noticed within hours); (c) the pilot
that proves the operator model end to end — GraphOps operates, Lodestar consumes,
the binary stays sovereign; (d) the best possible case study, published where the
Graph community already reads.

## Goals

1. `graph-network-nest` published (its own repo, `init --from`-consumable), covering
   the §1 contract set with the §2 derived views.
2. Lodestar panels migrated in §Migration order, each gated on parity vs the gateway.
3. Env end-state: `AMP_ENDPOINT` and `TOKEN_API_KEY` removed; `GRAPH_API_KEY`
   downgraded to free tier (QoS-only); `ARBITRUM_RPC_URL` no longer load-bearing.
4. Lodestar's `src/lib/ingest/` gateway-cron pipeline deleted (replaced by reads
   against Nuthatch `/sql`).
5. The pilot shape honored: GraphOps-hosted primary, Hetzner shadow, per-panel
   instant flip-back.

## Non-goals

- The QoS oracle subgraph (off-chain measurement data via DataEdge calldata + IPFS
  payloads — needs calldata + file ingestion Nuthatch doesn't have; stays on The
  Graph's free tier, well under 100K queries/month; the honest residual).
- Off-chain sources: CoinGecko/DefiLlama, forum/GitHub intel, Studio APIs, Push,
  indexer `/status` probes. Never indexing's job.
- Lodestar's wallet/write actions — untouched; this RFC is reads only.
- Factories (RFC-0009): the Graph contract suite is static; this nest needs none.
- IPFS subgraph-metadata enrichment ships as a stretch (§4), not a gate.

## Design

### 1. Contract set (Arbitrum One)

The nest indexes, at minimum (addresses vendored at authoring time from the
graphprotocol address book; the Horizon nest already vendors the first two):

| Alias | Contract | Feeds |
|---|---|---|
| `staking` | HorizonStaking | provisions, thawing, operator set, Horizon-era delegation |
| `service` | SubgraphService | allocations, POIs, rewards + query-fee collection |
| `legacy_staking` | L2Staking / StakingExtension | pre-Horizon allocation + delegation history (stake-history charts, realized-APY lookbacks) |
| `gns` | L2GNS | subgraph publishes, versions, metadata hashes, transfers, deprecations |
| `curation` | L2Curation | signal mint/burn, curator positions |
| `rewards` | RewardsManager | RewardsAssigned/Denied (APR inputs) |
| `epochs` | EpochManager | epoch parameters (epoch table derivation) |
| `disputes` | DisputeManager | dispute lifecycle, slashing outcomes |
| `escrow` | PaymentsEscrow / TAP collector | GraphTally: escrow balances, RAV redemptions |
| `token` | GraphToken (L2GRT) | mint/burn only — **requires the per-contract `events` allowlist** |

**Amendment this RFC forces on RFC-0001 (small, now-blocking):** per-contract
`events = ["Transfer", ...]` allowlist in `[[contracts]]`. Indexing every L2GRT
Transfer is millions of irrelevant rows; the allowlist was flagged "cheap, later" in
RFC-0001 and this nest is the "later." Implementation is a registry filter at build
time — an afternoon.

### 2. Derived views (mapped to Lodestar panels)

Extends the Horizon nest's shipped views; each names its consuming panel so scope is
auditable:

- `indexers` (Directory, Profiles): stake, delegation totals, capacity, reward/fee
  cuts + last-changed timestamps, allocation counts, cumulative rewards/fees.
- `allocations` (Profiles, POI dashboard): full lifecycle incl. legacy closures,
  POIs, per-allocation rewards.
- `delegations` / `delegation_events` (Portfolio, Flows chart, activity feed):
  positions with Active/Thawing/Withdrawable derivation (thawing-period fold) plus
  the raw stream for inflow/outflow and 7-day trends.
- `epochs` (Overview): boundaries from EpochManager events + per-epoch fee/reward
  aggregations (uses `block_timestamp`, shipped).
- `curation_signal` / `curators` (Curator portfolio, signal/stake ratios).
- `subgraphs` (Directory/detail): GNS publishes, version history, deprecations,
  transfers; metadata *hashes* now, resolved metadata via §4 later.
- `tap_escrow` / `rav_redemptions` (Payments page).
- `disputes_slashing` (Profiles).
- `horizon_activity` (Activity feed): the shipped Horizon-nest views, unchanged —
  this panel migrates first because it already exists and directly replaces ampd.

**The APR decision (the one hard derivation).** Lodestar's signal-weighted APR and
effective-cut math consumes rewards-accumulator state the network subgraph partially
maintains via eth_calls. Decision: derive per-allocation *realized* rewards purely
from `RewardsAssigned` events — exact, event-native — and make realized-basis APR
(30-day rolling from closed allocations) the primary method, which is already
Lodestar's preferred method with estimation as fallback. The estimated-APR fallback
keeps a small periodic RPC state read (accumulator snapshot) as an annotation
refreshed on Lodestar's existing cron cadence — explicitly outside the deterministic
core, documented as such. This removes the only eth_call entanglement without
pretending it doesn't exist. Parity for this panel accepts *documented divergence
within tolerance* (realized basis vs the subgraph's mixed basis), never silent
difference.

**Freshness, stated honestly (unchanged tradeoff from RFC-0002):** derived views
read sealed data and lag by Arbitrum's finality window; raw event tables are
tip-fresh. Every migrated panel tolerates ~10-minute derived-data lag except the
Activity feed, which reads raw tables. This limitation remains the concrete
motivation for IVM generalization and is documented per-view in the nest README.

### 3. Scale posture

First nest at real scale: ~10 contracts, ~4 years of Arbitrum history, order 10⁷
events after the token allowlist. Consequences:
- Backfill runs `dev --seal-direct --concurrency K` (shipped) against an archive
  Arbitrum endpoint (deployment-block detection already proved public non-archive
  endpoints lie about historical `eth_getCode`; the same caveat applies to deep
  `getLogs`). Record wall-clock + ev/s in `bench-report.json` — this is the W2
  workload at full size, and the published number for the nest README.
- Footprint: publish measured RSS + total Parquet size for the full nest. This is
  the 2 GB budget's first real test; expectation is low hundreds of MB during
  pipelined backfill, tens at steady state — verify, don't assume.
- Parity: `checks/*.sql` per §2 view with fixtures recorded from the deployed
  network subgraph at a pinned block (`nuthatch check` shipped; `--update` records;
  hermetic in CI). Discrepancies are the actual work — budget real time for
  allocation-status folding edge cases (Closed-after-Resized ordering) and legacy/
  Horizon boundary semantics.

### 4. Stretch: IPFS metadata enrichment (first production effectful component)

Subgraph names/descriptions/images resolve from IPFS hashes in GNS events. Ship the
effectful component (`wasi:http` → own IPFS gateway preferred) writing an
`annotations.subgraph_metadata` table — annotations only, per the purity rule; this
is deliberately the first production exercise of RFC-0008's C4 machinery: small,
retryable, low-stakes. If it slips, Lodestar keeps resolving metadata client-side as
today; nothing gates on it. (Token metadata — name/symbol/decimals — resolves the
same way or via a one-shot RPC-read annotation at nest init; either deletes
`TOKEN_API_KEY`.)

### 5. Deployment and integration shape (the pilot)

- **Primary: GraphOps-hosted Nuthatch** running this nest, fronted by their gateway
  (auth/metering/rate-limiting — their layer, per RFC-0005 v2 §6). Nuthatch ships
  the guards (`/sql` timeouts, result caps, concurrency semaphore) and `/metrics`;
  their gateway decides *who*, our guards bound *how much*.
- **Shadow: the Hetzner instance** runs the identical nest from the identical tag
  (releases only — the pilot dogfoods RFC-0005's release process; Lodestar never
  points at an unreleased build). Segments are content-addressed: shadow and primary
  MUST produce identical segment hashes for identical ranges — a free cross-operator
  determinism check, and the strongest ops-level correctness signal available.
  Automate the comparison (a small script diffing the two manifests; alert on
  divergence).
- **Lodestar client:** one thin adapter in `src/lib/` speaking `POST /sql` to
  whichever base URL the per-panel `data_source` flag selects
  (`graphops | shadow | gateway`), mirroring the existing subgraph client's
  interface so hooks change minimally. Per-view TypeScript types generated from the
  nest's `schema.json` (small codegen script in the nest repo). Epoch *progress*
  (current block within epoch) reads Nuthatch's status endpoint tip watermark — no
  RPC call from Lodestar.

## Migration plan (panel order = risk order; every step has an instant flip-back)

0. RFC-0001 allowlist amendment; author + publish the nest repo; full backfill on
   both primary and shadow; record metrics; verify cross-operator segment-hash
   equality.
1. **Horizon Activity feed** — views already shipped and proven on live Arbitrum
   data; delete `AMP_ENDPOINT`. First env var falls.
2. **Indexer Directory + Profiles (event-derived fields)** — highest query volume,
   pure folds; parity gate: top-100 indexer rows exact at the pinned block.
3. **Delegator Portfolio + Flows** — thawing-fold is the trickiest view logic;
   parity gate: 20 sampled delegator portfolios exact.
4. **Epochs + Overview aggregates.**
5. **Payments (TAP) + Disputes.**
6. **Curation + Subgraph directory** (hashes now; §4 enrichment when it lands).
7. **APR/effective-cut** — last, per §2's decision; documented-divergence gate.
8. Delete `src/lib/ingest/`; `GRAPH_API_KEY` → free tier; remove `TOKEN_API_KEY`;
   repoint `ARBITRUM_RPC_URL` to non-load-bearing.
9. 30-day soak with the `data_source` flag armed; then write the migration up on
   Lodestar's blog — before/after spend, parity methodology, the cross-operator
   hash check — the case study, published where the Graph community reads.

## Testing and acceptance

- Every migrated panel passed its pinned-block parity gate before cutover; gates
  committed in `checks/` and runnable by anyone (`nuthatch check`, hermetic).
- Full-nest backfill metrics + footprint published in the nest README (bench-report
  artifacts, per the house rule).
- Cross-operator determinism: GraphOps-hosted and Hetzner-shadow segment hashes
  identical over the backfilled range; the comparison automated.
- Env end-state achieved; before/after spend documented in the write-up.
- 30-day soak with zero unresolved data-quality flips (a flip-back resets the clock
  for that panel only).
- Pilot success per RFC-0007 v2: a Lodestar panel served via GraphOps-hosted
  Nuthatch for 14 consecutive days.

## Risks

- **R1 — subgraph semantic drift at scale**: years of handler nuance in the network
  subgraph. The parity harness is the arbiter; match semantics deliberately or
  document divergence deliberately, never silently. This is the migration's real
  labor; the schedule should assume it.
- **R2 — contract-suite upgrades mid-migration** (post-Horizon iterations): the nest
  pins addresses + ABIs; upgrades are explicit nest versions; the flag covers gaps.
- **R3 — event-volume surprises**: if allowlisted contracts still exceed ~10⁸ rows,
  revisit segment sizing before panel 2; the published footprint is the tripwire.
- **R4 — three-hats coupling** (author/operator/consumer): an outage now implicates
  Nuthatch, GraphOps, and Lodestar at once. Mitigations are structural: releases
  only, per-panel flags, the shadow instance, and the §5 hash comparison localizing
  blame to data vs ops in minutes.
- **R5 — pilot-timeline pressure**: GraphOps's platform dates are theirs. Panels
  migrate when their parity gates pass; no gate is relaxed to hit a launch (the
  RFC-0005 v2 rule, restated here because this is where it will be tested).

## Open questions

1. Nest lineage: does `graph-network-nest` supersede `horizon-nest` (strict
   superset) or do both persist (small teaching nest + production nest)? Leaning
   both — the small one is the documentation example, this one is the production
   artifact; revisit if maintaining two view sets chafes.
2. Should the QoS residual eventually motivate a calldata+IPFS ingestion RFC, or
   does QoS data move on-chain/into Horizon anyway? Park; free tier covers the
   interim indefinitely.
3. Does GraphOps want the nest's `checks/` runnable in *their* CI against their
   instance (operator-side parity as a service feature)? Raise on the call — it's
   zero core work (the framework is generic) and a genuinely novel thing for a
   hosted indexer to offer its tenants.
