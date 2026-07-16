# Validation log

Structured adoption conversations for nuthatch, per [RFC-0007](../rfcs/0007-launch-and-validation.md)
Phase 3. The point is not to collect compliments — it's to find out, from people who actually run
this kind of infrastructure, whether nuthatch solves a problem they'd otherwise pay to solve. The
thresholds are pre-registered in the RFC and judged once, at day 30, so a good conversation can't be
retrofitted into a success it wasn't.

## Method

Each conversation follows the same script:

1. **Demo** — a live nuthatch instance: `init → dev → /sql` on a real contract, and (for the
   compliance profile) `nuthatch audit replay`. No slides.
2. **Three questions, asked verbatim, in this order:**
   1. *"If this didn't exist, what would you use instead — and what does that cost you today
      (money, ops time, or a dependency you'd rather not have)?"*
   2. *"What's the one thing that would stop you running this in production next month?"*
   3. *"Who else should I show this to?"*
3. **Record verbatim, anonymised.** Answers go in the per-conversation file as written, attributed
   only by profile unless the person agrees to be named. Outcome is scored against the RFC's
   pre-registered bar (concrete adoption intent = pass), not against how nice the chat felt.

## Roster (5 profiles)

| # | Profile | Status | Outcome |
|---|---------|--------|---------|
| 1 | Infrastructure operator | ✅ Done (2026-07-16) | **Exceeded threshold** — partnership + revshare proposal, first target agreed |
| 2 | A team paying Goldsky / Envio Cloud (a real invoice) | ⬜ Pending | — |
| 3 | A Ponder production user (closest philosophical neighbour) | ⬜ Pending | — |
| 4 | A team building agents that consume chain data (the MCP audience) | ⬜ Pending | — |
| 5 | A stablecoin / fintech compliance operator (the RFC-0008 audience) | ⬜ Pending | — |

## Pre-registered thresholds (from RFC-0007, judged at day 30)

Recorded here so they can't drift. **Success signals** (any two → continue at pace): ≥1 conversation
ending in concrete adoption intent; ≥1 unsolicited "when can I use this"; a self-hoster running it a
week unaided; a public mention we didn't write; **or** the operator pilot serving a Lodestar panel for
14 consecutive days / nuthatch appearing in the operator's public catalogue. Conversation #1 already
banks the first of these — the remaining four conversations still happen, because one operator is a
sample size of one.

---

## Conversation #1 — infrastructure operator (2026-07-16)

- **Profile:** infrastructure operator running physical fleet capacity for The Graph ecosystem.
- **Format:** live demo of the embedded pipeline + the Horizon nest.
- **Pre-registered bar:** "concrete adoption intent."
- **Outcome: EXCEEDED.** Unprompted, the operator proposed hosting nuthatch as an offering on their
  data-service platform with revenue-share to fund core development, and agreed a first target (the
  Lodestar panel migration) as the pilot. This is recorded as sustainability Leg 2 in
  [RFC-0006 v2](../rfcs/0006-grant-funding.md) and as Phase 1.5 (the operator pilot) in RFC-0007 v2.
- **Neutrality note:** the partnership grants no exclusivity, private fork, partner-only core
  features, or roadmap veto — see [GOVERNANCE.md](../../GOVERNANCE.md#neutrality-the-guarantee-you-can-depend-on).

### Verbatim answers

_To be transcribed by the maintainer from notes — recorded here anonymised once written up._

- **Q1 (what would you use instead / what does it cost):** _(pending transcription)_
- **Q2 (what would stop you running it in production):** _(pending transcription — the DoS concern
  that shaped the demo-instance decision in RFC-0007 Q1 came up here; capture it precisely)_
- **Q3 (who else):** _(pending transcription)_

---

## Conversations #2–#5

Template — copy this block per conversation as it happens. Do not pre-fill; an empty section is an
honest "not yet," and the day-30 judgement depends on that honesty.

```
## Conversation #N — <profile> (YYYY-MM-DD)

- Profile:
- Format:
- Pre-registered bar: concrete adoption intent
- Outcome: <pass / partial / no — scored against the bar, not the vibe>

### Verbatim answers
- Q1 (what would you use instead / what does it cost):
- Q2 (what would stop you running it in production):
- Q3 (who else):

### Follow-up / notes
```
