# RFC-0005: Release engineering — v0.1.0

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: RFC-0001, RFC-0002 (release criteria); RFC-0004 (published numbers ship
  in the release notes)
- Blocks: RFC-0007 (launch requires an installable release)

## Abstract

Turn the repo into an installable product: tagged v0.1.0, prebuilt signed binaries for
the major platforms, a real `install.sh`, a Homebrew tap, a published crate, and release
notes generated from the progress log. After this RFC, the website's install command is
end-to-end true for a stranger on a fresh machine.

## Motivation

`curl | sh` currently hits a placeholder that exits 1. Every downstream goal — launch,
grants, the five conversations — requires that a stranger can install and run Nuthatch
without cloning and building. Release engineering is also where the single-binary claim
meets reality: static linking with a bundled C++ DuckDB is the one genuinely fiddly
part, and it needs to be solved once, in CI, forever.

## Release criteria for v0.1.0 (the definition of done)

1. RFC-0001 implemented (multi-contract, full-ABI decode).
2. RFC-0002 implemented through parity (Horizon nest published, parity checks green).
3. CI green including the RAM-budget gate; README/site claims consistent with the
   binary (Task-6-style honesty sweep re-run at tag time).
4. This RFC's artifacts all produced by the release workflow, not by hand.

Explicitly NOT required: RFC-0003 (ExEx ships in a 0.2.x when the soak completes),
RFC-0004 optimizations (baseline numbers suffice; optimizations land incrementally).

## Design

### 1. Version and branch policy

SemVer 0.x: breaking changes bump minor, everything else patch. `main` is always
releasable; releases are tags (`v0.1.0`) cut from main; no release branches at this
scale. The nest toml carries `schema_version` (RFC-0001) — binary refuses newer schema
than it knows, migrates older where trivial.

### 2. Build matrix and the DuckDB static-linking question

Targets, in priority order:

| Target | Notes |
|---|---|
| x86_64-unknown-linux-gnu | primary (Hetzner-class servers); glibc ≥ 2.31 floor documented |
| aarch64-apple-darwin | primary dev platform |
| aarch64-unknown-linux-gnu | ARM servers/homelab (RPi 5-class) |
| x86_64-apple-darwin | best-effort |
| x86_64-unknown-linux-musl | **deferred** — see below |
| Windows | explicitly unsupported in 0.1 (WSL2 documented) |

**musl decision:** duckdb-rs's bundled build compiles a C++ engine; fully-static musl +
libstdc++ builds are achievable but notoriously fragile (exception handling, atomics).
Decision: ship gnu binaries in 0.1 with the glibc floor stated, keep a tracking issue
for musl, and note that `cargo install` covers exotic platforms. The single-binary
claim is about *runtime shape* (no services), not about libc linkage — say exactly that
in the FAQ rather than fighting the toolchain now.

Build hygiene: `--locked`, `codegen-units = 1`, `lto = "thin"`, `strip = true` for
release profile; record `rustc -V` and the lockfile hash in the release notes
(reproducibility statement, not full reproducible-builds — honest about the gap).

### 3. Signing and verification

- `SHA256SUMS` over all artifacts.
- **minisign** signature over SHA256SUMS (chosen over GPG: one keypair, tiny tooling,
  trivially verifiable in install.sh; publish the public key in the repo, on the site's
  /install page, and pinned in a GitHub gist as a second channel).
- install.sh verifies checksum always; verifies signature when minisign is present,
  prints how to install it when not (never silently skips a *failed* verification —
  absence of tooling warns, mismatch aborts).

### 4. install.sh (the real one)

Replaces the placeholder in the site repo. Behavior: detect OS/arch → resolve latest
release via GitHub API → download artifact + SHA256SUMS + .minisig → verify →
install to `~/.local/bin` (or `$NUTHATCH_INSTALL_DIR`), never sudo by default →
PATH check with copy-pasteable fix per shell → print the three-command quickstart.
Idempotent re-runs upgrade in place. `set -euo pipefail`, works under `sh` (dash),
under 200 lines, shellcheck-clean in CI.

