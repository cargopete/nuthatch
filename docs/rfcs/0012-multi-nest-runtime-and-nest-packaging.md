# RFC-0012: Multi-nest runtime and content-addressed nest packaging

- Status: Accepted — implementing (2026-07-18). §0 brief amendment merged; slices 5–6 (`nest pack`/`mount`) shipped; the roost (§1–4) is next.
- Author: Pete (cargopete)
- Date: 2026-07-17
- Depends on: RFC-0001 (Implemented — decode registry, nest toml), RFC-0009
  (Implemented — child registry, shared-tables/single-filter routing; the fan-out
  primitive this design generalises), RFC-0004 (Implemented — the backfill this must
  compose with)
- Blocks: GraphOps hosting many tenants in one runtime (their density ask, 2026-07-17
  Discord); nest distribution / `mount <hash>`.
- Priority: NOT a blocker for v0.1.0-rc.1 or the GraphOps **pilot** — the pilot is a
  single nest (Lodestar-on-Ethereum, RFC-0011), which needs none of this. This is the
  feature GraphOps wants *after* the pilot proves out, to avoid orchestrating N
  separate Nuthatch processes. Design now (the partner is asking); build after the
  pilot lands.
- ⚠️ **Brief amendment required.** `CLAUDE.md` lists *multi-tenancy* under
  "Out of scope — do not build", and states the ≤2 GB budget as *per single-chain
  nest*. This RFC deliberately scopes a **subset** in (multi-nest co-tenancy) while
  keeping the forbidden part out (hosted-SaaS multi-tenancy), and restates the budget
  as **per-runtime**. §0 is the proposed amendment; it must be accepted before §2+ is
  built, not assumed.

## Abstract

One Nuthatch runtime hosts many nests — a **roost** — sharing a single cursor, a
single hot database, and one serving process, instead of one OS process per nest. And
a nest becomes a **content-addressed blob**: its authored inputs bundled and pinned by
hash, so deploying a nest is `mount <hash>`. These are two halves of one feature — the
blob is the *deploy unit*; the roost is what *mounts many of them*.

The load-bearing constraint, and the thing that keeps this inside Nuthatch's founding
discipline: co-tenant nests share a cursor **only when they share a chain**. One chain
→ one cursor, one log stream, fanned out to N nests' registries (exactly RFC-0009's
mechanism, generalised from "children of one factory" to "N independent nests").
Different chains still mean a process each — a second chain is a second cursor, and
Nuthatch does not run two cursors in one process. That is a feature, not a limitation.

## Motivation

GraphOps' platform wants to offer Nuthatch as a data service beside RPC / Subgraphs /
Substreams. Running one process per customer nest is operationally heavy (N cursors, N
redb files, N ports, N supervisors) when most tenants index the *same* chain and could
share the expensive part — the chain read. The partner said it plainly: "within a
single runtime and hot database, deploy multiple nests… the alternative is to
orchestrate individual deployments, which is less ideal."

Packaging is the same problem from the other end. To mount a nest you must first *name*
one unambiguously and move it around. Nuthatch already content-addresses its data
(sealed segments) and its decode (`registry_snapshot` hash in the seal manifest,
RFC-0009 §2). Applying the same discipline to the nest *definition* gives a pinnable,
reproducible deploy unit — and the natural input to `mount`.

## §0 — Proposed brief amendment (decide before building)

Two edits to `CLAUDE.md`, no more:

1. **"Out of scope" → split the term.** *Hosted-SaaS multi-tenancy* (billing,
   metering, per-tenant authz, tenant isolation for mutually-untrusting customers)
   stays out — it is the become-a-data-service-company path, and it is GraphOps'
   gateway's job regardless (RFC's node-vs-gateway split, the `/sql` DoS discussion).
   *Multi-nest co-tenancy* (N nests, one runtime, one cursor, one hot DB, cooperating
   tenants an operator chose to co-locate) moves **in**.
2. **Footprint budget → per-runtime.** "≤2 GB for single-chain tip-following" becomes
   "≤2 GB for a single-chain **roost**", i.e. shared across mounted nests. Density is
   RAM-bounded, not free (§3).

Everything else in the non-negotiables holds unchanged, and this RFC is built to keep
them: single cursor, single writer, reorgs only touch the hot store, determinism,
no phone-home (§5 resolution is local-first).

## Goals / Non-goals

