# RFC-0008: Compliance pack

- Status: Draft (v2)
- Author: Pete (cargopete)
- Date: 2026-07-16 (v1: 2026-07-14)
- Depends on: ~~P0 (i128/decimal balances)~~ **SATISFIED** (shipped 2026-07-15: i128
  circuit/storage/serving + restart-replay); slice 4 WASM runtime (shipped); the
  effectful-worlds host machinery (slice C4, this RFC — see revised funding note)
- Blocks: —
- Revision note: v2 marks P0 as shipped, decouples C4 from the grant milestone
  (RFC-0006 v2 removed effectful worlds from the NLnet budget — it is expected to be
  built here first, under the no-double-funding rule), and adds the operator
  distribution channel (GraphOps) with the core/operator dividing line. All six
  slices, gates, and out-of-scope rules are otherwise unchanged — the technical
  design stands.

> Note (unchanged): this RFC is an implementation brief (build-order slices with
> gates) aimed at stablecoin/fintech operators, within every CLAUDE.md non-negotiable.

## Architecture rules — unchanged from v1

(1) Pure/effectful split is the compliance architecture; the deterministic core stays
replayable with effectful stages disabled. (2) Screening is pure, list-as-data —
content-addressed list snapshots, no screening APIs in the data path, ever.
(3) Batched Arrow boundary everywhere. (4) Capability grants are the audit artifact —
declared grants vs actual imports enforced at load. (5) Retraction-correct flags —
reorged flags retract through DBSP, sinks emit `flag_retracted`. (6) No scope creep:
no multi-hop taint, no DAG runner, no LLM in the data path.

## Prerequisite — SATISFIED

**P0 — i128/decimal balances: shipped 2026-07-15.** The circuit, deltas, and storage
accumulate in i128; balances serialize as decimal strings; restart-replay reconstructs
the view from durable facts (sealed segments fold in DuckDB as HUGEINT, hot tail
replays); golden tests cover amounts exceeding i64 (34 live WETH holders exceeded
i64::MAX under the old code — the exact disqualifying bug this prerequisite named,
found and fixed before any compliance work built on it). Threshold arithmetic in
slice C3 must reuse the same i128 path — carry that as a C3 gate item rather than a
prerequisite.

## NEW: Distribution — the operator channel

The pack's intended audience (stablecoin/fintech operators) rarely self-hosts
directly; they buy operated infrastructure. The GraphOps partnership (2026-07-16)
gives the pack a distribution channel: **operators run Nuthatch + a compliance pack
for their customers; Nuthatch stays the neutral engine.** The dividing line, matching
RFC-0005 §6:

- **In core (this RFC, unchanged):** the annotation substrate, pure screening against
  content-addressed list snapshots, threshold/velocity views, effectful worlds,
  webhook sinks with retractions, the signed pack manifest, and the audit commands.
- **Operator layer (explicitly not core):** tenant routing of alerts, per-customer
  gateways/auth/metering, list-update SLAs, report white-labeling, and any
  fiat-conversion or entity-resolution service. If a pilot asks for these in core,
  the answer is the pointer to this paragraph.
- **The manifest is the trust interface between the two.** `nuthatch pack verify`
  lets an operator's *customer* independently confirm which pack (component hashes,
  grants, list snapshots) produced their alerts — and `nuthatch audit replay` lets
  them reproduce the results on their own hardware. That combination — operated
  convenience with customer-verifiable outputs — is the pack's differentiator versus
  API-based screening vendors, and it only works because rules 1–4 keep the core
  deterministic. Say exactly this in the pack's README.

Sequencing note: the pilot's first target (Lodestar via GraphOps) does NOT involve
the compliance pack; the pack enters operator conversations after C2 (screening)
demos. Conversation #5 in RFC-0007 v2 (a fintech compliance operator) is the design
review for these slices — schedule it before C3 hardens the flag-view config shape.

## Build order — slices C1–C6 unchanged from v1

(C1 labels & annotation substrate + direct-exposure DBSP view; C2 pure sanctions
screening with `lists fetch` host-side and the replayable `screen` command; C3
threshold & velocity views — add gate: thresholds arithmetic proven on the shipped
i128 path with an overflow-adjacent golden test; C4 effectful worlds ported from
liminal-host, adapted to the batched Arrow boundary, declared-vs-actual import
enforcement; C5 alert-webhook effectful component with durable outbox, at-least-once,
retraction events, never-block-the-indexer; C6 signed `compliance-pack.toml`,
`pack verify`, `audit replay`/`audit report`, MCP tools `flags`/`exposure`/
`screen_status`, scaffolding updates.)

One C4 note revised: v1 cross-referenced "RFC-0006 M1" as the funding home for
effectful worlds. Per RFC-0006 v2's no-double-funding rule, effectful worlds are now
expected to be built HERE first (compliance/operator demand is the nearer driver);
the NLnet budget substituted GraphQL-compat in its place. If, at grant-submission
time, C4 has NOT started, it may move back into the grant budget — the rule is about
where it's funded, decided once, by the progress log.

## Testing & CI additions — unchanged from v1

(Golden + retraction-convergence for every annotation view; `audit replay`
determinism gate on a fixed fixture, bit-identical; RAM budget gate with the full
pack active.)

## Out of scope — unchanged from v1, one addition

v1 list stands (multi-hop taint, fuzzy entity matching, fiat conversion in core,
rules DSL, Kafka sink, hosted/telemetry components). Addition, from the operator
channel: **per-tenant anything** — tenancy is the operator's layer by the dividing
line above.

## Definition of done — unchanged from v1

`nuthatch init <token> && nuthatch lists fetch ofac-sdn && nuthatch dev` on live
mainnet USDC: screening + threshold flags + exposure views active, webhook alert
fires on a fixture-injected hit in test, `nuthatch audit replay` reproduces stored
annotations bit-identically, peak RAM within budget, all tests green, README status
table and progress log updated in the house style.
