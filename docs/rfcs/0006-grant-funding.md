# RFC-0006: Sustainability — grants (NLnet/NGI, EF ESP) alongside operator revshare

- Status: Accepted (2026-07-18) — grant drafts + governance shipped; submission/decision process ongoing
- Author: Pete (cargopete)
- Date: 2026-07-16 (v1: 2026-07-14)
- Depends on: RFC-0002 (working demo — satisfied), RFC-0005 (roadmap milestones)
- Blocks: sustained development time
- Revision note: v2 retitles the RFC. Grants are no longer "the project's revenue
  model" (v1) but one of two sustainability legs — the second is the GraphOps
  operator-revshare partnership (conversation of 2026-07-16). v2 adds disclosure
  rules, a no-double-funding rule, milestone substitution, and a neutrality clause.

## Abstract

Nuthatch remains a free, AGPL public good with no direct monetization. Sustainability
now has two independent legs: (1) grants funding the commons-flavored roadmap
(NLnet/NGI, EF ESP — this RFC's original subject), and (2) revenue share from
operators who run Nuthatch as a hosted service — concretely, GraphOps, which intends
to offer Nuthatch on its data-service platform and share revenue to fund core
development. The legs are deliberately independent: either alone sustains
nights-and-weekends+; together they approach funded full-time work. This RFC keeps the
grant applications on track, adds the rules that keep the two legs honest with each
other, and codifies that no partnership changes the project's neutrality.

## Motivation

v1's premise ("the self-hosted indexer niche is real but non-monetizable; grants are
the sustainability model") was half-superseded within 48 hours of the demo existing:
an infrastructure operator with 8,000 physical cores proposed hosting Nuthatch with
revshare, unprompted. The research-phase pattern this matches is Plausible/Caddy —
sovereignty tools funded by *someone else's* managed offering — except with the
operator as a partner rather than the maintainer running the cloud. Grants remain
worth pursuing: revshare is unproven revenue until the platform launches and bills,
grants fund exactly the commons-flavored milestones operators won't prioritize, and
funder diversity is itself a bus-factor mitigation.

## The two legs and the rules between them

- **Leg 1 — grants** fund commons infrastructure: verifiability, the semantic layer,
  IVM generalization, security audit, docs. Public-benefit framing, milestone-based.
- **Leg 2 — operator revshare (GraphOps)** funds availability of the maintainer and
  operator-adjacent work (release engineering, guards, fleet ergonomics — RFC-0005
  §6), proportional to hosted-service revenue. Terms TBD on the upcoming call; the
  structural asks from our side: revshare on Nuthatch-derived revenue, roadmap
  *input* not veto, no exclusivity, no relicensing, and public disclosure of the
  relationship's existence (amounts private is fine).

Rules:
1. **No double funding.** A milestone funded by a grant is not simultaneously billed
   to or prioritized under revshare, and vice versa. Concretely: v1's M1 (effectful
   transform worlds) is now likely to be built earlier under RFC-0008 (compliance
   pack) demand — if it ships before grant submission, it is REMOVED from the NLnet
   budget and substituted (see Budget).
2. **Milestones must be un-started at submission.** The progress log is public;
   funders can and will check. Anything shipped moves from "budget" to "evidence."
3. **No exclusivity, ever.** Any operator may host Nuthatch under AGPL; GraphOps's
   edge is partnership, priority support of *their* integration questions, and being
   first — not a gate on others. This sentence is quotable to any future partner.
4. **Disclosure.** Both grant applications disclose the operator partnership in one
   plain sentence. NLnet and ESP both routinely fund projects with commercial
   ecosystems; concealment, not coexistence, is what damages applications.

## Funder 1: NLnet / NGI

### Which call — unchanged from v1
(Verify the open call at submission; NGI Assure lineage first; application is
call-agnostic; if nothing opens before September, file next cycle.)

### Framing — unchanged from v1, plus one sentence
The data-sovereignty pitch stands verbatim. Add to the "what exists already" section:
measured numbers now include the RFC-0004 progression (~289 → ~5,837 ev/s, ~20×
stacked, methodology in-repo) and the operator partnership as adoption evidence,
disclosed per Rule 4: "An independent infrastructure operator (GraphOps) is preparing
a hosted offering of Nuthatch under AGPL and shares revenue with the maintainer; this
application funds the commons-facing roadmap that hosting revenue would not
prioritize."

### Budget (€38,400 requested; NLnet range €5–50K) — revised per Rule 1

