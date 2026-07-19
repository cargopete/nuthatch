# Production-readiness checklist

The bar a nuthatch release must clear before it's pointed at someone's real workload, unattended.
Reconciled against [CLAUDE.md](../CLAUDE.md) (non-negotiables + build order), the
[RFC series](rfcs/README.md), the [backlog](backlog.md), and [CI](../.github/workflows/ci.yml) on
**2026-07-19** (repo at `0.4.0`, `0.5` in flight).

This is a *standing* checklist — the target, not a claim it's all done. Status reflects what's
verifiable today. When you cut a release, walk it top to bottom and update the flags with evidence.

## Legend & scope

| Flag | Meaning |
|------|---------|
| ✅ | Done and verified (test, bench artifact, or live run backs it) |
| 🟡 | Partial — exists but incomplete, unverified, or narrow |
| ⛔ | Not started, deferred, or blocked (see "Blocked on") |

**Two production targets, graded separately** — don't conflate them:

- **Embedded / single-chain roost** (the primary deliverable): one binary, one chain, tip-follow +
  serve, `≤2 GB` RAM. This is the thing that can be "prod ready" *now*.
- **Scaled mode** (docker-compose, Postgres + DataFusion federation): greenfield. Nowhere near a
  release, and honestly so — most of its checklist is ⛔ by design, not neglect.

A green embedded column with a red scaled column is a **legitimate ship** — just say which one you're
shipping.

---

## 0. The non-negotiables (gate everything else)

If any of these is ❌ the release does not go out, full stop. These are the CLAUDE.md invariants.

- [ ] ✅ **Single static binary, zero external services in embedded mode.** `curl | sh` → `init` →
  `dev` → live API, no Postgres/Docker/IPFS. — *CI builds the release binary; footprint job runs the
  real `init → dev` path.*
- [ ] ✅ **Footprint ≤ 2 GB RAM** for a single-chain roost, CI-enforced. — *`footprint.sh` gate, 256 MB
  ceiling, measured ~37 MB. **Note the gap:** the CI scenario is `--backfill 200` on one nest. A
  release claim of "≤2 GB" for a *dense multi-nest roost at tip* is not yet measured — see §5.*
- [ ] ✅ **No phone-home.** No telemetry, no mandatory tokens, AI degrades offline. — *Verify per
  release: grep for outbound calls not gated behind explicit user config / BYO-key.*
- [ ] ✅ **Determinism in the core.** Decode, reorg, entity derivation re-executable; no LLM output in
  the runtime data path. — *Golden tests + the RFC-0016/0017 hard fence. Re-assert on any new
  data-path code.*
- [ ] ✅ **Licence hygiene (AGPL-3.0).** No AGPL-we-don't-own ports (SQD worker-rs), no Materialize
  (BSL), no Envio/HyperSync dep. — *`cargo tree` audit each release; deps stay in the CLAUDE.md
  safe-list.*

---

## 1. Correctness & determinism

- [ ] ✅ Deterministic decode: topic0-keyed, contract-ABI priority with generic fallback. *(RFC-0001)*
- [ ] ✅ ABI acquisition Sourcify → Etherscan-class, cached locally.
- [ ] ✅ Decodings are **versioned**; no retroactive re-decode of stored history when ABIs improve.
- [ ] ✅ Golden/deterministic tests per handler and view (fixed fixtures in → exact state out).
- [ ] ✅ Property tests: random reorg depths converge to canonical state (`e2e_reorg.rs`).
- [ ] ✅ Nest invariant/parity checks (`nuthatch check`) run hermetically in CI against committed
  fixtures. *(RFC-0002 §5)*
- [ ] 🟡 **Sustained** byte-identical multi-nest-vs-solo table parity over a *long* range. — *Short
  runs pass; the long sustained run is the one outstanding 0012 acceptance item. **Blocked on:** a
  live run (public RPC fine).*
- [ ] 🟡 Factory / dynamic-contract discovery correctness at scale. — *Implemented (0009); child
  `end`/expiry conditions and wildcard-address decode still open.*

## 2. Reliability, reorgs & crash safety

- [ ] ✅ Reorgs only ever touch the mutable hot store; sealed Parquet is append-only past finality.
- [ ] ✅ Atomic seal/prune (no torn segment on crash mid-seal). *(0.4.0 hardening)*
- [ ] ✅ Crash-safety e2e (`e2e_crash_safety.rs`): kill mid-index, restart, converge.
- [ ] ✅ Single-writer discipline: only the ingestion thread writes DuckDB/redb; queries attach
  read-only. No concurrent-writer design anywhere.
- [ ] ✅ Single cursor / single process / one observable failure boundary. A second chain = a second
  process (never multiplex chains behind one cursor).
- [ ] ✅ Per-nest blast-radius isolation in a roost: one nest's bad view / runaway factory can't harm
  another. *(RFC-0012)*
