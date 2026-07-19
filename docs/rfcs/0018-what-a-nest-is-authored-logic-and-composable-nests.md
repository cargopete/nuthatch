# RFC-0018: What a nest is — first-class authored logic (SQL views) and composable, parameterized nests (Starlark)

- Status: Draft (v1)
- Author: Pete (cargopete)
- Date: 2026-07-19
- Depends on: RFC-0001 (generalized decode + the nest as the unit), RFC-0012 (content-addressed
  packaging — the nest as a reproducible blob), RFC-0013 §3 (SQL-over-the-tip: the DuckDB hot∪cold
  union these views ride on), RFC-0016 (the semantic layer these views live beside, and the
  errors-as-prompts machinery that validates them), RFC-0017 (the builder skill that must teach
  both new surfaces without hallucinating).
- Blocks: the catalogue thesis. `nests`, `horizon-nest`, and `graph-network-nest` today are
  *forks* of one another ("seeded from horizon-nest"), not instances of a reusable unit — and a
  freshly-`init`-ed nest is *inert*: config + ABIs, no visible authored thought. Both are symptoms
  of the same missing idea.
- Nature: design RFC. This one is a *definition* first and a feature list second — it states what a
  nest **is** before it proposes what to add, because the next few features will muddy that
  question if it isn't written down. Two of the three slices are "finish and promote what already
  half-exists"; only Starlark is genuinely new code.

## Abstract

Answer the question the project has never written down: **what is a nest?** Not "config plus ABIs"
(what it happens to be) but what it *should be* — and then close the two gaps that answer exposes.

The thesis: **a nest is a content-addressed, reproducible *specification* of a question about
on-chain data — machine-generated at the floor, human/AI-refinable in meaning and logic, and
instantiable like a function rather than copied like a file. A spec, never a program.** Four honest
layers — *sources* (generated), *meaning* (`semantic.toml`, RFC-0016), *logic* (authored SQL
views), *identity* (the content-addressed manifest, RFC-0012) — and one property that ties them
together: **a nest is a function, not a fork.**

Two of those layers are unfinished, and this RFC finishes them:

- **§1 — Logic: first-class authored SQL views.** `analytics.rs::define_nest_views` *already*
  loads `views/*.sql` into the DuckDB hot∪cold surface — but silently, un-scaffolded, unvalidated,
  invisible to every describe-surface, and drift-unchecked. Promote it from a hidden hook to a
  first-class nest layer: scaffolded by `init`, validated through the RFC-0016 errors-as-prompts
  path, surfaced in `/schema` and the MCP, described in `semantic.toml`, drift-gated, and taught by
  the builder skill. This is the layer that makes a nest feel *alive* when you open it, and it is
  **not a core change** — the views are read-only DuckDB `CREATE VIEW`s over already-sealed
  segments and the hot snapshot; nothing touches the deterministic ingest→decode→seal path.
- **§2 — Shape: parameterized, composable nests via an optional Starlark front-end.** A nest may be
  authored as a `nest.star` that *computes* its config — loops over addresses, shares an ABI once,
  and `load()`s another nest to extend it — evaluated hermetically at load time to the **same**
  `Config` struct the TOML produces. This turns `graph-network-nest` from a copy of `horizon-nest`
  into an *instance* of it. TOML stays the zero-authoring default `init` emits; Starlark is opt-in
  sugar for when config needs to compute. **Also not a core change** — it's a second front-end
  parser beside the TOML one; the core only ever sees the resolved `Config`.

- **§3 — Deferred (named, not built): hot incremental author-views.** Letting an authored view be
  maintained *live on the tip* by DBSP (SQL→circuit, reorg-safe, like the hardcoded balance
  circuit in `views.rs`) is the one piece that would put author-supplied logic into the
  deterministic data path — non-negotiable #4 territory. Explicitly deferred, with the cost on
  record, behind its own future RFC and a wad of reorg property tests.

The compounding argument for doing both §1 and §2 (in that order): §2 makes a nest a reusable unit;
§1 gives that unit a brain; and because §2 parameterizes, the §1 logic is **written once and shared
across every instantiation** (one `pool-tvl` view, inherited by the Uniswap-v3 nest on Base,
Arbitrum, and Optimism) instead of forked three ways.

