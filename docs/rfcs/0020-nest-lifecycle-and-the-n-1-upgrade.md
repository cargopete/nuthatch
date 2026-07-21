# RFC-0020: Nest lifecycle and the N-1 upgrade — kill the subgraph resync tax

- Status: **Accepted** (2026-07-21) — **slice 1** (compatible/breaking classifier + `nest diff`) and
  **slice 2a** (the atomic serving-swap mechanism: `SharedNest` re-points an endpoint's backing with no
  rebind, via axum `FromRef` so no handler changes) shipped 2026-07-21; **slice 2b** (index-new-then-
  flip orchestration), 3 (breaking → new endpoint + deprecation), 4 (segment reuse) pending.
- Author: Pete (cargopete)
- Date: 2026-07-21
- Depends on: RFC-0012 (a nest version *is* a content-addressed bundle), RFC-0019 (the registry that
  resolves `name@version`, and the movable `latest` pointer this layers semantics onto), RFC-0018 §1
  (authored SQL views + `semantic.toml` — the *schema* whose diff decides compatible-vs-breaking),
  RFC-0013 §3 (the sealed content-addressed segments that make cross-version *reuse* — not resync —
  possible).
- Blocks: operator confidence in *updating* a nest at all. Without this, every nest change is a
  subgraph-style resync gamble, and the "be your own indexer" promise stops at v1.
- Nature: design RFC. **The single most differentiating item in the Jul–Aug set** — content-addressed
  immutable segments give us a capability subgraphs structurally cannot have.
- Origin: roadmap thread 4 (`docs/high-level-roadmap-jul-aug-2026.md`); definition settled 2026-07-21.

## Abstract

Subgraphs' worst day: you deploy v2, it resyncs from genesis, you run v1 **and** v2 in parallel burning
double resources until v2 catches up, then flip. This is the **N-1 problem**, and the Foundation calls
it a top operator pain. This RFC fixes it for nests.

The fix rests on one classification and two paths:

- **Compatible update** → the operator indexes the new version, then **hot-swaps it behind the same
  endpoint** when it's caught up. The consumer notices nothing. Where the new version's decode + schema
  are unchanged for already-sealed ranges, it **reuses the existing content-addressed segments** — no
  full resync, the thing subgraphs can't do.
- **Breaking update** → the new version is served on a **new versioned endpoint**, run alongside the old
  one, so downstream app devs migrate on *their* clock; the old endpoint is deprecated afterwards.

## The settled definition (2026-07-21)

The predicate is **backward-compatibility for downstream consumers** (e.g. Lodestar), *not* "zero schema
delta":

- **Compatible** = every existing downstream query/subscription keeps working with unchanged meaning.
  Covers internal-only changes (decode fixes, perf, view refactors yielding identical output) **and
  purely additive** schema changes (a new column/table/view; nothing existing touched). → same endpoint.
- **Breaking** = anything a consumer can observe as **removed, renamed, retyped, or semantically
  changed**. → new endpoint, parallel run, deprecation window.

Additive-is-compatible is deliberate: forcing a new endpoint for a harmless added field would reinstate
the very migration busywork we're abolishing.

## Motivation

- **Updating must stop being scary.** A nest that can't be safely improved is a dead nest. Operators
  need a change to be either invisibly-hot-swapped or cleanly-parallel-versioned — never a resync
  lottery.
- **We uniquely can.** Sealed segments are content-addressed and immutable (RFC-0012/0013). If v2's
  decode + schema for a finalized range are identical to v1's, the *bytes are identical* — so v2 can
  mount v1's segments instead of re-deriving them. Subgraphs re-index because they have no such
  addressable, reusable substrate.
- **It's the payoff of three prior RFCs.** 0012 (identity), 0018 §1 (schema), 0013 §3 (segments) were
  each built for their own reasons; this RFC cashes them in.

## Goals

1. **Classify** an update `vN → vN+1` as compatible or breaking, by diffing the declared schema
   (RFC-0018 §1 views + `semantic.toml`), with a **conservative default: when in doubt, breaking.**
2. **Compatible path**: index vN+1, then atomically re-point the endpoint from vN to vN+1 with zero
   consumer-visible break; **reuse** sealed segments wherever decode+schema match.
3. **Breaking path**: stand up vN+1 on a **new endpoint**, keep vN serving, and provide a deprecation
   lifecycle (mark → warn → sunset) that's operator-controlled, never forced.
4. **Never resync what can be reused.** Segment reuse is the default, full re-derivation the exception,
   and the exception is *stated* (logged), never silent.

## Non-goals

- **Not auto-migrating consumers.** A breaking change means *we* keep the old endpoint alive; the app
  dev migrates their queries. We make migration unhurried, not automatic.
- **Not mutating sealed segments** — ever (founding non-negotiable). Reuse *mounts* existing immutable
  segments; it never rewrites them. A change that would require rewriting a sealed segment is, by
  definition, a new decoding/version producing *new* segments, not an edit of old ones.
- **Not re-decoding history when ABIs improve** — decodings are versioned (existing rule). This RFC
  interacts with that (see §4) but does not change it.

## Design

### §1 — Version identity and the schema diff

A nest *version* is already a content-addressed bundle (RFC-0012). To this we attach the version's
**observable contract**: its schema — the entity/table/view shapes and types surfaced in `/schema`, the
MCP, and `semantic.toml` (RFC-0018 §1). Classification is a diff of that contract:

- **Additive** (new table/column/view; nothing existing removed/renamed/retyped/re-meant) → *compatible*.
- **Any removal / rename / retype / semantic change** → *breaking*.
- Internal-only (no schema delta, decode/view refactor yielding identical output) → *compatible*.

