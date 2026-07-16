# RFC-0005: Release engineering — v0.1.0

- Status: Draft (v2)
- Author: Pete (cargopete)
- Date: 2026-07-16 (v1: 2026-07-14)
- Depends on: RFC-0001 (Implemented), RFC-0002 (Implemented; Horizon parity fixtures
  outstanding), RFC-0004 (Implemented: baseline + seal-direct + pipelined)
- Blocks: RFC-0007 (launch), the GraphOps pilot (see §Operator channel)
- Revision note: v2 adds the operator channel (GraphOps partnership, 2026-07-16
  conversation), promotes the OCI image from "later" to first-class, adds operator
  guards (query limits, metrics, config-stability contract), adds Base to the release
  criteria, and syncs criteria to shipped code.

## Abstract

Turn the repo into an installable product for two audiences that now demonstrably
exist: individuals (single signed binary, `curl | sh`, Homebrew, crates.io) and
**operators** (a versioned OCI image, operational guards, and stability contracts —
GraphOps intends to run Nuthatch as part of its data-service platform). Tagged v0.1.0,
with a v0.1.0-rc.1 that doubles as the GraphOps pilot artifact. After this RFC, the
website's install command is end-to-end true for a stranger, and a fleet operator can
deploy, monitor, and upgrade Nuthatch without reading the source.

## Motivation

Unchanged for individuals: `curl | sh` hits a placeholder; every downstream goal needs
a real install. New since v1: GraphOps (8,000 physical cores, launching a data-service
platform on Ethereum/Arbitrum/Base) proposes running Nuthatch as a hosted offering with
revshare — the strongest external validation to date, and it converts "release
engineering" from a courtesy to strangers into a contract with an operator. Operators
cannot build a service on `cargo build --release` from main. What they need is small
and well-understood: tagged artifacts, an image, metrics, guardrails, and a promise
about what won't break between versions.

## Release criteria for v0.1.0 (definition of done — synced to code state)

1. ~~RFC-0001 implemented~~ **DONE** (multi-contract, full-ABI decode, proxy
   resolution, shipped 2026-07-15).
2. RFC-0002: Horizon nest published as its own repo (`init --from` target) **and**
   parity fixtures recorded against the deployed community subgraph at a pinned block
   (`nuthatch check` framework exists; the subgraph-sourced fixtures are the remaining
   work).
3. ~~CI green including the RAM-budget gate~~ **DONE**; honesty sweep (README/site
   claims vs binary) re-run at tag time.
4. **NEW: Base chain registry entry** (chain 8453, OP-stack; `finalized`-tag policy as
   on Arbitrum). Rationale: completes GraphOps's launch matrix (Ethereum, Arbitrum
   One, Base) and is an afternoon of registry work under the RFC-0002 §1 design.
5. This RFC's artifacts all produced by the release workflow, not by hand.
6. **NEW: the operator surface of §Operator channel shipped** (image, /metrics, query
   guards, stability statement).

Explicitly NOT required: RFC-0003 ExEx wiring (0.2.x after the node soak; both
toolchain blockers already cleared), further RFC-0004 optimization (measured ~20×
suffices; adaptive chunker lands incrementally), RFC-0008 compliance slices.

## Design

### 1. Version and branch policy

Unchanged from v1 (SemVer 0.x, main always releasable, tags from main, nest
`schema_version` guard) with one addition: **v0.1.0-rc.1 is a real, published
pre-release** — it is the artifact GraphOps pilots against, so the rc gets the full
workflow (signing, image, notes), not a shortcut. Feedback from the pilot may produce
rc.2; the public v0.1.0 tag follows the pilot's first green week.

### 2. Build matrix and the DuckDB static-linking question

Unchanged from v1 (gnu targets with documented glibc floor; musl deferred with the
"runtime shape, not libc linkage" FAQ line; aarch64/x86_64 linux + darwin; Windows =
WSL2). One addition: the linux/amd64 and linux/arm64 builds feed the OCI image (§5a).

### 3. Signing and verification

Unchanged from v1 (SHA256SUMS + minisign, key published in three places, install.sh
verifies always / aborts on mismatch). Addition: the OCI image is signed with cosign
(keyless, GitHub OIDC) — operators verify provenance with one command; document both
verification paths side by side.

### 4. install.sh (the real one)

Unchanged from v1.

### 5. Distribution channels