### 5. Distribution channels

- **GitHub Releases**: canonical. Artifacts named
  `nuthatch-v0.1.0-<target>.tar.gz` (binary + LICENSE + README excerpt).
- **crates.io**: publish for real (the 0.0.1 name reservation exists); `cargo install
  nuthatch` must build clean with `--locked` on stable. Verify the packaged crate
  excludes fixtures/segments (crate size < 5 MB source).
- **Homebrew**: `cargopete/homebrew-tap` with a bottle-less formula pointing at release
  artifacts (arm64 + x86_64 darwin, linux). Formula update automated in the release
  workflow (commit to tap repo with the new version/hashes).
- Not yet: apt/AUR/nix (community-contributable once the tap pattern exists; a
  `flake.nix` is cheap and may ship opportunistically).

### 6. Release workflow (CI)

`release.yml` on tag push: build matrix → tests on each target (at minimum, run
`nuthatch --version` and the golden decode tests on the produced binary) → checksums →
minisign (key via GitHub OIDC-gated secret) → draft GitHub Release with generated
notes → publish crate (`--locked`) → bump Homebrew formula → smoke-test install.sh
against the draft release from a clean container → promote draft to published.
Any step failing leaves a draft, never a partial public release.

Release notes: generated from the README progress-log entries since the previous tag,
plus a "measured numbers" table (RAM, backfill baseline from RFC-0004, parity status)
and an explicit "known limits" section (carried from progress-log deferred items).
Honesty is a release artifact.

### 7. Website updates at release

/install page: real per-platform instructions, the minisign public key, the glibc
floor, and the WSL2 note. Hero command unchanged (it now works). Add a version badge
sourced from GitHub releases (build-time fetch, keeping the zero-third-party-runtime
rule).

## Implementation plan

1. Release profile + target builds locally (surface the DuckDB/gnu issues early);
   document the glibc floor.
2. install.sh + shellcheck CI + container smoke test.
3. minisign keygen (offline, backed up), public key published in three places.
4. release.yml end-to-end against a `v0.1.0-rc.1` pre-release tag on a scratch repo
   fork or with `prerelease: true` — full rehearsal including tap bump and install.sh
   smoke, before the real tag.
5. Cut v0.1.0 when release criteria are met; verify a fresh-machine install (a friend
   or a clean Hetzner cloud instance) completes quickstart unaided.

## Testing and acceptance

- Fresh Ubuntu 22.04 container and a clean macOS machine: `curl | sh` → quickstart →
  live API, no interventions.
- Tampered-artifact test: modified tarball fails checksum; modified SHA256SUMS fails
  signature; install aborts loudly in both.
- `cargo install nuthatch --locked` succeeds on stable, x86_64 linux + arm64 mac.
- Release workflow rehearsal (rc tag) completes every step including rollback-safety
  (delete draft, re-run, no duplicated tap commits).

## Risks

- **DuckDB bundled-build breakage across targets** — the reason target builds are step
  1, not step 4. If aarch64-linux fights back, ship it as `cargo install`-only in 0.1
  rather than delaying the tag.
- **Key management for minisign** — single maintainer, single key: offline backup
  (paper + drive), and the gist second-channel limits the damage of a site compromise.
- **Tap automation writing to a second repo** — scope the token to the tap repo only.

## Alternatives considered

- **cargo-dist** for the whole pipeline: strong option and close to this design;
  evaluated first in implementation step 1 — adopt it if it handles the DuckDB targets
  cleanly (this RFC then shrinks to configuration + signing policy). The RFC specifies
  the required behavior either way.
- **GPG instead of minisign**: heavier UX for verifiers, keyserver rot; rejected.
- **Docker image as a primary channel**: contradicts the single-binary story as the
  lead; ship an image later for the scaled mode, not for 0.1.

## Open questions

1. Version the nest format independently of the binary (nest `schema_version` already
   exists) — publish a compatibility table from 0.2 onward?
2. Auto-update check (an explicit `nuthatch self-update`, never a background check —
   phone-home rule) — post-0.1, opt-in command only.
