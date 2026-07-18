# RFC-0015: The delightful core — CLI/UX for the solo dev

- Status: Draft (v1) — the 0.5 theme
- Author: Pete (cargopete)
- Date: 2026-07-18
- Depends on: nothing new — this is polish over the 0.1–0.4 capabilities, which are all shipped.
- Blocks: adoption. This is the difference between "nuthatch can index my contract" (true today) and
  "nuthatch is nice enough that I'll pick it over the thing I already know" (the actual bar).
- Nature: product/direction RFC. Records the north star and the 0.5 scope so the enterprise breadth
  doesn't quietly become the front door.

## Abstract

nuthatch has one thing it does better than anything else, and 0.5 is about doubling down on it:

> **Turn any contract into a local SQL database — one command, on a cheap box, no bill, no third party.**

Everything else — the compliance pack, multi-nest roosts, factory discovery, firehose-class extraction —
is genuinely valuable and serves real *adjacent* users (institutions, hosting providers, Substreams
competitors). But it is **not the wedge**, and if it becomes the front door we lose the person the wedge
is for: the solo dev or small team who has a contract and just wants its data, cheaply, on their own box.

0.5 adds **no new data capabilities**. It makes the core path — *contract address → queryable SQL* —
irresistible.

## Motivation

The core user's blocker was never capability. By 0.4 nuthatch can index one or many contracts, seal past
finality, serve hot∪cold SQL, discover factory children, run compliance and IVM views, and host multiple
nests in one process — all from a single hardened binary. The blocker is **friction and first-impression**.

The competitive wedge is a four-way combination no one else hits: **zero authoring** (init from an
address, no schema/mappings to write), **zero infra** (one static binary, embedded, no Postgres/Docker),
**it's just SQL** (not a GraphQL schema you model), and **it's yours and it's tiny**. The Graph makes you
author a subgraph; Goldsky bills you monthly for their infra; Ponder needs Node + Postgres + handlers;
Subsquid is archive-heavy GraphQL. nuthatch's job in 0.5 is to make the happy path deliver on that
combination so completely that the choice is obvious.

The "one thing it does best" is **time-to-first-query and the joy of querying** — the magic of *"I had an
address ninety seconds ago, now I'm running `SELECT` over my contract's data on my own machine."*
Everything that protects or amplifies that moment is in scope; everything else is not.

## Non-goals

- **New data capabilities.** No new-chains-before-EVM-is-airtight, no firehose (RFC-0014), no scaled
  mode. 0.5 is pure DX/UX.
- **Removing or hiding the enterprise breadth.** The compliance pack, roost, blobs, MCP stay — they land
  the big partners. They just stop being the front door: discoverable in the docs, not in the first ten
  seconds. The README leads with the core.
- **A hosted / UX-as-a-service anything.** Still a binary you run.

## The work, in order of leverage

1. **`nuthatch sql` — the terminal-native query surface.** The single biggest gap: the user's *goal* is
   querying, but that meant HTTP `/sql` + curl. A one-shot `nuthatch sql "SELECT …"` (aligned table +
   `--json`, redb-lock-aware with an HTTP fallback) **shipped in 0.4** as the down-payment. 0.5 completes
   it: a **REPL** (readline, history, `.tables`, `.schema <table>`, `.exit`), pretty/paged output,
   column order matching the `SELECT`, and helpful errors. This is the delight.
2. **Magical `init`.** `nuthatch init 0xAddr` should often *just work*: probe/detect the chain (try a
   few) so `--chain` becomes optional; print what it found ("12 events across 3 contracts — indexing
   Transfer, Approval, Swap…"); default away every flag we can. Fewer decisions, more magic.
3. **Live backfill feedback.** A clean progress line during `dev` — events/sec, blocks done/total, ETA,
   and a crisp "caught up to tip" — instead of log spam. For a new tool, *feeling* fast and trustworthy
   is half the adoption battle.
4. **`nuthatch add 0xAnother`.** Grow a nest with another contract without re-`init` — the natural
   "one or many contracts" flow.
5. **The AI hook as one step.** `nuthatch mcp` → "point Claude (or any MCP client) at your indexer and
   ask your contract's data in plain English." The AI-native differentiator nobody else has; make wiring
   it a single, documented command with a copy-paste client snippet.
6. **A dead-simple prod story.** Small + easy to *run*, not just try: a production serve profile and a
   copy-paste systemd / docker one-liner, so "I tried it locally" → "it's running on my VPS" is trivial.

## Sequencing

The `nuthatch sql` one-shot is done (0.4). Then, roughly in leverage order: the REPL (1), magical `init`
(2), live feedback (3), `add` (4), the MCP one-liner (5), the prod profile (6). Each is independently
shippable and independently makes the happy path nicer; none depends on the others.

## Acceptance

The bar is subjective but real: **can a stranger go from a contract address to querying its data in a
terminal, delighted, in under two minutes, with the docs open only for the address?** When the answer is
an obvious yes, 0.5 is done. Instrument time-to-first-query in the demo; keep a 60-second asciinema as
the proof.

## Open questions

1. REPL library (rustyline vs a hand-rolled readline) — pick the smallest that gives history + editing.
2. Chain-detection heuristic for `init` — probe order, and how to fail gracefully to an explicit
   `--chain`.
3. How much of the prod story is docs vs a `nuthatch serve` subcommand.