**Goals.** Multi-nest co-tenancy on a shared same-chain cursor; strict per-nest
isolation (storage, reorg, blast radius) so one nest's bad view or unbounded factory
can't harm another; content-addressed nest packaging as the deploy unit; a
*reproducible* mount (regenerate the decode registry from the blob's inputs and assert
its hash matches the blob's manifest); local-first resolution; an honest per-runtime
footprint model with a pre-mount estimate.

**Non-goals.** Hosted-SaaS multi-tenancy — billing, metering, per-tenant authn/quotas
(gateway's job, §0). Cross-chain in one runtime (a second chain = a second cursor =
a second process; unchanged). Cross-nest federated queries (that is scaled-mode
DataFusion, RFC-future; `/sql` stays per-nest scoped). A hosted nest registry /
marketplace (that is a gated service — explicitly out; resolution is local-first,
transports are BYO). Implementing signing / capability enforcement (the blob
*reserves a slot* for it, §4; building it is deferred to a later RFC). Choosing the
blob's concrete container format (this RFC fixes the manifest schema and the hashing
rule; tar vs CAR vs OCI is an implementation detail left open).

## Design

### 1. Layout: from a nest dir to a roost

Today a nest *is* a directory: `nuthatch.toml`, `nuthatch.redb`, `abis/`, `schema.json`,
`views/`, `labels/`, sealed `segments/`, skills, `llms.txt`. A roost is a directory of
nests plus a runtime config:

```
roost/
  roost.toml                 # chain + rpc_urls + the mounted-nest list (names or blob hashes)
  nests/
    lodestar/                # a nest dir, exactly as today
    uniswap-v3/              # another, same chain
```

`roost.toml` owns the chain identity (`chain`, `chain_id`, `rpc_urls`) — the shared
cursor's chain — and lists mounted nests. A nest's own `nuthatch.toml` keeps its
contracts/templates/factories/screening/flags/views; its `[nest].chain` must match the
roost's (validated at mount; a mismatch is a hard error, since a different chain needs
its own roost). Single-nest `nuthatch dev` is unchanged — it is the degenerate roost of
one, and the existing directory layout is preserved for back-compat.

### 2. The shared cursor (the actual win) — a straight generalisation of RFC-0009

RFC-0009 already fetches one chain's logs under a single filter and routes each log to
the right registry entry (`(template, child) → rows`), all on one cursor. Multi-nest is
the same loop with the routing key widened to `(nest, contract) → rows`:

- **One cursor, one poll.** Each poll fetches the **union** of every mounted nest's
  address/topic filter for the shared chain. One `getLogs` (or one ExEx receipt stream)
  feeds all nests.
- **Fan-out routing.** Every returned log is matched against the merged registry and
  written to the owning nest's tables. A log an address belongs to two nests (rare but
  legal — e.g. two nests both index USDC) is routed to both; decode is a pure function
  of `(log, abi)`, so this is duplication of *storage*, never of *fetch*.
- **Filter at scale = the RFC-0009 flip, for free.** The union address list grows with
  tenant count; above the existing threshold it flips to topic0-only fetch with local
  registry-lookup routing (RFC-0009 §4). Many nests make the flip *more* valuable, and
  it is already built — the roost inherits it.
- **Backfill.** Each nest backfills its own range (nests mount at different times and
  cover different history). A newly mounted nest backfills alone; the shared cursor only
  couples nests at the **tip**. This keeps RFC-0004's pipelined backfill per-nest and
  avoids entangling backfill windows across tenants.

### 3. Isolation, reorg, and the footprint model

**Storage is per-nest, not shared tables.** Each nest keeps its own `nuthatch.redb` and
its own `segments/` under `nests/<name>/`. This is deliberate: it preserves single-
writer-per-store (the one ingestion thread still writes each store), keeps reorg
rollback and finality pruning per-nest and independent, and bounds blast radius — a
corrupt view or a runaway factory in nest A cannot touch nest B's data. The *cursor* is
shared; the *stores* are not. (Rejected alternative: one redb with nest-prefixed keys —
weaker isolation, entangles pruning, no upside once the cursor is already shared.)

**Reorg fan-out, one boundary.** The shared cursor detects a reorg once (block-hash
checkpoint mismatch) and fans the rollback out to every mounted nest's hot store, each
rolling back to the fork block by the existing mechanism. One cursor, one reorg
handler, one observable failure boundary — the non-negotiable holds. Finality is shared
too: one finality height per chain drives every nest's sealing.

