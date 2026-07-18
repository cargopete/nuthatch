# RFC-0007: Launch and validation

- Status: Accepted (2026-07-18) — launch kit shipped; launch/validation process ongoing (RFC-0011 pilot is partial validation)
- Author: Pete (cargopete)
- Date: 2026-07-16 (v1: 2026-07-14)
- Depends on: RFC-0005 (installable release; the rc doubles as the GraphOps pilot
  artifact), RFC-0002 (demo nest)
- Blocks: nothing — the finish line of the current phase
- Revision note: v2 records that conversation #1 happened and exceeded its threshold
  (GraphOps: an infrastructure operator proposed a hosted offering with revshare,
  unprompted), adds the operator channel as a launch phase and a success signal,
  resolves the demo-instance open question, and syncs numbers to shipped code.

## Abstract

Unchanged in spirit: take Nuthatch public deliberately — readiness gate, home turf
before Show HN, structured conversations, pre-registered thresholds. Changed in fact:
validation has already begun and scored. The GraphOps conversation (2026-07-16) was
the operator-profile conversation, and its outcome (partnership + revshare proposal)
exceeds the pre-registered bar of "concrete adoption intent." v2 folds the operator
channel into the launch plan without letting it substitute for the remaining
validation: one operator's enthusiasm is a wonderful signal and a sample size of one.

## Phase 0 — Readiness gate (blocks everything below) — unchanged from v1

Three strangers, unaided: install via curl, USDC quickstart to a live `/sql` query,
Horizon nest via `init --from`. Gate: 3/3 on steps 1–2 within 15 minutes, ≥2/3 on
step 3. Plus SECURITY.md, issue templates, Discussions, the scope/governance doc.
One addition: the governance doc now also carries the partnership-neutrality
paragraph (RFC-0006 v2 Rules 3–4) so launch-day "is this a GraphOps product?"
questions have a linkable answer.

## Phase 1 — Home turf (week 1) — unchanged from v1, one addition

Graph forum + #indexers, framed as local observability for the ecosystem, Paulie
credited and forewarned. Addition: **partnership disclosure timing is GraphOps's
call.** If their platform announcement precedes launch, the home-turf post mentions
the hosted option in one sentence; if not, the post stands alone and the partnership
is announced when they announce. Do not front-run a partner's launch for our
narrative.

## Phase 1.5 — NEW: the operator pilot (parallel, not a launch gate)

The pilot agreed in principle: GraphOps runs Nuthatch (v0.1.0-rc.1 per RFC-0005) on
their platform; Lodestar is the first tenant, migrating panels per the
graph-network-nest plan (RFC to be numbered — the Lodestar-migration design exists
and should be committed as its own RFC now that 0008 is the compliance pack).
Boundaries, restated from RFC-0005 §6: they own gateway/auth/metering; we own the
binary, guards, and /metrics. Pilot success = Lodestar's first migrated panel served
via GraphOps-hosted Nuthatch for 14 consecutive days. The pilot is deliberately NOT a
launch precondition — public launch proceeds on our criteria; the pilot proceeds on
theirs; neither waits for the other.

## Phase 2 — Show HN + Rust community (week 2–3) — updated numbers

Show HN title (draft, refresh at post time from the README):
`Show HN: Nuthatch – a self-hosted blockchain indexer in one Rust binary (58 MB RAM
for a 3-contract nest)` — or the single-contract 37 MB figure if the title reads
better; both are measured. First comment gains the RFC-0004 progression (~289 →
~5,837 ev/s on public RPC, ~20×, path-equivalence proven, methodology in-repo) and
keeps the honest limits verbatim from the README (now: "Ethereum + Arbitrum + Base,
events only, RPC polling — ExEx designed and stubbed, GraphQL not yet").
r/rust angle unchanged (DBSP retractions, batched Arrow over WIT, DuckDB
single-writer design — add the seal-direct path-equivalence test as the hook; r/rust
loves a determinism proof). One channel per launch-day, unchanged.

## Phase 3 — The structured conversations (weeks 1–4) — revised roster

Conversation #1 is DONE and recorded: profile "infrastructure operator," outcome
"partnership + revshare proposal, first target agreed" — logged in
`docs/validation/` like the rest. Four remain, one profile revised:

1. ~~Operator~~ **DONE — GraphOps, exceeded threshold.**
2. A team currently paying Goldsky/Envio Cloud (a real invoice).
3. A Ponder production user (the closest philosophical neighbor).
4. A team building agents that consume chain data (the MCP surface's audience).
5. REVISED: a stablecoin/fintech compliance operator (the RFC-0008 compliance pack's
   intended audience — replaces the Alchemy-refugee profile, which the operator
   conversation partially covered; Fathom-adjacent contacts make this reachable, and
   it validates the newest RFC rather than re-validating the oldest thesis).

Script unchanged (demo, then the three exact questions; verbatim answers, anonymized).
One addition to the script for #5: show `nuthatch audit replay` — the compliance
pack's "prove it" command is the demo for that audience even in its design-doc state.

## Pre-registered thresholds (judged once at day 30) — v1 thresholds stand, one added

Success signals (any two → continue at pace) — unchanged four, plus:
- NEW: the operator pilot serves a Lodestar panel for 14 consecutive days, or
  Nuthatch appears in GraphOps's public data-service catalog — either counts as one
  success signal (not two).

Failure signals — unchanged, with one honest note: the GraphOps signal already banked
means the "archive gracefully" branch now requires not just dead launch metrics but
also the pilot failing — the bar for continuing was met early; the bar for *pace* is
what day 30 judges.

Explicitly not failure — unchanged (HN indifference, cynic threads, feature floods).

## Post-launch operations (day 0–30) — unchanged from v1

(Respond-fast/fix-only launch day; triage labels; week-2 progress-log post; weekly
metrics, day-30 judgment, no dashboard-anxiety loop.)

## Risks

v1 risks stand (news-cycle burial; week-one security issue; success-as-danger), plus:
- **Partnership timeline entanglement**: mitigated structurally in Phase 1.5 — the
  pilot and the launch are decoupled on purpose; neither is the other's gate.
- **Narrative capture** ("the GraphOps indexer"): the governance-doc neutrality
  paragraph, linked from launch posts, is the standing answer; the license is the
  structural one.
- **Validation complacency**: one great operator conversation is not five
  conversations. The roster stays at five; the remaining four happen.

## Open questions

1. v1 Q1 (hosted read-only demo) — RESOLVED: yes, and the DoS concern from the
   GraphOps conversation shapes it. Preferred: GraphOps hosts the demo behind their
   gateway once the pilot runs (zero marginal work, showcases the partnership);
   fallback: self-hosted on the Hetzner box with RFC-0005 §6 query guards at strict
   settings + nginx rate limiting. Either way the demo instance runs the Horizon
   nest, read-only, guards documented on the page.
2. v1 Q2 (launch blog post vs README) — unchanged, leaning README + forum posts.
3. NEW: should launch wait for Base support (RFC-0005 criteria #4)? RESOLVED: Base
   shipped (v0.1.0) — "Ethereum, Arbitrum, Base" reads materially better than two
   chains, and it was an afternoon. No longer a gate.