**The proof is on the org already (§5).** `horizon-nest` and `graph-network-nest` are, today, a
measured byte-identical fork (only `name` and README differ). They are this RFC's acceptance case:
§5 refactors horizon into the §1 exemplar (it gains the missing `semantic.toml` + validated views)
and graph-network into the §2 exemplar (fork → `load()`-based instance), with the nests' existing
`checks/` parity harness proving the revamp is observably a no-op on results. "The RFC landed well"
is then a green `nuthatch check`, not an assertion.

## Motivation

**The inert-nest itch.** Open a freshly-`init`-ed nest and you see `nuthatch.toml`, an `abis/`
folder, and generated `schema.json`. It is a faithful *description* of an indexing job and it
contains no authored thought. For the zero-authoring happy path that is exactly right and must stay
so. But it means a nest never *shows its reasoning* — the "top holders," the "daily volume," the
"delegator exposure" that is the actual reason the nest exists lives only in ad-hoc queries typed at
a REPL and then lost. There is nowhere in a nest to *say* what it computes.

**The fork problem — measured, not hypothesized.** The `nuthatch-indexer` org today has
`horizon-nest` and, beside it, `graph-network-nest` whose own description reads *"Seeded from
horizon-nest, expanding to full parity."* A full diff of the two repos (taken 2026-07-19) shows
"seeded from" means **byte-identical**: same three contracts at the same addresses, the same six
`views/*.sql`, the same `abis/`, the same `schema.json`, the same `checks/`. The *only* differences
are the `name =` line in `nuthatch.toml` and the README. graph-network-nest's entire honest content
today is "horizon-nest, plus a roadmap (RFC-0011: Curation, GNS, Epochs, Indexer Directory)." There
is no mechanism for one nest to *reuse* another, so the catalogue (`nests`) is on a path to becoming
a museum of divergent forks that must be patched in parallel — the moment a bug is found in the
shared `delegations.sql`, it must be fixed in N copies, and any copy that misses the fix **silently
rots** (it's a copy, not a reference; nothing tells it it drifted). That is the opposite of what a
"prebuilt nest catalogue" ([nest-catalogue.md](../nest-catalogue.md)) is supposed to buy an
operator.

**Both real nests also demonstrate the §1 gap directly.** Each already ships a `views/` dir (six
authored SQL derivations, filename-ordered `10-`…`60-`) *and* a `checks/` parity harness — i.e.
they are already using the cold-views layer in anger — yet **neither has a `semantic.toml`**. So
today the MCP and enriched `/schema` cannot see that `operators`, `indexers`, or `rewards_daily`
exist at all; an agent pointed at either nest is blind to the very views that hold the answers. And
the silent-skip loader means a renamed column would drop a parity-critical view with no signal,
right under a harness whose job is to prove parity. horizon-nest's own README even names §3 for us:
its "Freshness" note says closing the sealed-vs-tip gap by "registering hot rows into DuckDB per
query" is "a nuthatch follow-up, not this nest's concern." The nests aren't a hypothetical for this
RFC — they are its test fixtures, and §5 makes them the acceptance case.

**Both symptoms have one cause:** a nest is currently *only* its sources layer. It has no logic
layer to make it alive, and no identity-as-a-function to make it reusable. This RFC names the
missing layers and fills them — in the cheap, additive, determinism-preserving order.

**Why now, and why cheap.** The substrate for the expensive-sounding half already exists and is
load-bearing in production:

- **Cold SQL views already run.** `analytics.rs::define_nest_views(conn, dir)` reads `views/*.sql`,
  sorts, and `execute_batch`es each against the same DuckDB connection that already `read_parquet`s
  the sealed segments and unions the hot snapshot (RFC-0013 §3). It works today. It is simply
  *hidden*: errors are swallowed to `tracing::debug!` and dropped; `init` never writes the dir; no
  describe-surface knows the views exist; nothing checks them for drift. Slice §1 is 80% promotion,
  20% new glue.
- **The validation machinery already exists.** RFC-0016 shipped `sql_errors.rs` (errors fuzzy-
  matched to the real schema) and a validate-without-executing `explain`. Author views get
  first-class validation for free by routing through it.
- **The describe surface already exists.** `semantic.toml` + the enriched `/schema` (RFC-0016 §2)
  is the one place a nest's meaning is authored and the many-readers surface that renders it. Views
  slot in as another described object, not a new mechanism.

So §1 is not new machinery; it is *finishing* machinery — the same move RFC-0016 made when it took
the raw schema and made it *governed*. §2 (Starlark) is the only genuinely new code, and it is
walled off as a front-end that cannot reach the data path.

## Goals

1. **State what a nest is**, on the record, as the primary artifact of this RFC (§0). Future
   feature RFCs cite it instead of relitigating it.
2. **Make a nest able to hold authored logic** — visible, diffable, reviewable, AI-legible — via
   first-class `views/*.sql`, *without* moving the zero-authoring floor.
3. **Make a nest a function, not a fork** — parameterizable and composable — via an optional
   Starlark front-end, *without* making Starlark mandatory or letting it into the data path.
4. **Preserve every non-negotiable**: single binary, ≤2 GB RAM budget, no phone-home, determinism
   in the core, AGPL-3.0. §1 and §2 are provably outside the deterministic data path; §3 (which
   isn't) is deferred precisely because it isn't.
5. **Keep the whole thing AI-native**: both new surfaces must be things an LLM writes fluently
   (SQL; Python-shaped Starlark) and must be taught by the builder skill (RFC-0017) under its drift
   gate.

## Non-goals

- **A bespoke nest DSL ("Nestlang").** Rejected outright (see Alternatives). It has neither of
  nuthatch's superpowers — no generator advantage (the CLI already emits TOML) and no training-data
  presence (the agent can't write a language days old) — so it would degrade both the human and the
  agent experience to earn a colored dot on GitHub. Every "language" in a nest is **borrowed**
  (SQL, Starlark), never invented.
- **Making a nest a program.** The instant a nest contains imperative, effectful, per-event
  handler code as its *front door*, we have re-grown the subgraph-mapping tax nuthatch exists to
  abolish. Imperative logic remains the WASM escape hatch (RFC-0014-era transform layer), reached
  by almost no one, not the shape of a nest.
- **Hot, incrementally-maintained author-views.** Deferred to §3 and a future RFC. This RFC ships
  *cold* views only (analytical, read-only, recomputed per query over the union), which is where
  the visible-logic win lives and where the determinism risk does not.
- **Turing-complete config.** Even in §2, Starlark is used in its hermetic, guaranteed-terminating,
  side-effect-free mode (no ambient FS, no clock, no randomness, no network) — config that
  *computes*, not config that *does things*.
- **Forcing Starlark on anyone.** TOML remains what `init` emits and what the common one-contract
  nest uses forever. A nest with two contracts should never see a `def`.

## §0 — What a nest is (the definition on record)

> **A nest is a content-addressed, reproducible specification of a question about on-chain data —
> machine-generated at the floor, human/AI-refinable in meaning and logic, and instantiable like a
> function rather than copied like a file. A spec, never a program.**

A nest has **four layers**, each optional above the first:

| Layer | File(s) | Owner | Status |
|-------|---------|-------|--------|
| **Sources** — which contracts, which chain, from when, decoded how | `nuthatch.toml` + `abis/*.json` + generated `schema.json` | machine (`init`); the zero-authoring floor | shipped |
| **Meaning** — what the tables/columns *mean*, the footguns, the hot/cold seam | `semantic.toml` | human/agent, seeded by `init` | shipped (RFC-0016) |
| **Logic** — the derivations the nest exists to answer | `views/*.sql` | human/agent | **half-shipped — §1 finishes it** |
| **Identity** — the reproducible, verifiable, packable unit | content-addressed manifest / blob | the runtime | shipped (RFC-0012) |

And **one property** across all four: **a nest is a function, not a fork.** It should be
*instantiable* (parameterized by an address / chain / start block) and *composable* (one nest
extends another). §2 delivers this property; without it, layers 1–3 still just get copied.

**The frame:** *a nest is a cartridge.* The `nuthatch` binary is the console; a *roost* (RFC-0012)
is the console with a multi-cartridge slot; a nest is the self-contained, portable, reproducible
thing you drop in and it *plays* — indexes, derives, serves SQL, answers questions. Cartridges
don't ship their own CPU. That is exactly the right amount of aliveness: rich, portable, and
reproducible, with the engine kept in the console where determinism and the footprint budget live.

Everything below is filling in this picture, in the order that ships value first and touches the
core last.

## §1 — Logic: first-class authored SQL views (the alive layer)

### What exists

`analytics.rs::define_nest_views` already loads `<nest>/views/*.sql` (sorted, batch-executed) into
the DuckDB connection that serves `/sql`, alongside `define_labels_view` and `define_children_views`
and the per-event `read_parquet`-backed table views. So an author can *today* drop a
`views/top-recipients.sql` containing `CREATE VIEW top_recipients AS SELECT "to" AS addr,
count(*) n FROM "usdc__transfer" GROUP BY 1 ORDER BY n DESC` and query `top_recipients` — the
mechanism is real and tested (see the `recipients`/`top_recipient` tests in `analytics.rs`).

### What's wrong with it

It is a *hook*, not a *layer*:

1. **Invisible.** `init` never creates `views/`, so nobody discovers the feature. It's word-of-
   mouth.
2. **Silent on error.** A malformed view is swallowed to `tracing::debug!` and dropped — the view
   simply doesn't exist and the author gets no feedback. This directly contradicts the RFC-0016
   principle that a stale/broken thing is *worse* than an absent one.
3. **Undescribed.** `/schema` and the MCP don't list authored views or say what they mean; the
   semantic layer has no slot for them.
4. **Drift-blind.** A view referencing a column the registry no longer has fails silently forever.
5. **Untaught.** The builder skill (RFC-0017) has no `views.md`.

### The change (promote hook → layer)

- **Scaffold at `init`.** Create `views/` with a commented, ready-to-uncomment starter view derived
  from the nest's own tables (e.g. a `count(*)`/`GROUP BY` over the first event table), plus a
  `views/README` one-liner. The happy path is unchanged (it's a directory of comments); the feature
  is now *discoverable* the moment you `init`.
- **Validate loudly, load safely.** On `dev` startup and via `nuthatch check`, validate each view
  through the RFC-0016 `sql_errors` path (fuzzy-matched, teaches the fix) and *surface* failures —
  a broken view is a loud warning at startup and a `check` failure, not a debug line. Loading stays
  fault-isolated (one bad view never takes down the others or the process), but silence ends.
- **Describe them.** Add a `[views.<name>]` section to `semantic.toml` (description, and optionally
  the columns the view exposes), seeded generically at `init` for any scaffolded view, and render
  authored views in the enriched `/schema` and the MCP `schema` tool exactly as tables are — so an
  agent *sees* `top_recipients` and what it means, and can query it without rediscovering it.
- **Drift-gate them.** Extend `semantic.rs::drift` (or a sibling) so a view that names a
  nonexistent table/column warns on `dev` and fails `check`, same discipline as semantic drift.
- **Teach them.** New `skills/nuthatch-builder/views.md` under the RFC-0017 drift gate: how to add a
  view, the hot∪cold caveat, the big-int `*_dec` reminder, the reserved-word `"from"`/`"to"`
  footgun — all referencing only real schema, CI-checked.

### Determinism / footprint (why this is *not* a core change)

Authored views are read-only `CREATE VIEW`s over the existing DuckDB surface: sealed Parquet via
`read_parquet` (immutable, past finality) unioned with the hot snapshot. They are **recomputed per
query**, never materialized, never written back, and never seen by ingest, decode, seal, or reorg.
Non-negotiable #4 is untouched: no author SQL enters the deterministic data path. Footprint impact
is a few `CREATE VIEW` statements at connection setup — negligible against the ≤2 GB budget, and
CI-checked by the existing budget gate regardless.

## §2 — Shape: parameterized, composable nests (optional Starlark front-end)

### The problem restated

`graph-network-nest` is a copy of `horizon-nest`. There is no `load()`. Fifty pool addresses off a
factory are fifty hand-repeated `[[contracts]]` blocks. Config cannot compute, so config gets
copied — and copies diverge.

### The change

Allow a nest to be authored as `nest.star` (Starlark — Google's hermetic, deterministic Python
subset; mature Rust impl `starlark-rust`, as used by buck2). It is **evaluated once, at load time,
hermetically** (no FS beyond declared `load()`s, no clock, no randomness, no network) to produce the
**exact same `Config` struct** that `toml::from_str::<Config>` produces today. The core is unchanged
— it receives a resolved `Config` and never learns whether a TOML or a `.star` produced it.

Config resolution precedence in `Config::load`: if `nest.star` is present, evaluate it; else parse
`nuthatch.toml`. `init` continues to emit **TOML** — Starlark is never forced.

```python
# nest.star — evaluates to the same Config a nuthatch.toml would. Pure by construction.
RPCS = ["https://ethereum-rpc.publicnode.com", "https://eth.drpc.org"]

def erc20(alias, address, start_block):
    return contract(alias, address, start_block, abi = "abis/erc20.json")  # one ABI, reused

STABLES = {
    "usdc": ("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", 6082465),
    "usdt": ("0xdac17f958d2ee523a2206206994597c13d831ec7", 4634748),
    "dai":  ("0x6b175474e89094c44da98b954eedeac495271d0f", 8928158),
}

nest(
    name = "stables",
    chain = "mainnet",
    rpc_urls = RPCS,
    contracts = [erc20(a, addr, sb) for a, (addr, sb) in STABLES.items()],
)
```

```python
# graph-network.star — "seeded from horizon-nest" done as an instance, not a fork.
load("//horizon:nest.star", "horizon_staking")

horizon_staking(
    chain = "arbitrum",
    extra_contracts = [erc20("grt", "0x9623063377ad1b27544c965ccd7342f7ea7e88c7", 1)],
)
```

The host exposes a **small, closed builtin surface** — `nest(...)`, `contract(...)`, `factory(...)`,
`template(...)` — mirroring the serde structs one-to-one, so there is nothing to compute *into*
except a valid `Config`. `load()` is restricted to nest files within the project/catalogue (no
arbitrary module import). Evaluation is bounded (step/time budget) so a pathological `.star` can't
wedge startup.

### The compounding win (why §1 + §2 is more than the sum)

A parameterized nest whose `views/*.sql` reference its tables gives you **write-the-logic-once,
instantiate-everywhere**: one `pool-tvl.sql` authored in the Uniswap-v3 nest is inherited by every
chain instantiation. Fix the view once, all instances get it. §2 makes the unit reusable; §1 gives
it a brain; parameterization means the brain is shared, not forked. Neither slice delivers this
alone — which is also why the **order matters** (§4).

### Determinism / footprint (why this is *not* a core change either)

Starlark-the-language is hermetic by construction (that is *why* Bazel chose it): no side effects,
guaranteed termination, reproducible evaluation — a near-perfect match for non-negotiable #4,
though it never needs to *be* in the data path because it only produces config. The interpreter runs
once at load and is dropped; it adds a dependency (`starlark-rust`) and a modest binary-size bump,
weighed in §4's gate. No runtime, hot-path, or per-block cost.

## §3 — Deferred: hot incremental author-views (named, not built)

`views.rs` maintains one *hardcoded* incremental view — per-address balances as a DBSP circuit,
reorg-safe (a reorg is the same transfer re-fed at weight −1). Letting an **authored** view be
compiled (SQL→circuit, which Feldera/DBSP can do) and maintained live on the tip the same way would
be genuinely powerful — sub-second derived state, reorg-correct. It is **deferred** because it is
the one thing here that puts author-supplied logic into the **deterministic data path**
(non-negotiable #4). It requires: a trusted SQL→circuit compile step, golden/deterministic-
simulation tests per compiled view, and random-reorg-depth property tests proving convergence — the
full RFC-0004/§correctness-rules treatment. It gets its own RFC when a real workload needs a
live-tip derived view maintained faster than per-query recomputation can serve. Until then, cold
views (§1) cover the demand and carry none of the risk.

## §4 — Sequencing and the gate

**Order is a risk control, not a preference.**

1. **§1 first (cold views).** Even a lonely, un-parameterized nest becomes *alive*. Ships on
   existing substrate, outside the data path, this release-ish.
2. **§2 second (Starlark).** Arrives as the *multiplier*, once there is authored logic worth
   reusing and forks worth collapsing. New dependency, so it clears its own gate (below).
3. **§3 maybe never (hot views).** Only when a real workload demands live-tip derived state.

**Do them backwards and you lose:** Starlark-first gives you beautifully parameterized *empty*
nests — an interpreter dependency spent to automate the copying of nothing. The day we'd regret the
order is the day we did §2 before §1.

**The §2 gate (before writing Starlark code):** confirm `starlark-rust`'s license is compatible with
AGPL-3.0 core and it is not on the CLAUDE.md forbidden list; confirm the binary-size delta is within
tolerance; confirm the builder skill can teach the closed builtin surface under its drift gate. If
any fails, §2 parks and §1 still stands alone — they are independently valuable, which is the point
of the layering.

## §5 — Proving it: refactoring horizon-nest and graph-network-nest (the dogfood acceptance)

The RFC is only "landed well" if the two real nests come out **better and provably unchanged in
behavior**. They are the acceptance case, not a follow-up. The refactor is staged to mirror §1→§2
and gated by the parity harness the nests already carry (`checks/`), so every step proves it changed
*nothing an operator can observe* while improving what an author and an agent can.

**Guiding invariant (the whole point of the harness):** at every step, `nuthatch check` against the
committed `checks/expected/*.json` must stay green, and the two nests' decode-registry hashes must be
unchanged. If a "revamp" moves a parity number, we broke something — the refactor is meant to be
observably a no-op on results and a step-change on legibility/reuse.

### Step A — horizon-nest becomes the §1 exemplar (add the missing meaning layer)

horizon-nest is the *nucleus* and stays a standalone, hand-authored nest — the reference for "a nest
with a brain, done right."

- **Author `semantic.toml`.** Describe each of the six views (`operators`, `allocations`,
  `delegations`, `indexers`, the `*_daily`/`*_hourly` rollups, `global`) and the key raw tables,
  with the footguns the README already documents (the `*_dec` base-unit convention; the
  sealed-lag-by-finality freshness note). This is the file the MCP/`/schema` render — it turns a
  currently-invisible view set into an agent-legible one.
- **Route the views through loud validation + drift.** Re-run under the §1 loader: the six views
  must validate (they do today, silently) and any future column rename now fails `check` with a
  fuzzy-matched hint instead of vanishing.
- **Fold `checks/` into the first-class story.** The existing parity checks become the canonical
  demonstration that a nest's authored logic is *validated*, not just *loaded* — `check` gains the
  view-drift gate alongside the parity comparison.
- **Acceptance:** the MCP `schema` tool lists all six views with their meanings; a deliberately
  broken view fails `check` loudly; parity fixtures stay byte-identical to pre-refactor.

### Step B — graph-network-nest becomes the §2 exemplar (fork → instance)

graph-network-nest is the *organism*: horizon's spine plus the RFC-0011 expansion. It stops being a
copy.

- **Replace the duplicated config with a `nest.star` that `load()`s horizon-nest.** The three shared
  contracts and the six shared views are *inherited*, not vendored. Concretely the resolved `Config`
  must be byte-identical to today's (modulo `name`), proving the front-end swap changed nothing.
- **Make the RFC-0011 expansion purely additive on top of the shared spine.** New contracts
  (`L2Curation`, `L2GNS`, `EpochManager`, Indexer Directory, Rewards) and their new
  `views/70-…`…`views/90-…` are appended in `graph-network`'s own tree; the staking core remains a
  single source of truth in horizon-nest. A fix to `delegations.sql` in horizon now propagates by
  reference.
- **Decide the composition seam explicitly** (ties to Open question #2): does graph-network `load()`
  horizon by same-org path, by git ref, or by a content-addressed *packed* nest (RFC-0012)? For the
  dogfood, start with a pinned path/ref; note the blob-reference upgrade as the reproducible
  end-state.
- **Acceptance:** `graph-network-nest`'s resolved `Config` and registry hash equal the pre-refactor
  values (minus `name`); its `checks/` parity stays green; deleting a shared view from horizon-nest
  now visibly breaks graph-network's `check` (proving the dependency is *real*, not copied).

### Step C — the proof writes itself back

- **Update both READMEs** to state the new model: horizon-nest is a reusable module; graph-network
  imports it and extends. The "Seeded from horizon-nest" euphemism is replaced by a real `load()`.
- **Add a one-paragraph "RFC-0018 landed here" note** to each nest pointing at the semantic layer
  (§1) and, for graph-network, the composition (§2) — so the catalogue itself documents the pattern
  the RFC establishes.
- **Optional roost demonstration:** both nests are on Arbitrum One, so a `roost.toml` co-tenanting
  them (RFC-0012) — one cursor, one `getLogs` — is a natural bonus proof that "function not fork"
  and "many nests, one runtime" are the same two repos. Nice-to-have, not required for acceptance.

### Why this is the right acceptance case

It exercises the whole RFC on production artifacts with an existing, committed parity oracle: §1
(horizon gains meaning + validation), §2 (graph-network becomes an instance), and the §3 deferral
(horizon's own freshness note) are all demonstrated on the two nests an operator actually deploys —
and the `checks/` harness makes "we changed nothing observable" a CI fact, not a claim.

## Implementation plan (when the time comes)

**§1 — cold views to first-class (small, mostly promotion):**
- `project.rs` / `init`: scaffold `views/` with a commented starter view + README; seed a
  `[views.<name>]` stub in `semantic.toml` for scaffolded views.
- `analytics.rs::define_nest_views`: keep fault-isolated loading; add a validation pass (reuse
  `sql_errors`) whose failures are *returned/surfaced*, not swallowed.
- `check.rs`: authored views validate + drift-check; `check` fails on a broken/drifted view.
- `serve.rs` + `mcp.rs`: authored views listed in `/schema` and the MCP `schema` tool, rendered
  from `semantic.toml` `[views.*]`.
- `semantic.rs`: `[views.*]` section + drift over view-referenced tables/columns.
- `skills/nuthatch-builder/views.md`: authored, under the RFC-0017 drift gate.

**§2 — Starlark front-end (new, gated):**
- New `src/starlark.rs` (or `config_star.rs`): `starlark-rust` host, closed builtin surface
  (`nest`/`contract`/`factory`/`template`) mapping 1:1 to serde structs, restricted `load()`,
  bounded evaluation → `Config`.
- `config.rs::load`: prefer `nest.star` when present, else `nuthatch.toml`; identical `Config` out.
- `pack.rs` (RFC-0012): a packed nest pins the *resolved* `Config` (and/or the `.star` + its
  `load()` closure) so a blob stays reproducible and content-addressed regardless of front-end.
- Builder skill: `config-as-code.md` teaching the closed surface, drift-checked.

## Testing and acceptance

- **§1:** golden test — a scaffolded nest exposes its starter view in `/schema` and the MCP; a
  deliberately-broken view produces a *surfaced, fuzzy-matched* error from `check` (not a silent
  drop) and does not crash `dev` or disable sibling views; a drifted view (renamed column) fails
  `check`. Footprint budget stays green.
- **§2:** the `stables` and `graph-network` examples above evaluate to a `Config` **byte-identical**
  to the equivalent hand-written TOML (round-trip equality is the acceptance bar); a `.star` with a
  side effect / nonterminating loop is rejected by the bounded host; a packed Starlark-authored nest
  `mount`s and produces the same registry hash as its TOML twin.
- **Determinism guard (both):** a property test asserting that neither authored views nor Starlark
  evaluation appear anywhere in the ingest→decode→seal→reorg path (i.e. the deterministic-core test
  suite is unchanged by either feature).
- **Agent acceptance (RFC-0016 Tier-B harness):** an agent asked to "add a top-holders view" and to
  "make this nest work on Base too" succeeds using only the builder skill — scored in the existing
  harness, honest that a real score needs a keyed run.
- **Dogfood acceptance (§5, the headline proof):** horizon-nest and graph-network-nest are
  refactored per §5 and, at every step, `nuthatch check` against their committed `checks/expected/`
  parity fixtures stays green and their decode-registry hashes are unchanged — i.e. the revamp is
  *observably a no-op on results* while making horizon's views agent-legible (§1) and turning
  graph-network from a byte-identical fork into a `load()`-based instance (§2). Deleting a shared
  view from horizon-nest must break graph-network's `check`, proving the dependency is a real
  reference, not a copy. This is the acceptance test that proves the RFC "landed well."

## Risks

- **Feature creep past "spec, not program."** Mitigation: §0 is the standing definition; the WASM
  escape hatch remains the *only* imperative path and stays a back door. Any proposal to make views
  or `.star` effectful is a §0 violation to be flagged, per CLAUDE.md.
- **Two config front-ends confuse users/agents.** Mitigation: TOML is the default `init` emits and
  the documented norm; Starlark is explicitly opt-in sugar; the builder skill teaches "reach for
  `.star` only when config repeats." One-contract nests never see it.
- **`starlark-rust` dependency / license / size.** Mitigation: the §4 gate — if it fails AGPL
  compatibility or the size budget, §2 parks and §1 stands alone.
- **Cold views mistaken for live-tip state.** A cold view recomputes per query over hot∪cold; it is
  *not* an incrementally-maintained entity. Mitigation: `semantic.toml` view descriptions and the
  `views.md` skill state the hot∪cold, recomputed-per-query semantics explicitly; §3 is where
  live-tip maintenance would come from, and it's deferred.
- **Silent-to-loud is a behavior change.** Promoting swallowed view errors to `check` failures could
  surprise anyone relying (unknowingly) on the current silent-skip. Mitigation: it's a Draft/new-
  minor behavior; the failure teaches the fix (RFC-0016), and a broken-but-ignored view was never
  doing anything anyway.

## Alternatives considered

- **A bespoke nest DSL ("Nestlang").** Rejected. Fails both superpowers: no generator advantage
  (CLI already emits TOML) and zero training-data presence (an agent can't write a language days
  old — the exact hallucination problem RFC-0017 fights). It would buy a colored GitHub dot at the
  cost of a worse human *and* agent experience, plus a lexer/parser/checker to maintain forever.
- **Rhai / Lua / Dhall instead of Starlark (for §2).** Rhai and Lua are general scripting languages
  — Turing-complete and effect-capable by default, so we'd be *removing* capabilities to make them
  safe. Dhall is total and pure (attractive) but far less present in training data and less
  ergonomic for the "loop over addresses" shape. Starlark wins on the exact axis that matters:
  hermetic/deterministic *by design* (matching non-negotiable #4 without extra work) **and**
  Python-shaped (so LLMs write it fluently), with a mature Rust host. It was built for precisely
  this — config that computes but must be reproducible.
- **SQL *or* Starlark (pick one).** They solve different holes — SQL is the *logic/brain*, Starlark
  is the *shape/skeleton*. A nest with SQL and no Starlark is alive but forked; a nest with Starlark
  and no SQL is reusable but hollow. The answer is *both, sequenced* (§4), not either.
- **Do §2 (Starlark) first.** Rejected — see §4. Parameterizing empty nests spends a dependency to
  automate copying nothing; logic-first makes even a single nest worth having.
- **Materialized cold views (write derived tables back).** Rejected for now — reintroduces a writer
  and a staleness/invalidation problem the per-query recompute avoids, for a performance win no
  current workload needs. If it's ever needed, it's the §3 (incremental, DBSP) path, done properly,
  not a bolted-on cache.
- **Ship imperative (WASM) handlers as the logic layer instead of SQL.** Rejected as the *front
  door* — that's the subgraph-mapping tax again. WASM stays the escape hatch for the rare imperative
  case; SQL is the legible, AI-native, already-the-query-surface default.

## Open questions

1. **View dependencies / ordering.** Views currently load in filename sort order. Do we need
   explicit inter-view dependency declaration, or is "name them so they sort right" + fault
   isolation enough for v1? (Lean: enough for v1; revisit if real nests build deep view stacks.)
2. **Starlark `load()` scope.** Restrict to same-repo/catalogue paths only, or allow a
   content-addressed nest-blob reference (`load` a *packed* nest by hash) so composition is
   reproducible across repos? (Lean: v1 same-repo; blob-reference `load()` is a natural RFC-0012
   tie-in later.)
3. **Where does a packed nest pin its config?** The resolved `Config`, the `.star` + closure, or
   both? Affects reproducibility guarantees for Starlark-authored blobs (see §Implementation /
   `pack.rs`).
4. **`[views.*]` column typing.** Do we require authors to declare a view's output columns in
   `semantic.toml`, or introspect them from DuckDB at load and only let authors *describe* them?
   (Lean: introspect the shape, author only describes — less to keep in sync, matches how the
   footguns are *derived* not authored in RFC-0016.)
5. **Does §1 alone justify a minor version bump**, given the silent→loud behavior change, or does it
   ride with §2? (Process, not design.)