**Footprint is per-runtime (§0).** Rough model: `base_serving + Σ_nests(tip_working_set
+ redb_cache)`. The shared cursor and one DuckDB/serving process are paid once; each
nest adds its tip working set. The roost therefore prints a **pre-mount RSS estimate**
(as RFC-0009 prints a pre-backfill row estimate) and refuses a mount that would push the
projected resident set past a configured `max_rss` (default the 2 GB budget). Density is
honest and bounded, not aspirational. `/metrics` (RFC-0005) gains per-nest RSS
attribution.

### 4. The nest blob (the deploy unit)

A nest blob is the nest's **authored inputs**, canonicalised and content-addressed:

- **Contents:** `nuthatch.toml`, `abis/*`, `schema.json`, `views/*.sql`, skills,
  `llms.txt` — the things a human authors. *Not* the generated decode registry, *not*
  sealed data: those are derived, and including them would make the hash depend on
  build artifacts. Instead the manifest records the **expected `registry_hash`** and
  the **generator/schema version** that produced it.
- **Manifest + hashing.** A `manifest.json` lists every included file with its own
  content hash, plus `{ schema_version, generator_version, registry_hash }`. The blob
  hash is the hash of the canonical manifest (stable field order, stable file order,
  fixed encoding) — a Merkle root over the inputs. This reuses the content-addressing
  discipline already in the seal manifest; it does **not** invent new crypto.
- **Reproducible mount.** On `mount`, the runtime regenerates the decode registry from
  the blob's `nuthatch.toml` + ABIs and **asserts the resulting `registry_hash` equals
  the manifest's**. A mismatch means the blob was authored by a different generator
  version — rejected, exactly as `schema_version > CURRENT` is rejected today
  (`config.rs`). This extends determinism from the data path to the *authoring* path:
  same inputs + same generator → same blob → same decode, verifiably.
- **Immutability + upgrade.** A blob is immutable; changing a nest yields a new hash. A
  thin name→hash pointer layer (in `roost.toml`) gives "the lodestar nest = its current
  hash". Live data across an upgrade is versioned **side-by-side**, never retro-
  re-decoded (the existing "version decodings" rule, RFC-0001).
- **Reserved, deferred: signing + capabilities.** The manifest reserves optional
  `signature` and `capabilities` fields. A blob is the natural signable, self-describing
  trust unit, and capabilities (which effect grants a nest's transforms may hold —
  liminal composition-manifest purity) belong here. **This RFC neither builds nor
  requires them** — it only shapes the manifest so a later RFC can add them without a
  format break. Said out loud so "later" stays later.

### 5. Resolution is local-first (the no-phone-home line)

`mount <hash>` resolves a blob through a chain of transports, local first:

1. Local content-addressed store (`~/.nuthatch/blobs/` or a roost-local `blobs/` dir).
2. Any operator-configured transport — a directory, an HTTP URL prefix, IPFS, an OCI
   registry — **BYO, pluggable, none mandatory**.

There is deliberately **no default Nuthatch-hosted registry**. A hosted nest registry
or marketplace would be a gated service / phone-home, which the brief forbids and this
RFC does not walk into. If such a thing is ever wanted it is a separate conversation and
almost certainly *GraphOps'* layer, not core's. `pack` (produce a blob from a nest dir)
and `mount` (resolve + verify + install a blob) are the only two verbs core owns.

### 6. Serving surface

Per-nest, namespaced, isolated:

- `GET /nests` — the roster (name, blob hash, chain, tip/sealed height, RSS).
- `GET /<nest>/...` — every existing per-nest route (`/tables`, `/table/{name}`,
  `/entity/{id}`, `/sql`, `/balances`, …) under the nest's prefix. `/sql` stays
  **per-nest scoped** (a query sees one nest's segments — isolation, and no cross-nest
  DuckDB attach). The `/sql` DoS guard (`QueryGuard`, admission gate) applies per nest;
  the admission gate becomes roost-wide so total analytical concurrency stays bounded
  across tenants.
- MCP: the server takes a nest selector; each nest is independently discoverable.

Access control (which caller may reach which nest, quotas) is **not** added here — that
is the gateway's identity-shaped job, unchanged from the node-vs-gateway split.

## Implementation plan (vertical slices; each ends runnable)