| Milestone | Deliverable | Est. effort | € |
|---|---|---|---|
| M1 | Governed semantic layer + reliable NL/agent queries over it | 8 wk | 12,800 |
| M2 | IVM generalization: declarative views compiled to DBSP, hot/cold view freshness | 6 wk | 9,600 |
| M3 | GraphQL compatibility layer + subgraph-migration path (the open-standards continuity story) | 6 wk | 9,600 |
| M4 | Security review remediation (WASM host boundary, via NLnet's ROS) + threat model + hardening release + docs | 4 wk | 6,400 |

Changes from v1: effectful-worlds milestone removed (Rule 1 — expected to ship under
RFC-0008 first); GraphQL-compat substituted (genuinely commons-flavored: it lets
existing subgraph consumers move to self-hosted infrastructure without rewrites).
Rate unchanged (€40/h). ROS audit still requested explicitly. **Re-verify against the
progress log the week of submission and re-apply Rules 1–2** — at the current shipping
pace this table has a short shelf life, which is a good problem.

### Mechanics — unchanged from v1.

## Funder 2: Ethereum Foundation ESP

### Framing — unchanged from v1, strengthened by two facts
The TrueBlocks-lane pitch stands. Add: (a) the operator partnership as ecosystem
evidence — an Ethereum-infrastructure operator adopting the tool for its platform is
the adoption signal ESP reviewers weight most; disclosed per Rule 4; (b) RFC-0003's
blockers are cleared (toolchain 1.95 needle threaded, reth v2.4 resolves alongside the
core), so the ExEx milestone is de-risked engineering, not speculation.

### Ask — unchanged from v1 ($50–90K / 12 months; milestone table reuses NLnet's
revised set plus ExEx + OP-stack multi-chain; co-funding disclosed with distinct
milestone ownership).

### Mechanics — unchanged from v1.

## Secondary / later channels — unchanged from v1
(The Graph ecosystem grants: still do not apply this cycle — and note the optics have
improved, not worsened: GraphOps is a core Graph-ecosystem operator, so Nuthatch is
demonstrably complementary infrastructure inside the ecosystem. Revisit if community
members propose it. Gitcoin/RetroPGF after launch. GitHub Sponsors: enable now.)

## What we will not do for funding OR partnership (scope integrity, extended)

v1 list stands: no token, no decentralized-network milestone, no
enterprise-feature commitments in core, no telemetry, no relicensing. Extended for
Leg 2: no exclusivity, no private forks, no partner-only features in the AGPL core,
no roadmap veto, and the RFC-0005 §6 dividing line (auth/metering/tenancy = operator
layer) is contractual. If any funder or partner requires items on this list, decline
that term.

## Implementation plan

1. This week: the GraphOps call — bring the two-leg structure and Rules 1–4 as the
   proposed shape; agree the pilot (RFC-0005 rc + Lodestar as first tenant) and the
   revshare mechanics in principle. Enable GitHub Sponsors (still not done — zero
   cost).
2. Grant drafts in `docs/grants/` (public, PR-reviewed) — unchanged; demo video after
   the Horizon parity fixtures land (now the RFC-0005 criteria-#2 item).
3. Submit NLnet when a call opens; ESP inquiry same week — unchanged, urgency
   honestly softened: with Leg 2 in motion, a missed cycle is a delay, not a threat.
4. Track responses, 48h answers — unchanged.
5. Failure branch revised: if both grants decline AND revshare hasn't materialized in
   two quarters, v1's nights-and-weekends threshold applies. If either leg lands,
   continue at pace.

## Acceptance

v1 items (applications submitted with video + benchmarks + parity evidence; milestones
map 1:1 to public RFCs; Sponsors enabled; drafts public) plus: Rules 1–4 reflected in
the submitted texts; the GraphOps terms, once agreed, summarized in one public
paragraph (existence + shape, not amounts) in the repo's governance/scope doc.

## Risks

v1 risks stand (calendar slip; Graph-competition optics — now mitigated per above;
milestone overcommitment — now governed by Rules 1–2), plus:
- **Revshare never materializes** (platform delays, priorities shift): Leg 1 exists
  precisely for this; nothing in the grant plan depends on Leg 2.
- **Perceived capture** ("GraphOps's indexer"): Rules 3–4 and the public neutrality
  sentence are the answer; the AGPL license makes capture structurally impossible
  anyway — worth saying in the FAQ once the partnership is public.

## Open questions

1. v1 Q1 (fiscal routing) now includes: revshare to Nixum Ltd — confirm VAT treatment
   (B2B reverse charge if GraphOps entity is EU, else out of scope) with the
   accountant before the first invoice.
2. v1 Q2 stands (ROS audit gating the 1.0 designation — still leaning yes).
3. Should the governance doc name a successor/escrow arrangement for the minisign and
   signing keys now that an operator depends on releases? Small, worth doing at
   v0.1.0.
