# RFC-0007: Launch and validation

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: RFC-0005 (installable release); RFC-0002 (the demo nest)
- Blocks: nothing — this is the finish line of the current phase

## Abstract

Take Nuthatch public deliberately: a readiness gate (strangers complete the quickstart
unaided), a channel sequence that starts on home turf (Graph forum) before Show HN, and
— the part four research reports kept deferring to reality — the five structured
conversations with teams currently paying for or operating indexers. Defines success
and failure thresholds in advance so the outcome is a decision, not a vibe.

## Motivation

The research phase's honest residual was always: demand for the self-hosted middle is
evidenced but inferential, and only real users resolve it. Launch is therefore not a
marketing event; it is the experiment. Running it deliberately — with pre-registered
thresholds — protects against both premature abandonment and sunk-cost drift, the two
documented failure modes of solo-maintainer infra OSS.

## Phase 0 — Readiness gate (blocks everything below)

Three strangers (not friends-of-the-project; recruit one Rust dev, one dapp dev, one
Graph-ecosystem person) each attempt, unaided, screen-recorded or self-reported:

1. Install via the site's curl command on their own machine.
2. Quickstart: USDC nest to a live `/sql` query.
3. Horizon nest via `init --from`, one view query.

Gate: 3/3 complete steps 1–2 within 15 minutes; ≥2/3 complete step 3. Every stumble
becomes an issue; gate re-runs after fixes. Also before launch: `SECURITY.md`, issue
templates, Discussions enabled (chosen over Discord for a solo maintainer — async,
searchable, no presence obligation; revisit at >50 weekly actives), and the
scope/governance doc published (the "what we will not build" shield, adapted from
CLAUDE.md — linkable when launch-day feature requests arrive).

## Phase 1 — Home turf (week 1)

**The Graph forum + #indexers community.** Post framing: a decade-long ecosystem
member built a small tool; its flagship example indexes Horizon itself; here are
measured numbers and a parity check against a community subgraph. Explicitly not
framed as a Graph competitor — framed as local observability for the ecosystem
(which it genuinely is; the network-serving question was settled out of scope long
ago). Credit Paulie's subgraph prominently; ideally give him a heads-up first — a
first community nest author is worth more than a launch-day surprise.

Why home turf first: highest-trust audience, most-informed criticism, and any
embarrassing bug gets found by people who file good issues rather than dunk-tweet.

## Phase 2 — Show HN + Rust community (week 2–3, after week-1 fixes)

**Show HN** title (draft): `Show HN: Nuthatch – a self-hosted blockchain indexer in
one 49 MB Rust binary (37 MB RAM)` — concrete numbers, no adjectives. First comment
(prepared in advance, the custom): what it is in three sentences, architecture
paragraph (redb + content-addressed Parquet + DuckDB + DBSP IVM + WASIp2 components),
the measured-numbers table with methodology links, honest limits verbatim from the
README ("Ethereum + Arbitrum only, events only, GraphQL not yet shipped"), and why
AGPL. Post Tue–Thu, 14:00–16:00 UTC. The crypto-skepticism ritual will occur; the
answer is the sovereignty/local-first framing and refusing to argue token economics
(there is no token — the thread's best weapon).

**r/rust** (separate week): the engineering angle — DBSP retractions for reorgs,
batched Arrow over WIT, the DuckDB single-writer design. r/ethdev and lobste.rs
opportunistically. One channel per launch-day; the maintainer is one person and
launch-day responsiveness is the actual product demo.

## Phase 3 — The five conversations (parallel, weeks 1–4)

Structured, 30 minutes each, five profiles:

1. A team currently paying Goldsky/Envio Cloud (a real invoice).
2. A Ponder production user (the closest philosophical neighbor).
3. An Alchemy-sunset refugee (migrated under duress in Dec 2025).
4. A Graph indexer/ecosystem operator (Lodestar-adjacent contacts).
5. A team building agents that consume chain data (the MCP surface's audience).

Script: 10 min demo (quickstart live, then Horizon nest), then three questions, asked
exactly: *"What would have to be true for you to run this for something real?"* /
*"What's missing that you'd consider disqualifying?"* / *"What here don't you
believe?"* Record answers verbatim in `docs/validation/` (anonymized). No selling —
the third question exists to surface the doubt people politely swallow.

## Pre-registered thresholds (written before launch, judged at day 30)

Success signals (any two → continue investing at current pace):
- ≥3/5 conversations yield a concrete adoption intent or a named, addressable blocker.
- ≥5 quickstart-completion reports from strangers (issues, Discussions, or posts —
  no telemetry, so evidence is voluntary; count conservatively).
- ≥1 external nest published or ≥1 non-trivial external PR/issue with reproduction.
- ≥300 GitHub stars (the Ponder-calibrated niche signal, not a vanity target).

Failure signals (both → downshift to nights-and-weekends maintenance; both plus dead
conversations → archive gracefully with a written post-mortem, per report-4's
threshold discipline):
- <2 conversation successes AND no organic quickstart evidence by day 30.
- Zero engagement from the Graph-forum home-turf post (the most favorable audience;
  silence there is the strongest negative signal available).

Explicitly not failure: HN indifference (timing lottery), crypto-cynic threads,
feature-request floods (that's demand, filtered through the scope doc).

## Post-launch operations (day 0–30)

- Launch-day: respond to everything within hours; fix only breakage, no features.
- Triage labels from day one: `bug`, `docs-gap` (a stranger's confusion is a docs bug),
  `out-of-scope` (close kindly, link the scope doc), `nest-request` (redirect to
  "nests are repos — here's the template").
- Week-2 progress-log post: what launch surfaced, what changed — the honesty format,
  continued in public.
- Metrics reviewed weekly, thresholds judged once at day 30 — not daily (the
  dashboard-refresh anxiety loop is a solo-maintainer burnout accelerant; the
  research on this was clear).

## Risks

- **Launch lands during a news cycle / gets buried**: the thresholds deliberately
  don't depend on HN; home turf + conversations carry the validation weight.
- **A security issue in week one**: SECURITY.md + the capability-sandbox design limits
  blast radius; the WASM host boundary audit (RFC-0006) is the structural answer —
  until then, the threat model doc states current assumptions plainly.
- **Success**: the genuinely dangerous outcome for one maintainer. The scope doc, the
  Discussions-not-Discord choice, and the RFC discipline are the pre-committed
  defenses. If adoption outruns capacity, the move is raising the contribution bar
  documentation, not the feature pace.

## Open questions

1. Offer the Horizon nest as a hosted read-only demo instance (nuthatch-indexer.com/
   demo) for launch day? Attractive (zero-install taste) but contradicts nothing —
   it's our own box serving our own nest. Leaning yes if it costs <1 day.
2. A launch blog post vs letting the README carry it: leaning README-only + forum
   posts — the site's /manifesto already exists and a launch post would mostly
   duplicate it.