- [ ] ✅ Graceful recovery from a corrupt/partial segment on startup (detect + quarantine + resume
  rather than crash-loop). — *0.5.x: `seal::verify_and_quarantine` runs at startup — each manifest
  segment is hash-verified against its content address; a corrupt/tampered/unreadable one is moved to a
  sibling `quarantine/` dir with a loud error, and `define_views` skips any missing file, so one bad
  segment reduces a table's cold data instead of failing every `/sql`. Fixtures: `seal.rs`
  quarantine test + `analytics.rs` query-survives-missing-segment test.*
- [ ] 🟡 RPC-provider failure handling: dead-provider failover, honest stall reporting under sustained
  provider flakiness. — *Failover is now **health-aware** (0.5.x): a failed endpoint enters a 30 s
  cooldown and is skipped, so a dead provider no longer costs a request-timeout on every round-robin
  hit. The tip loop already retries the same window (no silent gaps). Remaining: a **loud stall signal**
  (metric + escalating log) when all endpoints are down — lands with the health endpoint (§7).*

## 3. Performance & footprint budgets

Benchmarks are **CI artifacts**, not vibes — every published number traces to a `bench-report.json`
with date/provider/hardware/commit (the RFC-0004 house rule).

- [ ] ✅ Backfill throughput bench exists and is reproducible (`nuthatch bench backfill`). — *Floor
  ≥10K events/sec, aim 30K.*
- [ ] 🟡 A **published, current** backfill number for the release commit on reference hardware. —
  *Re-run per release; don't ship a stale figure.*
- [ ] ⛔ Tip-lag benchmark (notification → row queryable) as a tracked number. — *Meaningful number
  needs ExEx. **Blocked on:** reth node (0003).*
- [ ] 🟡 Entity point-read p50/p99 bench tracked across releases (regressions fail the build).
- [ ] 🟡 Peak-RSS regression gate wired for the **dense multi-nest** scenario, not just single-nest
  `--backfill 200`. — *The 2 GB claim for a real roost density is unmeasured; §0 note.*
- [ ] ✅ Regressions fail the build (benchmarks-as-gates principle established). — *Extend coverage as
  the benches above land.*

## 4. Security

- [ ] ✅ Blob-mount RCE fixed (0.4.0 critical).
- [ ] ✅ `/sql` arbitrary file-read fixed (0.4.0 critical).
- [ ] ✅ `/sql` surface is structurally read-only (single-writer + read-only attach).
- [ ] ✅ A security review pass on the **serving surface** (`serve.rs`, `mcp.rs`, `webhooks.rs`,
  `analytics.rs`, `abi.rs`, `rpc.rs`) — *done (0.5.x hardening): no criticals; SQL read-only gate holds
  three-deep, no SSRF (ABI/RPC hosts are fixed constants), no file-read via `/sql`. Fixed: `/nest`
  webhook-URL disclosure, `/sql` error path-scrub, `screen_status` quote-escape, constant-time admin
  token, concurrent webhook delivery. Re-run per release on the diff.*
