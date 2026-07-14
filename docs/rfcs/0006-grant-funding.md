# RFC-0006: Grant funding — NLnet/NGI and Ethereum Foundation ESP

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: RFC-0002 (working demo), RFC-0003–0005 (fundable roadmap milestones)
- Blocks: sustained development time (this is the project's revenue model)

## Abstract

Nuthatch is a public good with no monetization plan by design; grants are the
sustainability model. This RFC specifies the two primary applications (NLnet/NGI and
Ethereum Foundation ESP), the framing that fits each funder, a milestone-based budget
derived from RFCs 0001–0005 and the deferred roadmap, and the supporting materials to
prepare. It also codifies what we will not do for funding (scope integrity).

## Motivation

Reports 3–4 of the research phase established: the self-hosted indexer niche is real
but non-monetizable; the sustainable model is grants + sponsorship (TrueBlocks
precedent: EF grants in 2018/2022/2024, the 2018 infrastructure grant reported at
~$120K; NLnet awards €5–50K per project with a famously light application). The demo
now exists with measured numbers — the strongest position a grant applicant can be in.
The constraint is calendar: grant cycles are slow, so applications go out in parallel
with RFCs 0003–0005, not after.

## Funder 1: NLnet / NGI

### Which call (verify at submission — programs shifted in 2026)

Action item zero: check nlnet.nl/propose for the currently-open call the week of
submission. Research flagged that NGI Zero Commons Fund's final call closed June 1 2026
and the August 1 deadline belongs to unrelated programs (Taler/Fediversity). Candidate
homes, in order: **NGI Assure lineage** (explicitly lists distributed ledgers and
verifiable/trustworthy infrastructure as in-scope building blocks), any successor
Commons call, or NLnet's general themed calls. If nothing suitable is open before
September, submit to the next opening cycle — the application below is call-agnostic.

### Framing (this is the part that wins or loses it)

Lead with **data sovereignty and verifiable infrastructure**, not blockchain:

> Nuthatch lets anyone operate their own index of public-ledger data on a €5 computer —
> replacing trusted third-party data APIs with a small, auditable, deterministic tool.
> Every derived result is reproducible: transformations run in capability-isolated
> WebAssembly components whose purity is verifiable from their imports alone, and all
> historical data is content-addressed. No telemetry, no accounts, no gated APIs.

NLnet's reviewers care about: open standards (WASI component model, WIT, MCP, Arrow/
Parquet — genuinely standards-forward, say so), user autonomy, small auditable
software, and EU dimension (Bulgarian company, EU maintainer). The AI angle is framed
as *local-first agent access to one's own data* (offline MCP, Ollama), never as
AI hype.

### Budget (€38,400 requested; NLnet range €5–50K)

Milestone-based, mapped to public RFCs (reviewers can see the plan is real):

| Milestone | Deliverable | Est. effort | € |
|---|---|---|---|
| M1 | Effectful transform worlds (capability-granted components, annotations-only) + signed pipeline manifests | 6 wk | 9,600 |
| M2 | Governed semantic layer + reliable NL/agent queries over it | 8 wk | 12,800 |
| M3 | IVM generalization: declarative views compiled to DBSP, restart replay, hot/cold view freshness | 6 wk | 9,600 |
| M4 | Security review remediation + hardening release + docs (a11y of docs, threat model) | 4 wk | 6,400 |

Rate: €40/h × 40h/wk (NLnet-typical modest rates read well; do not pad). NLnet also
funds third-party security audits via their partner (Radically Open Security) — request
it explicitly for the WASM host boundary; it costs the fund, not the milestone budget.

### Mechanics

1–2 page application: abstract (100 words), the problem (trusted third-party data
infrastructure), the solution + what exists already (link repo, measured numbers,
progress log — the honesty format is itself evidence), budget table, comparison with
prior art (Graph/Goldsky/Ponder — reuse the site's honest table), and "how does this
benefit the commons" (AGPL, nests as shareable public definitions, indexes The Graph's
own public network as the flagship example).