- **GitHub Releases** — canonical, unchanged.
- **crates.io / Homebrew** — unchanged.
- **5a. NEW: OCI image, first-class.** `ghcr.io/cargopete/nuthatch:{version}`,
  multi-arch (amd64/arm64), distroless-or-scratch base + the static-ish binary, image
  runs as non-root, data dir at a declared volume path, config via mounted
  `nuthatch.toml` + env overrides for the operator-relevant knobs (listen addr, RPC
  endpoints, guards). The v1 decision ("Docker image later — contradicts the
  single-binary story") is **revised, not reversed**: the binary remains the lead
  story and the only path the website hero shows; the image is the operator story and
  is exactly the same binary in a box — say precisely that in the docs. (Prior art:
  gib ships to GHCR the same way.)
- Not yet: apt/AUR/nix — unchanged.

### 6. NEW: Operator channel (the GraphOps-shaped requirements)

What Nuthatch itself ships so an operator can run it as a service. The dividing line,
stated once and kept: **gateways, auth, metering, and multi-tenancy are the operator's
layer** (that is literally GraphOps's product); Nuthatch ships the guards and signals
that make fronting it safe and billable. Nothing here adds phone-home, accounts, or
tenancy to the binary.

- **/metrics (Prometheus)**: tip height + lag, sealed watermark, rows decoded/sealed,
  per-endpoint request counts + latencies, /sql query counts + durations + rejections,
  RSS gauge, outbox/webhook gauges when RFC-0008 C5 lands. This is the endpoint an
  operator bills and alerts against.
- **Query guards (config, off-by-default-permissive locally, documented for public
  fronting)**: `/sql` statement timeout (DuckDB interrupt), max result rows/bytes, max
  concurrent analytical queries (semaphore), request body cap. These answer the DoS
  concern raised in the GraphOps conversation without Nuthatch growing an auth system:
  the operator's gateway decides *who*; the guards bound *how much*.
- **Bind posture**: unchanged rule (default 127.0.0.1); when bound publicly, startup
  logs a loud one-liner pointing at the guards doc. No token system in core beyond
  what already exists — fronting is the operator's job.
- **Config-stability contract**: `nuthatch.toml` keys and the nest `schema_version`
  get a written deprecation policy — a key removed in 0.(n+1) must warn in 0.n. Same
  for the data layout: redb tables, segment layout, `manifest.json`, `schema.json`
  are versioned; an upgrade note per release states "in-place safe" or "reseal
  required" (target: always in-place within 0.x; say so, then keep it true).
- **Fleet ergonomics**: clean shutdown on SIGTERM (finish the in-flight window, flush,
  exit 0) verified by test; exit codes documented; logs structured (JSON option) for
  aggregation.

### 7. Release workflow (CI)

v1 pipeline unchanged, plus: buildx multi-arch image → cosign sign → push to GHCR →
image smoke test (run container, `--version`, quickstart against a mock RPC fixture) —
all before the draft release is promoted. Release notes gain an "operator notes"
section (upgrade safety, config changes, guard defaults).

### 8. Website updates at release

Unchanged from v1, plus /install gains an "Operators" tab: image pull + verify
commands, the guards doc link, and one honest sentence that hosted Nuthatch offerings
are run by independent operators — the binary neither knows nor cares.

## Implementation plan

1. Base chain registry entry (criteria #4) — do first, it's small and unblocks pilot
   conversations concretely.
2. Target builds + glibc floor (v1 step 1) and the Dockerfile/image build alongside.
3. Query guards + /metrics + SIGTERM handling (§6) with tests.
4. install.sh + shellcheck + container smoke (v1 step 2), minisign + cosign keys.
5. release.yml end-to-end rehearsal with `v0.1.0-rc.1` (`prerelease: true`) — this rc
   is handed to GraphOps as the pilot artifact.
6. Horizon parity fixtures (criteria #2) in parallel; cut v0.1.0 after the pilot's
   first green week and a fresh-machine stranger install.

## Testing and acceptance

All v1 acceptance items unchanged, plus: image runs the quickstart on amd64 and arm64;
cosign verification documented and tested; a `/sql` query exceeding the timeout is
interrupted and counted in /metrics; SIGTERM mid-backfill exits cleanly and a restart
resumes without gaps (extends the existing checkpoint tests); config deprecation
warning fires on a renamed key fixture.

## Risks

v1 risks unchanged (DuckDB targets, minisign key custody, tap token scope), plus:
- **Operator-driven scope pull**: the partnership will generate feature asks. The
  dividing line in §6 is the shield — auth/metering/tenancy requests are gateway-layer
  and get declined from core with a pointer to that paragraph. Roadmap input from
  GraphOps: welcomed; roadmap veto: no (mirrors RFC-0006's funder rule).
- **Pilot timeline coupling**: GraphOps's platform launch date is theirs, not ours.
  The rc ships when our criteria pass; the pilot consumes it when they're ready — no
  criteria are relaxed to hit an external date.

## Alternatives considered

v1 items stand (cargo-dist evaluation first; minisign over GPG). Revised: "Docker
image as a primary channel — rejected" becomes "image as a first-class *secondary*
channel" per §5a; the reasoning that it must not lead the story is retained.

## Open questions

1. v1 Q1/Q2 stand (nest-format compat table; opt-in `self-update`).
2. Support expectations for operators: best-effort via Discussions in 0.x, with the
   revshare conversation (RFC-0006 v2) as the venue for anything firmer. Do not
   promise SLAs in release notes.
3. Should the rc be public or a private pre-release shared with GraphOps? Leaning
   public (`prerelease: true` visible) — consistent with everything else this project
   does in the open.
