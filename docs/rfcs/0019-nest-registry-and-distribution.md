# RFC-0019: The nest registry and distribution — publish, pull, private nests

- Status: **Implemented** (2026-07-21) — all three slices: slice 1 (FsStore + publish/load + refs),
  slice 2 (S3 `ObjectStore` behind `--features object-store`), slice 3 (private nests + auth: 401/403 →
  a clear "private / credential rejected" message, distinct from "not found"). Live MinIO/S3 integration
  verification is a VPS run (in-memory + FS paths are unit-covered).
- Author: Pete (cargopete)
- Date: 2026-07-21
- Depends on: RFC-0012 (content-addressed `.bundle` — the artifact this distributes; identity,
  verification, and the reproduce-check are inherited unchanged), RFC-0001 (the nest as the unit).
- Blocks: RFC-0020 (the upgrade story needs a *source of versions* to resolve `name@version` against),
  RFC-0022 (distributed placement pulls nests from a shared store, not from an operator's laptop),
  RFC-0023 §4 (the hosted verifiable state-cache reuses this exact object-storage + content-addressing
  substrate — one substrate, two payloads).
- Nature: design RFC. Mostly *plumbing over an artifact that already exists* — RFC-0012 made the nest a
  portable content-addressed blob; this gives the blob somewhere to live and a way to be fetched by
  name. The only genuinely new surface is auth for private nests.
- Origin: the Jul–Aug 2026 roadmap, thread 3 (see `docs/high-level-roadmap-jul-aug-2026.md`).

## Abstract

RFC-0012 turned a nest into a single content-addressed `.bundle` and gave us `nest load <file|url|dir>`.
That is the *deploy* unit; it is not yet a *distribution* story. Today you share a nest by handing
someone a file or a URL you happened to host. This RFC gives nests a **registry**: a place to
**publish** them to and **pull** them from by name, with **private nests** behind auth, backed by
**object storage** (or a plain directory on disk).

One load-bearing principle, straight off the founding brief: **the registry is decoupled from the
nuthatch binary, and using one is never mandatory.** nuthatch *pulls*; it never *becomes* the
registry, and it never *requires* one. A bundle you built yourself, loaded from a local path, must
work forever with no registry in the loop — exactly as it does today. The registry is a convenience
for sharing, in the shape of crates.io or an OCI registry: run your own, point at a bucket, or don't
use one at all.

## Motivation

- **Sharing is manual.** "Here's a URL to a `.bundle`" doesn't scale to a catalogue, doesn't version,
  and has no notion of *private*. Operators (GraphOps foremost) want to publish a nest once and have
  others — or their own fleet — pull it by name.
- **The distributed mode needs it (RFC-0022).** A writer pool across machines can't each carry a copy
  of every operator's laptop. Workers pull the nest they're scheduled to run from a shared store. No
  registry, no distributed placement.
- **Private nests are wanted** (roadmap decision, 2026-07-21). A team authors a proprietary nest and
  wants to publish it *privately* — visible only to holders of a credential — and have their own
  nuthatch pull it. This is table stakes for real operators, and it's a day-one requirement here, not
  a later bolt-on.
- **It's the substrate for two other RFCs.** RFC-0020 resolves upgrade versions against it; RFC-0023 §4
  stores verifiable call-result segments in the same bucket. Building the object-storage + addressing
  layer once, cleanly, pays for itself three times.

## Goals

1. `nest publish` — push a `.bundle` to a registry (object storage or disk), indexed by **name and
   version**, resolving to the same content-address RFC-0012 already computes.
2. `nest load <name>@<version>` — resolve a name/version to a bundle hash, fetch the blob, verify it
   against the hash (RFC-0012's check, unchanged), and mount it.
3. **Private nests** — publish access-controlled; pull with a credential. (This is *registry auth*,
   credential kind **(a)** below.)
4. **Decoupled + optional** — the registry is a separate service/spec; nuthatch speaks a client protocol
   to it. No registry is ever required to build, load-from-disk, or run a nest.
5. **Object storage or disk** — an S3-compatible bucket *or* a local directory, behind one trait. The
   embedded, self-hosted-first path is "a folder"; the fleet path is "a bucket."

## Non-goals

- **Not a hosted service, and not `nuthatch`'s job to *be*.** We ship a client and a reference
  registry spec/impl; we do not run a registry for anyone, charge for one, or make ours canonical.
- **No mandatory dependency, no phone-home.** There is no default registry nuthatch calls home to.
  Resolution of a bare `name` requires an explicitly configured registry; there is no implicit one.
- **Not nest *runtime* secrets.** Credential kind **(b)** — the private RPC endpoints and enricher API
  keys a *running* nest needs — is scoped here only enough to state the boundary (§4). Its mechanism is
  shared with RFC-0022 and detailed there.
- **No re-litigation of identity.** The bundle hash, manifest canonicalization, and reproduce-check are
  RFC-0012's and are used verbatim. The registry maps names to those hashes; it does not mint new ones.

## Design

### §1 — The bundle store: object storage or a directory

A single `BundleStore` trait with two implementations:

- **`FsStore`** — a local directory. `publish` writes `<dir>/blobs/<hash>.bundle`; `resolve` reads it
  back. This is the zero-dependency, self-hosted-first default and the thing tests run against.
- **`ObjectStore`** — an S3-compatible bucket (the `object_store` crate; MinIO/R2/S3/GCS). `publish`
  puts the blob at `blobs/<hash>`; `resolve` gets it. "object/S3 no problem" — the notes were right;
  this is the easy half.

Blobs are **immutable and content-addressed** — the store key *is* the RFC-0012 hash. Re-publishing an
identical bundle is a no-op (same key); dedup is free. Nothing in the store is ever mutated, only added.

### §2 — The index: `name@version → hash`

The store holds *blobs by hash*; the **index** maps a human name to a hash. Kept deliberately thin:

```
index/<namespace>/<name>/<version>  →  <hash>
index/<namespace>/<name>/latest     →  <hash>   (a movable pointer, the only mutable thing)
```

The index is itself just objects in the same bucket (or files in the same dir) — no separate database
required for the self-hosted case. A fleet may front it with a real service (RFC-0022's control-plane),
but the on-disk/on-bucket form is the source of truth and works standalone.

**Protocol — open, leaning S3-native.** Three candidates were floored in the roadmap: (i) OCI (reuse
container registries and their auth/CDN for free), (ii) a bespoke HTTP API, (iii) a plain S3
bucket-plus-index. The recommendation is **(iii) as the reference** — it's the least infrastructure for
a self-hoster (a bucket is a registry) — with the client trait shaped so an **OCI** backend can be added
without touching callers, for operators who already run a container registry. This stays an open
implementation question (roadmap §"design work"); the trait boundary is what this RFC commits to.

### §3 — Private nests and registry auth (credential kind **a**)

A namespace may be **private**: its index and blobs are access-controlled by the store. A publisher
presents a credential to write; a puller presents one to read. The mechanism is the *store's* — an S3
bucket uses bucket policy/signed URLs; a directory uses filesystem perms; an OCI backend uses registry
tokens. nuthatch's job is only to (a) carry a configured credential to the store, and (b) fail
*loudly and early* with a "this nest is private / your credential was rejected" message, never a silent
empty result.

Credentials live in nuthatch config / env, **never in a nest or a bundle**. A private *nest* is still a
public-shaped *bundle* (same content-addressing) — "private" is a property of *who can fetch it from the
store*, not of the blob's format. This keeps verification identical for public and private nests.

### §4 — Runtime secrets are a different thing (credential kind **b**) — boundary only

The "private credentials" note actually spans two unrelated concerns, and conflating them is a
security bug waiting to happen:

- **(a) Registry auth** — fetch a private *bundle*. Handled above.
- **(b) Nest runtime secrets** — a *running* nest's private RPC URL, an effectful component's API key.
  These are **injected per-nest, per-worker at mount time** and must **never** be baked into a
  content-addressed bundle (baking them would both leak the secret and break addressing — two nests
  with different keys would hash differently despite being the same nest).

This RFC commits to the *rule* — bundles are secret-free; secrets are mount-time injection — and defers
the injection *mechanism* to RFC-0022, where the control-plane that holds them lives. Stated here so
0019's bundle format is designed secret-free from the first commit.

### Resolution flow (what feeds 0020 and 0022)

```
nest load foo@1.2.0
  → index lookup  foo@1.2.0 → <hash>       (§2; carries auth if private, §3)
  → blob fetch    <hash>                    (§1)
  → verify        blob == <hash>            (RFC-0012, unchanged)
  → mount                                   (RFC-0012 `load`)
```

`foo@latest` resolves the movable pointer. RFC-0020 layers *compatible-vs-breaking* semantics on top of
this same resolution (a breaking version resolves to a *different endpoint*, a compatible one hot-swaps
behind the same one). RFC-0022's scheduler calls exactly this flow on whichever worker it places `foo`.

## Implementation

- New `src/registry.rs` client (distinct from the existing decode `registry.rs` — name TBD, likely
  `src/distribution.rs` to avoid the collision) with the `BundleStore` trait, `FsStore`, and
  `ObjectStore` (feature-gated on `object_store` so the embedded binary needn't pull S3 deps unless
  built with it — footprint discipline).
- CLI: `nest publish <bundle> --registry <url|path> [--as <name@version>]`, and extend `nest load` to
  accept a bare `name@version` (today it takes file/url/dir).
- Config: a `[registry]` block (or repeatable, for multiple registries) with endpoint + optional
  credential reference (env-var name, not the secret inline).
- Reuse RFC-0012's manifest/hash/verify verbatim; add zero new identity code.

## Testing

- **Round-trip** (`FsStore` and a MinIO container): publish → resolve `name@version` → hash matches →
  verify passes → mount produces a byte-identical nest to `load <dir>`.
- **Content-addressing invariants**: re-publishing an identical bundle is a no-op; two different
  versions of a name coexist; `latest` moves without touching historical blobs.
- **Private/auth**: a private namespace rejects an unauthenticated pull *loudly*; a valid credential
  succeeds; a wrong one fails with a clear message, never a silent miss.
- **Offline / degradation**: a locally-cached or on-disk bundle loads with the registry unreachable
  (the no-mandatory-dependency invariant, asserted as a test, not a hope).
- **Secret-free bundles**: a property test that no config-injected secret can appear in a produced
  bundle (kind-(b) leakage guard).

## Risks

- **Mandatory-dependency creep** — the single biggest risk to the founding brief. Mitigation: there is
  no default/implicit registry; `load <dir>` and self-built bundles never touch one; the offline test
  is a CI gate, not a footnote.
- **Secret leakage into bundles** (kind (b)) — mitigated by the secret-free-bundle property test and by
  keeping injection entirely mount-time (RFC-0022).
- **Protocol lock-in** — mitigated by committing to the trait boundary, not the wire format; S3-native
  reference now, OCI addable later without caller changes.
- **`registry.rs` name collision** with the decode registry — trivial, but flagged so it's a conscious
  rename, not an accident.

## Alternatives considered

- **OCI-only** — free auth + CDN, but forces every self-hoster to run/understand a container registry to
  share a nest. Rejected as the *reference* (too much infra for the embedded ethos); kept as an
  *optional backend*.
- **A bespoke registry service with its own DB** — more control, but it's a service to run and the
  antithesis of "a bucket is a registry." Rejected for the reference; a fleet may still front the index
  with the RFC-0022 control-plane.
- **Fold distribution into RFC-0012** — 0012 is the *artifact*; distribution is a separable concern with
  its own auth surface and its own consumers (0020, 0022, 0023). Kept separate deliberately.

## Open questions

- Registry protocol concretely (S3-index vs OCI backend first) — trait now, pick the reference impl in
  implementation.
- Namespacing/naming: flat names, `@scope/name`, or operator-prefixed? (Affects private-namespace auth.)
- Does `latest` belong in the core at all, or only as an RFC-0020 concept (versions/endpoints)? Leaning:
  `latest` is a raw pointer here; *compatible-latest* is 0020's semantic on top.