1. **Roost layout + serving.** ✅ **Done (2026-07-18).** `roost.toml` (`[roost]` chain/chain_id/
   rpc_urls + a `nests` list) at `src/roost.rs`, `nests/<name>/` layout, `nuthatch roost dev`,
   `/nests` roster + each nest's full API under its `/<name>/…` prefix (`serve::run_roost`,
   `Router::nest`). Chain identity is hoisted to the roost and every mounted nest's `[nest].chain`/
   `chain_id` is validated against it (mismatch = hard error; a different chain needs its own roost).
   *Still one cursor each* at this step (naive) — the shared cursor is slice 2. Stores stay per-nest
   and isolated (own redb/segments/views). `indexer::run` was refactored into `spawn_nest` (builds a
   nest's state + background tasks) + a thin `run` (serve + fate-share), so `dev` is unchanged (the
   roost-of-one) and the roost fate-shares the server with *all* nests' ingestion via `select_all` —
   any nest's ingestion dying exits the whole process non-zero (single-failure-boundary, generalised).
   Tests: valid load, wrong-chain reject, reserved/duplicate name reject, empty-list reject (132 tests,
   +4; clippy clean). The live two-nest demo folds into slice 2's path-equivalence gate (it needs real
   ingestion over a chain regardless).
2. **Shared cursor.** Collapse to one cursor per chain: union filter, fan-out routing
   by `(nest, contract)`, tip-only coupling. Reuse RFC-0009 routing + the topic0-flip.
   Acceptance: two nests, one `getLogs` stream, byte-identical per-nest tables vs
   running each nest solo over the same range (the RFC-0004/0009 path-equivalence
   discipline, roost edition).
   - **2a — static nests. ✅ Done (2026-07-18).** Two behaviour-preserving extractions first
     (`NestIngest`, then `index_loop` takes a `NestIngest` + a reusable `prepare`), so the shared
     driver runs the *same* per-window code a solo `dev` runs. Then `indexer::roost_index_loop`:
     each nest `prepare`s its own backfill (tip-only coupling — backfill stays per-nest), then one
     shared loop does one `source.tip()` + one **union `getLogs`** per window and demuxes each log to
     the owning nest by address (`NestIngest::owns`) through `process_window`. A `min`-based global
     cursor lets nests at different heights self-heal; a nest with zero owned logs still advances +
     seals (identical to solo). `roost dev` now drives one `spawn_roost` task. Byte-identity holds by
     construction (shared code path) + demux unit tests (`owns`, `union_filter`, demux-reproduces-solo);
     135 tests (+3), clippy `-D warnings` clean; two-nest boot smoke green (mounts both, one cursor,
     API live). The full **live** two-nest table-parity run over a chain is the remaining acceptance
     evidence (folds in with a live demo, as the RFC-0011 pilot proved delegation parity).
   - **2b — factory nests (deferred).** A factory nest is topic0-only (no address to demux), which
     would force the whole union fetch topic0-only and tangle the demux; `spawn_roost` refuses one with
     a clear error for now. Route-by-decode + the topic0-flip under the shared cursor is 2b.
3. **Reorg + finality fan-out.** One reorg → all nests converge; shared finality drives
   each nest's sealing. Extend the reorg proptest with a multi-nest dimension (random
   reorgs converge every mounted nest independently).
4. **Footprint model.** Pre-mount RSS estimate, `max_rss` refusal, per-nest `/metrics`
   attribution. Publish a 3-nest roost RSS number (the honesty rule).
5. **`pack` + blob manifest.** ✅ **Done (2026-07-18).** Shipped as `nuthatch nest pack <dir>` (the
   `nest` command group — the RFC-0008 compliance `pack` already owns the bare verb). Canonical
   manifest (`{blob_format_version, nest_name, schema_version, generator_version, registry_hash,
   files:[{path,sha256}]}`, files sorted, compact encoding), blob hash = `sha256` of the canonical
   manifest, `registry_hash` regenerated from inputs + `verify_registry_reproduces` (the check `mount`
   will run). Blob is a content-addressed *directory* for now (identity is the manifest hash; a
   single-file container is a later wrapper). Tests: determinism, registry-pin/verify, changed-input,
   derived-file exclusion.