- [ ] 🟡 Bind/exposure defaults are safe. — *`dev` binds `127.0.0.1` by default; off-localhost it warns
  loudly that the data surface has NO auth (the gateway's job). Confirmed by the review; the one control
  a fronting gateway must enforce is auth on **every** route, not just `/_admin`.*
- [ ] ✅ Dependency vulnerability scan (`cargo deny`) wired into CI. — *`deny` job runs advisories +
  licences + bans + sources against `deny.toml`; the AGPL-compatible licence gate is now enforced. Three
  transitive advisories ignored with written rationale (quick-xml not-reachable ×2; wasmtime-wasi
  FilePerms tracked for a runtime bump).*
- [ ] ✅ Effectful (capability-granted) components can only produce **annotations**, never canonical
  entities — purity checkable from the composition manifest. *(transform layer)*

## 5. The ≤2 GB budget under realistic load

Called out separately because it's the headline promise and the current gate only exercises the easy
case.

- [ ] ✅ Single nest, backfill, single chain: measured ~37 MB, gated at 256 MB.
- [ ] ⛔ Multiple nests co-located in one roost at tip, sustained, measured against 2 GB.
- [ ] ⛔ Large-ABI / high-event-rate contract at tip (memory doesn't grow unbounded with hot-store
  churn).
- [ ] ⛔ Long-running soak (24h+) with no RSS creep (leak check).

## 6. Testing & CI gates

- [ ] ✅ `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test --locked` on every
  PR + main.
- [ ] ✅ Release binary builds `--locked`; footprint gate runs against the built artifact.
- [ ] ✅ e2e harness exists (`TapeSource`) and covers solo, reorg, crash-safety, roost parity.
- [ ] 🟡 MSRV is honest. — *`Cargo.toml` declares `rust-version = 1.85`, but CI pins the toolchain to
  `1.95.0`. Either test against 1.85 in CI or bump the declared MSRV; right now the claim is
  unverified. (Also the DataFusion-48/arrow-56 MSRV tension noted in the backlog.)*
- [ ] 🟡 Coverage of the AI/MCP surface (schema discovery, SQL exec, entity lookup, subscribe) with
  the RFC-0016 eval harness. — *S1 eval harness gates the semantic-layer work; wire it in.*
- [ ] 🟡 `--offline` / no-network test path proving AI features degrade gracefully.

## 7. Operability & observability

- [ ] ✅ Metrics surface exists (`metrics.rs`).
- [ ] 🟡 Health/readiness endpoint suitable for a supervisor (is dev live? is tip fresh? is it
  stalled?).
- [ ] 🟡 Structured logs at a sane default level; a clear "we are behind / we are at tip" signal.
- [ ] 🟡 Documented restart/recovery runbook and a backup/restore story for the redb hot store +
  sealed segments. — *[operators.md](operators.md) is the home for this; confirm it's complete.*
- [ ] ⛔ SSE **push** for live status (status page polls today). *(0010 small increment)*
- [ ] 🟡 Alerting hooks (`alerts.rs`, `webhooks.rs`) documented end-to-end with a runnable example.

## 8. Release engineering

- [ ] ✅ Versioning + release workflow in place (`release.yml`), reproducible `--locked` builds.
  *(RFC-0005)*
- [ ] ✅ `curl | sh` install path.
- [ ] 🟡 Cross-platform release matrix — which targets are built/tested? (Linux x86_64 is the CI host;
  macOS/arm64 install claims should be tested or scoped.)
- [ ] 🟡 CHANGELOG / release notes discipline per tag (the progress-log is close; formalise for
  consumers).
- [ ] 🟡 Documented upgrade path / on-disk format stability guarantee across `0.x` bumps (does a
  `0.4 → 0.5` upgrade preserve existing sealed segments and hot store?).

## 9. AI-native surface (MCP)

- [ ] ✅ MCP server compiled into the binary (`mcp.rs`), works offline against the local instance.
- [ ] ✅ `init` scaffolds schema + views + handlers + tests from the ABI.
- [ ] ✅ Ships `llms.txt` / docs-as-MCP / `.claude/skills/` in scaffolded projects.
- [ ] 🟡 The RFC-0016 governed semantic layer (`semantic.toml`, enriched `schema`, errors-as-prompts,
  `explain`) — *in design, measure-first, not shipped.*
- [ ] 🟡 The RFC-0017 builder skill with CI-checked CLI/config reference drift. — *in design.*

## 10. Docs & first-run UX

- [ ] ✅ `<2 minute` first-indexed-query demo path (`init → dev → sql`).
- [ ] ✅ Terminal-native query REPL (`nuthatch sql`). *(RFC-0015 slice 1)*
- [ ] ✅ Operator docs, factory docs, benchmark docs present.
- [ ] 🟡 A single "here's how you run this in production, unattended" guide that ties together
  §7 (ops), §4 (safe exposure), and §8 (upgrades). — *This checklist's operational cousin; write it
  when the 🟡s above go green.*

---

## 11. Scaled mode (graded on its own — mostly ⛔ by design)

Nothing here blocks an **embedded** release. It blocks calling *scaled mode* production-ready.

- [ ] ⛔ Postgres hot store behind the `HotStore` trait (no `#[cfg]` forks of business logic).
- [ ] ⛔ DataFusion federation across hot + cold behind one SQL surface. — *0013 §2/§4,
  **benchmark-gated**; build scaled-side first.*
- [ ] ⛔ `nuthatch bench` DuckDB-vs-DataFusion spike (latency + RSS within budget) before retiring
  DuckDB.
- [ ] ⛔ Golden SQL-compat suite across both engines.
- [ ] ⛔ docker-compose deployment story tested end-to-end. *(binary + compose only — no k8s/Helm,
  per CLAUDE.md out-of-scope.)*

## 12. Infra-gated capabilities (the shared blocker)

Almost everything un-buildable-on-a-laptop traces to one missing box.

- [ ] ⛔ **Colocated reth node** (full for tip, archive for deep backfill/traces). — *Provisioning +
  days of sync; hardware/ops, not code. Gates the two below.*
- [ ] ⛔ ExEx tip mode wired to a real node; `nuthatch-node` binary; honest tip-latency number. *(0003;
  groundwork in, **blocked on** the node.)*
- [ ] ⛔ Firehose-class extraction (traces + state diffs), own-node/ExEx only. *(0014; **blocked on**
  0003.)* — *One node-independent slice is buildable now and forward-compatible: the calldata decoder,
  `[extract]` config, `traces`/`state_diffs` schemas, and the unbounded-volume guard.*

---

## Bottom line

**Embedded, single-chain, single-nest:** the core (§0–§2, §6) is genuinely strong — this is the
column that can go to `1.0` first. The honest gaps before you'd point a stranger's workload at it
unattended are the operational and load ones: the **dense-roost RAM proof** (§5), the **sustained
parity run** (§1), **provider-failure resilience** (§2), **safe-exposure defaults + a security pass on
the serving surface** (§4), and an **unattended-operation runbook** (§7, §10).

**Scaled mode and anything node-gated (§11, §12):** not production-ready, and correctly deferred — the
project's "build only what we can verify live" discipline is why. Don't let a red column here read as
failure; it's scope, clearly fenced.

Ship the column you can defend, and name it.