**Detection authority (open):** auto-diff, author-declared (semver-style), or **diff-proposes /
author-confirms** — the recommended default, because a pure auto-diff can't see a *semantic* change that
keeps the shape (e.g. a column's meaning flips), and pure author-declaration is unchecked. The diff
proposes; the author may only *upgrade* the severity (compatible→breaking), never downgrade it. This is
an implementation choice, flagged, not settled here.

### §2 — The compatible path: hot-swap behind one endpoint

1. Operator loads vN+1 (RFC-0019 resolution) while vN keeps serving.
2. vN+1 indexes to the tip, **reusing sealed segments** (§4) so this is fast, not a genesis resync.
3. When vN+1 has caught up and validated, the endpoint's backing is **atomically flipped** vN→vN+1.
4. vN is torn down. The consumer's URL never changed; their queries never broke.

`latest` (RFC-0019's movable pointer) tracks the compatible head; a compatible update advances it and the
served endpoint follows. This is where 0019's raw `latest` gains *meaning*: **compatible-latest**.

### §3 — The breaking path: a new endpoint, parallel, deprecable

1. vN+1 is a *breaking* version → it gets a **new versioned endpoint** (e.g. `/v2/...` or a
   version-qualified nest id), resolved distinctly by RFC-0019.
2. vN keeps serving its endpoint unchanged. Both run; the operator pays for the overlap *by choice*, for
   as long as they choose.
3. A **deprecation lifecycle** on the old endpoint: `active → deprecated (served + warned) → sunset
   (removed)`, operator-driven, surfaced in the admin UI (RFC-0010) and `/schema`. No forced flip.

### §4 — Segment reuse mechanics (the no-resync engine)

For a finalized, sealed range, a segment is a content-addressed function of *(decoded facts, schema
projection)*. vN+1 can **mount vN's segment unchanged** iff, for that range, both the **decode version**
and the **schema projection** are identical. Cases:

- **Schema additive, decode unchanged** → existing segments reused as-is; only the *new* column/view is
  derived (from already-decoded facts) — a cheap forward-fill, not a resync.
- **Decode version bumped** (better ABI) → per the existing "never re-decode history" rule, *history
  keeps its old decoding*; the new decoding applies **going forward**, producing new segments from the
  bump point. Reuse holds for the pre-bump range; the post-bump range is genuinely new. This is the
  subtle interaction §1's classifier must respect: a decode bump is *compatible* for consumers (same
  shape) yet produces new segments prospectively — the two axes (consumer-compat vs segment-reuse) are
  independent, and this RFC keeps them so.
- **Breaking schema** → new endpoint, its own segment lineage; no reuse across the break (nor should
  there be — it's a different contract).

Reuse decisions are **logged** ("reused N segments \[hash…], re-derived M ranges because …") — silent
truncation of reuse would read as "fast upgrade" while quietly resyncing.

## Implementation

- Extend the version resolution (RFC-0019) with a `classify(vN, vN+1) → Compatible | Breaking` over the
  RFC-0018 §1 schema surface.
- Endpoint layer (RFC-0010 serving): an indirection from *endpoint → backing version*, so a compatible
  flip is a pointer swap; a breaking version binds a *new* endpoint.
- Segment mount path (RFC-0013): allow a new version to attach prior versions' catalogued segments when
  `(decode_version, schema_projection)` match for the range.
- Admin UI (RFC-0010): show version, compat status, reuse summary, deprecation state.

## Testing

- **Golden — compatible**: an additive update serves **identical** results for all pre-existing queries
  on the **same** endpoint; the new field appears; no full resync occurs (assert segments reused).
- **Golden — breaking**: a removal/retype spins a **new** endpoint; the old endpoint keeps serving
  identical results; both correct concurrently.
- **Segment-reuse correctness**: a reused-segment upgrade yields **byte-identical** served data to a
  from-scratch re-derivation of the same version (reuse must be indistinguishable from recompute).
- **Decode-bump interaction**: history retains old decoding; new decoding applies forward; pre-bump
  segments reused, post-bump new — asserted explicitly.
- **Conservative-default**: an ambiguous/semantic-only change classifies **breaking**, not compatible.

## Risks

- **Misclassifying breaking as compatible** — the dangerous failure: a hot-swap that silently changes
  consumer-visible meaning. Mitigations: conservative default (doubt → breaking); author may only
  *raise* severity; the golden compatible-path test asserts *identical* results, catching a sneaked
  semantic change.
- **Reuse correctness** — a wrongly-reused segment corrupts served history. Mitigation: reuse keyed on
  `(decode_version, schema_projection)` equality + the byte-identical-vs-recompute test.
- **Endpoint sprawl** — many breaking versions = many live endpoints. Mitigation: operator-driven
  deprecation lifecycle; surfaced, not automatic.

## Alternatives considered

- **Always new endpoint (never hot-swap)** — simplest, but reinstates a migration for *every* change,
  including additive; throws away the whole point. Rejected.
- **Always hot-swap (never a new endpoint)** — breaks downstream consumers on breaking changes. Rejected.
- **Zero-delta = compatible** (stricter) — considered and *declined* 2026-07-21 (additive-is-compatible
  confirmed); it would force needless migrations for added fields.
- **Full resync every upgrade** (the subgraph status quo) — the thing this RFC exists to kill.

## Open questions

- Detection authority (auto-diff / author-declared / propose-confirm) — recommended propose-confirm;
  settle in implementation.
- Deprecation lifecycle policy — default sunset windows, or fully manual?
- Endpoint naming for breaking versions (`/vN` path vs version-qualified nest id) — serving-layer detail.