## Funder 2: Ethereum Foundation ESP

### Framing

For ESP, blockchain is the point — frame as **public-goods Ethereum data
infrastructure**: the TrueBlocks lane (local-first indexing as a public good; direct
precedent, cite it), plus what's new since TrueBlocks: reth ExEx integration
(strengthens the reth ecosystem — EF-adjacent), deterministic re-execution as
practical verifiability, and the L2 story (Horizon nest on Arbitrum). Emphasize: no
token, no company capture, AGPL, one maintainer with a decade of ecosystem
contributions (Matchstick, Graphcast — name them; ESP reviewers know them).

Ask: project grant, $50–90K / 12 months, milestone structure reusing the NLnet table
plus RFC-0003 (ExEx, already self-funded — shows momentum) and multi-chain (OP-stack
ExEx). Note in the application that NLnet has been approached for the
sovereignty-flavored milestones; co-funding with distinct milestone ownership is
normal and reviewers prefer it disclosed.

### Mechanics

ESP inquiry form first (short), full proposal on invitation. Materials: the repo, the
benchmark page (RFC-0004), the parity check vs the community subgraph (external
correctness evidence — rare in grant applications, lead with it), and a 10-minute
demo video (record the quickstart + Horizon nest, unedited single take — the
two-minute claim, proven on camera).

## Secondary / later channels (tracked, not pursued now)

- **The Graph ecosystem grants**: genuinely awkward (Nuthatch competes with the
  network's serving layer) but not absurd (the flagship nest indexes Horizon itself;
  Graph tooling grants funded Matchstick). Decision: do not apply in this cycle;
  revisit if community members propose it — better initiated by them than by us.
- **Gitcoin/Octant/RetroPGF rounds**: enable once launched (RFC-0007); retroactive
  funding rewards existing impact, so it sequences after adoption.
- **GitHub Sponsors**: enable now (zero cost, catches goodwill from launch).

## What we will not do for funding (scope integrity)

No token, no "decentralized network" milestone, no enterprise-feature commitments, no
telemetry-for-metrics, no relicensing. If a funder requires any of these, decline.
CLAUDE.md's out-of-scope list is contractual as far as grant milestones are concerned.

## Implementation plan

1. This week: verify open NLnet call; enable GitHub Sponsors; start the demo video
   after RFC-0002 parity passes.
2. Draft both applications in the repo (`docs/grants/`) — public drafts, consistent
   with the transparency posture; PR-review them like code.
3. Submit NLnet as soon as an appropriate call is open; submit ESP inquiry the same
   week (they are independent).
4. Track responses; NLnet typically responds in ~2 months with clarifying questions —
   answer within 48h (responsiveness is scored, informally).
5. If both decline: continue nights-and-weekends per the report-4 threshold ("do not
   attempt to monetize"), reapply next cycle with more adoption evidence.

## Acceptance

- Both applications submitted with the demo video, benchmark page, and parity evidence
  attached.
- Budget milestones map 1:1 to public RFCs/issues (a reviewer can audit the plan).
- Sponsors enabled; grant drafts public in-repo.

## Risks

- **Calendar slip on calls**: mitigated by call-agnostic drafts ready to file.
- **Perceived competition with The Graph** in ESP review: preempt in the application —
  Nuthatch indexes Ethereum and L2 data locally; it complements network-scale serving
  rather than replacing it, and the flagship nest serves The Graph's own ecosystem
  observability.
- **Milestone overcommitment**: every milestone is an existing RFC or a deferred item
  from the progress log — nothing invented for the application. Keep it that way.

## Open questions

1. Fiscal sponsorship vs direct: NLnet pays individuals/companies directly (fine); ESP
   also fine with an EU Ltd. No action unless a funder requests otherwise.
2. Should the security audit (NLnet's ROS) gate the 1.0 designation? Leaning yes —
   "audited WASM host boundary" is a 1.0-worthy claim.