6. **`mount` + local-first resolution.** ✅ **Verb + verify + install done (2026-07-18).** Shipped as
   `nuthatch nest mount <blob> [--dir <target>] [--expect <hash>]`: resolves a blob from the **local**
   filesystem (no network — the no-phone-home line holds by construction; a BYO transport is a later
   wrapper), rejects a newer `blob_format_version`, checks the optional `--expect` content address
   *before* touching disk, verifies every file's `sha256` against the manifest, installs into the target
   (defaults to `./<nest_name>/`, refuses a non-empty target), then runs `verify_registry_reproduces`
   — regenerating the decode registry from the *installed* inputs and asserting it matches the manifest's
   pinned `registry_hash`. Tests: pack→mount round-trip (+registry reproduces), wrong-`--expect` reject,
   tampered-file reject, newer-format reject. **Deferred to the roost slices:** installing *into a roost*
   and mounting two nests under one cursor (needs the roost runtime, §2, gated on the §0 CLAUDE.md
   amendment).
7. **Docs + example.** A runnable two-nest roost example (Lodestar + one more,
   same chain), mounted from blobs; operators page update.

## Testing and acceptance

- Path-equivalence: a nest in a roost produces byte-identical tables and segments to the
  same nest run solo over the same range (reuses the RFC-0004 discipline).
- Isolation: a nest with a deliberately broken view / a runaway factory does not affect
  a co-tenant's data, tip, or serving.
- Reorg: multi-nest proptest — random reorg depths converge every mounted nest to
  canonical state independently.
- Determinism: `pack` is byte-identical across machines; `mount` rejects a blob whose
  regenerated `registry_hash` ≠ manifest (and one whose `schema_version` is too new).
- Footprint: a 3-nest same-chain roost stays within the per-runtime budget; the
  pre-mount estimate is within a stated tolerance of measured RSS.
- No-network: `mount` from a local blob works fully offline; there is no code path that
  contacts a Nuthatch-hosted service.

## Risks

- **Budget creep with tenant count.** N tip working sets share 2 GB; the estimate +
  `max_rss` refusal is the guard, but it caps density. Honest, and better surfaced than
  discovered. Mitigation: the per-nest hot-store working set is already small
  (point-read tip only); most bytes are shared serving.
- **Shared-cursor coupling at the tip.** A single slow/failing RPC now stalls every
  nest, not one. This is inherent to sharing the read (and is the point). Mitigation:
  the existing round-robin failover is roost-wide; one flaky nest cannot stall the
  cursor (nests don't drive fetch, the union filter does).
- **Blob format churn.** If the manifest schema changes after hashes are published,
  every blob rehashes. Mitigation: do not publish blob hashes as stable identifiers
  until the manifest is frozen at v1; the `schema_version`/`generator_version` fields
  make an incompatible blob fail loudly, not silently.
- **"Multi-tenancy" scope drift.** The word invites billing/metering/registry
  expectations. Mitigation: §0 and the Non-goals draw the line explicitly; the partner
  conversation keeps "hosted registry" as a separate, deferred topic.

## Alternatives considered

- **One process per nest (status quo).** Correct and maximally isolated, but the density
  GraphOps is trying to avoid. Multi-nest keeps the isolation (per-nest stores) while
  sharing the one genuinely-expensive thing (the chain read).
- **Shared tables with a `nest_id` column** (à la RFC-0009 children). Rejected for
  independent nests: it entangles reorg/pruning/finality and weakens blast-radius
  isolation, with no gain once the cursor is already shared.
- **Cross-chain nests in one runtime.** Rejected: a second chain is a second cursor,
  which violates the single-cursor non-negotiable. Separate processes, orchestrated
  externally — the honest boundary.
- **Blob includes the built registry + data.** Rejected: makes the hash depend on build
  artifacts. Blobs pin *inputs*; the registry hash is *asserted*, giving reproducibility
  without shipping derived bytes.

## Open questions

1. Does GraphOps want mounted-nest lifecycle (mount/unmount/upgrade) exposed as a
   runtime admin API, or driven only by `roost.toml` + restart? Admin surface overlaps
   RFC-0010; decide where it lives. Their layer may prefer to own orchestration.
2. Pointer layer for "latest": in `roost.toml` (simple, restart to move) vs a mutable
   runtime command. Start with the file; revisit if hot-swap is demanded.
3. Per-nest resource *quotas within* a roost (one nest's `/sql` concurrency vs another's)
   — is roost-wide admission enough, or do co-tenants need fair-share? Likely gateway
   territory (identity-shaped), but the roost-wide gate is the floor. Raise on the call.
