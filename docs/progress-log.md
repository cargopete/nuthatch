# Progress log

Newest first. One entry per push, tracking the [build order](CLAUDE.md#build-order-vertical-slices-each-ends-runnable).

- **2026-07-17 - RFC-0009 step 5: the address-list → topic0 filter flip at scale.** A factory
  backfill's address filter grows with every discovered child; providers cap address-list size, so
  above **~500 children** (or a per-template `filter = "topic0"` override) the backfill **flips** to a
  single topic0-only fetch + local registry-lookup filtering (`decode_window` already routes each log
  by contract/child membership), dropping the address-list two-pass entirely - logged once when it
  flips. The flip is **byte-identical**: a new assertion proves topic0-flip mode seals the exact same
  segments as address-list mode (the flip changes only the *fetch strategy*, never the output) - which
  is also why flip mode composes with a pipelined backfill for free (topic0-only filters are
  version-independent). New `Template.filter` config + `FactorySet::force_topic0()`. 105 tests green,
  clippy clean. *(Step 5a - ExEx-mode factories - is inherent: ExEx logs arrive in-process with no
  getLogs filter, so they route through the same `decode_window` local filter; an explicit bridge-
  harness test is a small follow-up under the `exex` feature.)*

- **2026-07-17 - RFC-0009 step 4: reorg convergence for the discovered set + registry snapshot in the
  seal manifest.** Two correctness guarantees for factories. (1) A **reorg convergence property test**
  (`child_registry_reorg_converges`, 96 cases): discovering pools along a prefix chain, reorging at a
  random fork, then applying an alternate branch yields the exact registry content-hash of building the
  winning chain directly - the same convergence the hot store has, now for the discovered-child set. (2)
  Every sealed segment now records the **`registry_snapshot`** - the child registry's content hash at
  seal time - in its manifest entry (new `Segment.registry_snapshot`, `seal_range_with_snapshot`; wired
  through the factory backfill and the tip-path `maybe_seal`; `None` and omitted for a static nest, so
  pre-RFC-0009 manifests still parse). So a factory segment records exactly which discovered set
  produced it, making its child rows reproducible. 105 tests green (+1), clippy clean.

- **2026-07-17 - RFC-0009 step 3a: factory backfill determinism (byte-identical) + honest pipelining
  call.** The RFC plans a `filter_version` + supplemental-fetch machine to pipeline factory backfills,
  but its own Risks section says ship the simple correct thing first - the child-event *bulk* is
  inherently sequential until the step-5 topic0-flip makes filters version-independent, so pipelining
  below the flip buys little. So factory backfill runs **sequentially regardless of `--concurrency`**
  (logged when >1), and step 3a instead pins the guarantee that matters: a byte-identical determinism
  test proving the same range over the same chain history seals **identical content-address hashes**
  (two pools, interleaved swaps, run twice → identical segment signatures). The filter-version pipeline
  is deferred to the step-5 flip. 104 tests green (+1), clippy clean.

- **2026-07-17 - RFC-0009 step 3: factories over history (sequential two-pass backfill).** Factories
  now work in `--seal-direct` backfill, not just the tip. New `backfill_direct_factory`: per chunk,
  pass 1 fetches with the current address filter (base contracts + children discovered so far) and
  updates the child registry from the factory events it decodes; a pass-2 fixpoint loop re-fetches the
  same range for *only* the newly discovered children (children-only, so nested factories within one
  chunk resolve too); all logs are then decoded together with the full registry, stamped, sorted by
  `(block, log_index)`, and sealed - deterministic segments (the byte-identical property step 3a will
  pin against the pipelined path). Uses the efficient address filter, not the tip loop's topic0-only
  fetch. Seal-direct is re-enabled for factory nests (routing to this sequential path; the pipelined
  path composes in step 3a). **Gate met:** hermetic two-pass test (a pool created in pass 1, its
  historical swap fetched + sealed in pass 2, queryable). Verified **live on Uniswap V3**: a
  seal-direct backfill sealed 104 pools and **106 historical swaps across 65 discovered pools**. 103
  tests green (+1), clippy clean.
- **2026-07-16 - RFC-0009 step 2: factory tip regime - it indexes Uniswap.** Dynamic child contracts
  are now decoded live. The decode registry gains **template decoders** (`registry.rs`: topic0-keyed,
  address-agnostic, matched against the runtime child registry not a fixed address; `decode_child`,
  `topic0s`/`tables`/`schema`/`registry_hash` all include templates). A factory nest fetches
  **topic0-only** (empty address filter - `get_logs` now omits the field when empty), so a child
  created *and* active in the same block is already in hand: the tip loop decodes each window in chain
  order (`decode_window`), routing each log to a contract decoder or a discovered child's template
  decoder, and **discovering new children inline** so same-window child activity decodes with no extra
  RPC. Reorg drops children whose factory event rolled back; a warm restart rebuilds the child registry
  by folding stored factory events (`rebuild_children` - a pure fold, determinism preserved).
  `--seal-direct` is disabled for factory nests until the efficient factory backfill lands (steps 3–4).
  **Gate met:** hermetic same-block test (factory `PoolCreated` at log 0 → pool `Swap` at log 1 in one
  window → both decoded, child discovered + reorg-rolled-back). Verified **live on Uniswap V3**: 44
  pools discovered from real `PoolCreated` events, **45 child swaps decoded** into `pool__swap` (a
  discovered pool's swaps routed to the shared template table). 102 tests green (+1), clippy clean.
- **2026-07-16 - RFC-0009 step 1: factory schema + the dynamic child registry.** The foundation for
  Uniswap-class dynamic contract discovery (a factory event announces a child contract, indexed under a
  template). New `[[templates]]` (name + ABI) and `[[factories]]` (`watch`/`event`/`child_param`/
  `template`/optional `start`) config sections, validated: references must resolve, a template can't
  collide with a contract alias, and nesting stays within the depth-3 ceiling (validated on
  `init --from`). New `src/factory.rs`: `FactorySet::build` (validated rules) + `discover(row)` (the
  pure fold step - extracts the child address from a factory-announcement event) + **`ChildRegistry`**
  (address → discovered-child entry with block/log/timestamp/parent/depth; a monotonic `version` =
  RFC-0009 §3's `filter_version`; idempotent insert; **reorg `rollback_to`** dropping children
  discovered above a block; a content **`hash`** so a sealed segment can later record which discovered
  set produced it). Determinism proven: a reorged-then-rolled-back registry has the identical content
  hash to one folded canonically. No pipeline wiring yet - the tip/backfill/ExEx ingestion regimes that
  *consume* this land in steps 2–5. 101 tests green (+5 factory), clippy clean.
- **2026-07-16 - RFCs 0009–0011 filed (design docs, not yet built).** Three forward-looking RFCs
  committed to `docs/rfcs/`: **0009 factory & dynamic contract discovery** (v2 - Uniswap-class runtime
  child contracts, discovery composed with the shipped pipelined backfill via a `filter_version` rule,
  ExEx-mode simplification, factory path-equivalence test); **0010 admin UI & webhooks** (v2 - a
  local-only embedded UI + a single host-side delivery engine that RFC-0008 C5's alert sink now
  *reconciles into* as one of two producers, poll-based until streaming ships, `--seal-direct` backfill
  suppression via `since`); **0011 graph-network nest & the Lodestar migration** (the GraphOps-pilot
  target - extend the Horizon nest to the full Graph contract suite on Arbitrum, migrate Lodestar
  panel-by-panel behind `nuthatch check` parity gates, cross-operator segment-hash determinism as the
  ops correctness signal; forces a small RFC-0001 per-contract `events` allowlist amendment). Docs only.
- **2026-07-16 - RFC-0008 C6: compliance pack manifest + audit surface (RFC-0008 COMPLETE).** The
  finale - the "prove it" layer. New `src/pack.rs`: a signed, content-addressed **`compliance-pack.toml`**
  (`pack build`) declaring the nest's compliance configuration by artifact hash - the decode-registry
  hash, screening list snapshots (hash + count), the screen component's content hash + its (empty)
  grants, flag config, and alert routing. **ed25519** signing (`pack keygen`; key in a local JSON file,
  no key service); **`pack verify`** checks the signature over the canonical body, re-hashes the
  referenced artifacts, and confirms grant conformance - so a customer can confirm *which* pack produced
  their alerts without trusting the source. New `src/audit.rs`: **`audit replay --from --to`** re-runs
  the pure screening over the sealed segments and diffs against the stored `sanction_hit` annotations -
  PASS means they reproduce exactly from (list snapshot, block range, component); **`audit report`**
  summarises hits/flags with list-snapshot hashes + block bounds (markdown or `--json`). MCP gains
  **`flags`**, **`exposure`**, **`screen_status`** tools (now 11) + a compliance section in the schema
  hint, so an agent can answer "was address X flagged, and against which list version?". **Gate met:**
  a full-pipeline integration test (seal → screen → `audit replay` reproduces exactly → `audit report`
  summarises) + a sign/build/verify roundtrip incl. tamper + missing-artifact detection; 96 tests
  green, clippy clean. Verified live on a clean USDC nest: `pack build --key` → `pack verify` PASS
  (signed, artifacts match); `audit replay` reproduced **156/156** sealed sanction_hits exactly; `audit
  report` summarised them. Adds `ed25519-dalek` + `getrandom`. **RFC-0008 is complete - labels+exposure,
  sanctions screening, threshold+velocity flags, effectful worlds, alert webhooks, and the signed
  audit pack - and with it all eight RFCs (0001–0008) have shipped.**

- **2026-07-16 - RFC-0008 C5: alert webhooks.** Flag/hit annotations delivered to operator-configured
  HTTP endpoints, **at-least-once**, **without ever blocking the indexer**. New `[[alerts]]` config
  (`kinds = [...]`, `url = ...`) routes annotation kinds to sinks. New `src/alerts.rs`: a **durable
  outbox** in the hot store (redb - new `OUTBOX` table + `outbox_push/pending/remove/trim/len`;
  survives restart, so at-least-once holds across a bounce), an enqueue that's one fast write
  (decoupled from delivery), and a background **delivery worker** that drains the outbox via `reqwest`
  and removes an entry only on a 2xx (a failure is retained for retry). **A stalled sink can't wedge
  indexing**: the outbox is bounded (`outbox_trim`, 10k) and sheds its oldest entries loudly on
  overflow; delivery runs on its own task. A reorg re-fires each rolled-back annotation as a
  **`flag_retracted`** event, so a consumer that acted on a flag learns the chain took it back. Depth
  exposed as `nuthatch_alert_outbox_depth` (`/metrics`) and `alert_outbox` (`/`). Delivery lives
  host-side by design - the guarantees (durable, retraction-correct, non-blocking) are host state and
  the endpoint is operator-configured, not a URL an untrusted component picks; the C4 grant model
  remains available for a `wasi:http`-sandboxed enricher. **Gate met:** an e2e test drives a real local
  webhook server - a raised annotation delivers a `flag`, a reorg delivers a `flag_retracted`,
  delivered entries leave the outbox, and a dead endpoint retains the alert for retry. 93 tests green,
  clippy clean. Verified live on USDC: a `threshold_flag` sink delivered 183 alerts to a local receiver
  (event/kind/value intact), outbox draining as designed. 5 new tests (+router, +noop, +trim, +live
  webhook flag/retraction, +failed-retry).

- **2026-07-16 - RFC-0008 C4: effectful worlds (the capability-injection model).** The machinery that
  lets a WASM stage reach the outside world - but only as far as it is *granted*. Ported from liminal's
  per-component capability injection, adapted to the batched-Arrow boundary. New `wit/effectful.wit`: a
  host-provided **`kv`** capability (get/set) and an `effectful` stage world (`effectful-kv`) that
  imports it - the import makes the capability requirement visible in the component's *type*. New
  `src/effectful.rs` host runtime with **two enforcement layers**: (1) it reads the component's actual
  imports (`component_type().imports()`) and **refuses to load** one whose imports exceed its declared
  `Grants` - a clear error, before instantiation, no code inspection; (2) the linker is wired with only
  base WASI + the granted capabilities, so an ungranted import can't even instantiate. Grants come from
  the host (the pack manifest in C6), never the component; an effectful stage has no import that could
  write canonical entities, so **"annotations only" is enforced by the absence of the capability**, not
  by convention. New toy guest `components/recurrence/` (imports `kv`, keeps a per-address seen-count
  across batches - state a pure stage can't hold - emits `(address, seen)` annotations); its `kv`
  import is visible via `wasm-tools component wit`. **Gate met:** (1) loading the kv-importing component
  with no grant is rejected with a clear error; (2) with `kv` granted it runs and its state persists
  across batches. 88 tests green, clippy clean. *Transparent slice boundary: outbound-HTTP (`wasi:http`)
  is in the `Grants` model + the import check already, but its linker wiring lands in C5, where the
  `alert-webhook` stage actually needs it. C4 has no indexer wiring - effectful stages are wired to
  consume flag/hit deltas in C5.* +2 tests (the two gate cases).

- **2026-07-16 - RFC-0008 C3: threshold & velocity flags.** Two flavours of compliance flag, both
  configured in `nuthatch.toml` (`[flags]`), amounts in token **base units** (i128 - no currency
  conversion in-core). **Threshold** (`flags.threshold`): any single transfer ≥ N becomes an
  append-only `threshold_flag` annotation, block-keyed so it seals to its own Parquet table and rolls
  back with its transfer - a pure per-transfer predicate, no aggregation needed. **Velocity**
  (`flags.velocity_amount` + `velocity_window`): a new DBSP windowed view (`velocity.rs`, the same IVM
  machinery as balances/exposure) tracking per-address outbound volume + count per **tumbling
  block-bucket** - an honest, documented approximation of "~24h" (blocks, not wall-clock; a true
  sliding window would need per-block aging). A reorg re-feeds the transfer at weight −1 and the
  bucket's volume retracts, so an invalidated flag disappears; restart-safe via `rebuild_velocity`
  (cold DuckDB fold + hot replay, like the other views). Both served at **`/flags?kind=threshold|
  velocity`** (velocity from the live view; threshold's sealed history via `/sql SELECT * FROM
  threshold_flag`). **Gate met:** golden tests for the velocity aggregate + retraction convergence and
  the threshold predicate, both with the **i128 overflow-adjacent** case the RFC asks for; 86 tests
  green, clippy clean. Verified live on USDC (threshold 100k, velocity 1M over 50-block windows):
  `/flags?kind=threshold` surfaced 148k & 1.4M-USDC transfers (168 sealed), `/flags?kind=velocity`
  surfaced an address moving 90M USDC across 11 transfers in one window; 3,384 velocity buckets tracked.
  6 new tests (+4 velocity, +2 flags). Reorg retraction wired for velocity; threshold flags roll back
  via the block-keyed store.

- **2026-07-16 - RFC-0008 C2: pure sanctions screening.** The audit centrepiece: screening is a
  **pure, zero-capability WASM component**, and lists are **content-addressed data** - so every hit
  traces to `(list-snapshot hash, block range, component hash)` and reproduces byte-for-byte. New
  `nuthatch lists fetch <ofac-sdn|eu-consolidated|…> [--file|--url]` extracts every `0x…40hex` address
  (crypto-addresses only - no name/entity fuzzy matching) into a `lists/<sha256>.json` snapshot,
  host-side and out-of-band (never a phone-home in the data path). New `screen` component
  (`components/screen/`, wasm32-wasip2, embedded in the binary via `include_bytes!` so it always
  travels with the single binary; **imports = base WASI only**, verifiable with `wasm-tools component
  wit`) takes a transfers batch + a sanctioned-address batch over the Arrow boundary and emits
  `sanction_hit` facts; the host stamps each with the list + component hashes the sandbox never sees.
  Two paths: a **live stage** (`[screening] lists = [...]` in `nuthatch.toml`) that screens each
  window, and the audit-grade **backfill** `nuthatch screen --list <hash> --from --to` that re-screens
  *sealed* transfers over immutable segments. Hits become append-only `sanction_hit` annotations -
  block-keyed (so they seal + roll back with their transfers), sealed to their own Parquet table,
  queryable at `/sql`. Segment sealing is now **content-addressed idempotent** (re-auditing a range is
  a no-op, not a double-count). **Gate met:** golden test (fixture list → exact hits) + the replay test
  (live screening == backfill screening, i128 values loss-free), 80 tests green, clippy clean. Verified
  live on USDC: `lists fetch` → `screen` over 32,149 sealed transfers → 1,475 `sanction_hit`s with full
  provenance in `/sql`; re-run idempotent; live stage logged per-window hits. 6 new tests (+3 lists,
  +3 screen incl. the golden + replay gates); the pure component's purity checked via `wasm-tools`.

- **2026-07-16 - RFC-0008 C1: labels + direct counterparty-exposure view.** The compliance pack's
  foundation. New `nuthatch labels import <csv|json>` writes a **content-addressed** label snapshot
  (`labels/<sha256>.json`) - list-as-data, the same discipline sanctions lists will use in C2: the hash
  is a reproducible name for exactly that (address, label) set, import is append-only, and loading
  merges every snapshot. Labels are queryable via `/sql` (a DuckDB `labels` view over the snapshots).
  New `exposure.rs` maintains a **DBSP** view of *direct* counterparty exposure to the labeled set: for
  a transfer `from → to`, if `to` is labeled the sender gains **outbound** exposure (count + summed
  amount), if `from` is labeled the recipient gains **inbound** - served at `/exposure/{address}`.
  Amounts are **i128** (same discipline as balances - a threshold view on i64 would be a liability),
  and a reorg **retracts** through the circuit exactly like balances (golden test covers join +
  retraction convergence + i128 + seed=replay equivalence). Restart-safe: `rebuild_exposure` folds cold
  sealed segments to pre-summed `(key, amount, count)` in DuckDB (joined against the `labels` view) and
  replays only the hot tail. Verified live on USDC: labeled the top recipient, a real sender showed
  9 outbound transfers summed correctly; `exposure_entries` populated from cold+hot rebuild. 74 tests
  (+5 labels, +4 exposure, +1 analytics cold-exposure fold). *Deliberate slice boundary: chain-derived
  annotation facts (sanction hits) sealed to their own Parquet table land in C2, where they're produced;
  C1's annotation kind is labels, durable as content-addressed snapshots.* RFC-0008 rewritten to v2
  (P0 i128 marked shipped; C4 effectful-worlds decoupled from the grant milestone; operator-channel
  dividing line added).

- **2026-07-16 - RFC-0007 (launch & validation): the launch kit.** The artifacts that make going
  public deliberate rather than a shout into the void. New `SECURITY.md` (scope: the `/sql`+MCP surface
  and the WASM host boundary; private-advisory reporting; 0.x support policy), GitHub issue templates
  (bug + feature, both routing scope/vuln reports correctly) with a `config.yml` that sends questions to
  Discussions and vulns to the private advisory flow. New `docs/validation/` records the structured
  adoption conversations against pre-registered day-30 thresholds - **conversation #1 (GraphOps, the
  infrastructure operator) is logged as *exceeded*** (partnership + revshare proposal), verbatim answers
  left as honest placeholders for transcription; four profiles remain pending. New `docs/launch/`
  carries pre-written copy with the real measured numbers - a Show HN draft (~58 MB / ~37 MB RAM,
  ~289 → ~5,837 ev/s ~20×, honest limits verbatim), the home-turf forum post (Horizon-nest parity as
  the ask), and the r/rust determinism-proof angle. RFC-0007 rewritten to v2: records the operator
  conversation, adds the operator pilot as a launch phase (decoupled from public launch), revises the
  conversation roster, and resolves the demo-instance + Base-gate open questions. Docs only.

- **2026-07-16 - RFC-0005 step 2: operator runtime surface (`/metrics`, SIGTERM, bind warning).**
  The §6 operator signals - the endpoint an operator alerts and bills against. New `GET /metrics`
  (hand-rolled Prometheus text, no framework): `nuthatch_tip_height` / `last_block` / `tip_lag_blocks`
  / `sealed_through` gauges, `rows_decoded` / `rows_sealed` / `reorgs` / `http_requests` / `sql_queries`
  / `sql_rejections` / `rpc_requests` counters, and `rss_bytes`. **Graceful shutdown** on SIGTERM/SIGINT
  (axum drains, ingest is checkpointed → clean exit 0, restart resumes without gaps). A **loud startup
  warning** when bound off-localhost, pointing at the guards + the new `docs/operators.md` (guards,
  metrics, lifecycle, 0.x stability contract). The `/sql` guards themselves (timeout + row cap +
  concurrency) already shipped. Verified live: `/metrics` served real values, bind warning fired on
  `0.0.0.0`, SIGTERM exited 0. 63 tests (+2).
- **2026-07-16 - RFC-0006 (sustainability): grant drafts + governance.** Public, PR-reviewed grant
  applications in `docs/grants/` - **NLnet/NGI** (`nlnet.md`, €38,400: semantic layer, IVM
  generalization, GraphQL compat, security audit) and **EF ESP** (`ef-esp.md`, $50–90K: reth ExEx tip
  mode, OP-stack multi-chain, benchmarks). New `GOVERNANCE.md` codifies the two-leg sustainability
  model (grants + operator revenue-share), the **neutrality guarantee** (no exclusivity / private
  forks / partner-only core features / roadmap veto - the AGPL makes capture structurally impossible),
  the core-vs-operator dividing line (guards in core; auth/metering/tenancy in the operator's
  gateway), the "won't do for funding or partnership" list, and the release-key-custody item. Adds a
  `FUNDING.yml` Sponsors button. RFC-0006 rewritten to v2 (grants are now *one* of two legs, not "the"
  revenue model; adds no-double-funding + disclosure rules). Docs only.
- **2026-07-16 - v0.1.0 release.** First tagged release: multi-contract full-ABI decode across
  Ethereum + Arbitrum One + Base, finality-sealed Parquet + DuckDB SQL, DBSP i128 balance view, the
  ~20× seal-direct/pipelined backfill, and the operator surface (`/metrics`, `/sql` guards, graceful
  shutdown). Published to crates.io and as prebuilt binaries on the GitHub Release. `cargo install`
  compiles on rustc ≥ 1.95; the binaries are the recommended path (no compile, no toolchain quirks).
- **2026-07-16 - RFC-0005 step 1: Base chain registry entry.** Adds `base` (chain 8453, OP-stack) to
  the registry - keyless Base RPCs, the same L1-aware `FinalizedTag` finality policy as Arbitrum, a
  moderate `log_window` the adaptive chunker tunes. Completes the operator launch matrix the RFC-0005
  (v2) release criteria call for (Ethereum + Arbitrum One + Base), an afternoon of registry work under
  the RFC-0002 §1 design. Verified live: Base serves the `finalized` tag (latest vs finalized ~470
  blocks apart), and `init 0x8335…2913 --chain base` resolved Base USDC via Sourcify and `dev` decoded
  6,516 Base events. (RFC-0005 rewritten to v2: adds the GraphOps operator channel - OCI image,
  `/metrics`, query guards, config-stability contract - as first-class v0.1.0 release criteria.)
- **2026-07-16 - RFC-0004 step 5: adaptive `getLogs` range chunker.** Replaces the fixed per-chain
  window with a controller (`chunker::AdaptiveWindow`) targeting ~2,000 logs/response: overshoot or a
  provider "result too large" error shrinks it (and retries the same range), an undershoot grows it -
  multiplicative, damped to 4×/step, bounded. One bit of code now handles dense (USDC) and sparse
  (Horizon) ranges and self-heals into any provider's result cap, instead of hand-tuning a constant.
  Wired into both the tip-following loop and `backfill_direct`; `is_result_too_large` matches the major
  providers' cap phrasings. Verified: footprint green (41 MB / 2,412 transfers), 6 new tests.
  _(This push also carries the in-progress `/sql` `QueryGuard` hardening - wall-clock deadline + row
  cap + a concurrency semaphore - that was in the working tree; bundled because it shares `indexer.rs`.)_
- **2026-07-16 - RFC-0004 step 4: `dev --seal-direct` (the 20× in production) + backfill-semantics
  fix.** The seal-direct + pipeline win now lives in `dev`, not just the bench. `nuthatch dev
  --seal-direct [--concurrency K]` runs a **phased** cold-start backfill: fast-seal the finalized
  history straight to Parquet (no redb), rebuild the IVM view from those segments, then hand the
  near-tip window to the normal hot loop. Verified live on USDC: 39,943 rows sealed in ~8 s, 9,615
  holders rebuilt from cold, then tip-following resumed cleanly. **Also fixes a regression** the CI
  footprint job caught: since `dev` learned to honour vendored `start_block`s, `--backfill` was being
  silently ignored on a nest that declares them. Now `--backfill N` is an `Option` that **explicitly
  overrides** `start_block` (recent-history mode); omitting it backfills from deployment. `cold_start_block`
  policy tightened + re-tested. 51 tests.
- **2026-07-16 - RFC-0004 step 3: pipelined backfill (~20× stacked, measured).** With storage cheap
  (seal-direct), wall-clock is dominated by sequential `getLogs` latency. `indexer::backfill_direct_pipelined`
  fetches `K` windows concurrently (`futures::stream::buffered`) but consumes results **in block
  order** - so the sealed segments are byte-identical to the sequential path (proven by
  `pipelined_backfill_matches_sequential`); concurrency overlaps latency without touching the output.
  Exposed via `bench backfill --seal-direct --concurrency K`. Measured on USDC (same 120 blocks, same
  ~24 requests): seal-direct 2,420 ev/s → **8-way pipeline 5,837 ev/s (~2.4×)**, stacking to **~20×
  over the redb baseline** (289 ev/s). Public-RPC-bound here (4 endpoints); higher against an own node.
  RSS 62 MB (K windows in flight - bounded, within budget). 51 tests (+1 determinism).
- **2026-07-16 - RFC-0004 step 2: seal-direct backfill (~8.7× measured).** Past-finality history can
  skip the hot store entirely: `indexer::backfill_direct` streams decode → buffered rows →
  content-addressed Parquet segments, no redb write, no read-back, no prune - with the same implicit
  columns (incl. batched `block_timestamp`) and the *same* `seal_range` writer, so a given range yields
  **byte-identical** segments regardless of path (proven by `seal_direct_matches_seal_via_hot_store`).
  Bounded buffer caps RSS. Exposed via `bench backfill --seal-direct` for the measured before/after,
  and reusable by a future `dev --seal-direct`. Measured on USDC (same 120 blocks, same 24 RPC
  requests, only the storage path differing): hot store 289 ev/s vs **seal-direct 2,521 ev/s - ~8.7×**;
  the gap is ~12k per-row redb fsyncs the direct path never pays. 50 tests (+1 path-equivalence).
- **2026-07-16 - RFC-0004 step 1: `nuthatch bench backfill` (measure first).** An honest,
  reproducible backfill-throughput harness - runs the real fetch → decode → store path over a *pinned*
  block range and reports the **median** of events/sec, wall-clock, peak RSS, and RPC requests (incl.
  failover retries), emitting a `bench-report.json`. Throwaway store per run; `--rpc` overrides the
  endpoints for an own-node tier. The house rule is codified: every published number traces to a
  report artifact - no hand-typed figures. Nothing is optimised yet; this is the baseline the
  seal-direct / adaptive-chunker / pipeline slices must each beat on a measured before/after. Added
  `RpcClient::request_count()` and `docs/benchmarks.md` (methodology, workloads W1–W3, tiers T1–T3).
  Verified live: USDC over 201 blocks = 21,171 events in 53.6 s = ~395 ev/s, 47 MB peak, on public
  RPC (latency-bound, single-threaded - exactly the number the optimisations target). 49 tests.
- **2026-07-15 - RFC-0003 groundwork: source-agnostic `indexer::run`.** Split `dev` into the RPC
  front-end (builds an `RpcClient` source) and a shared `run(source, dir, config, listen, backfill)`
  that drives the whole pipeline - decode → hot store → seal → IVM → serve - against any `Source`.
  `dev` is now a thin wrapper; `nuthatch-node` will build an ExEx `Source` and call `run` directly, so
  the reth path reuses the identical core with zero business-logic fork (the Source-trait promise, now
  cashed in). 47 tests, clippy, fmt green.
- **2026-07-15 - RFC-0003 groundwork: expose the core as a library.** `nuthatch` is now a lib + bin,
  not bin-only - `src/lib.rs` re-exports every module (decode, hot store, seal, IVM, serve, the
  `Source` trait, …). The binary is one front-end over that library; `nuthatch-node` (the colocated
  reth ExEx build) will be another, reusing the *same* indexing core through the `Source` trait rather
  than forking it. Also confirmed the other RFC-0003 gate: reth v2.4.0 (git) **resolves cleanly
  alongside our `alloy 1.6`** (913 packages, no version conflict). Pure refactor - 47 tests, clippy,
  fmt all green. Both RFC-0003 blockers (toolchain, dependency resolution) are now cleared.
- **2026-07-15 - RFC-0003 groundwork: toolchain 1.94.1 → 1.95.0 (unblocks reth).** RFC-0003 embeds
  reth as a colocated ExEx (`nuthatch-node`) reusing the same dbsp-backed indexing core. reth v2.4.0's
  MSRV is **rustc 1.95**, but our pin was 1.94.1 (chosen only to dodge the `dbsp` next-solver ICE that
  lands on 1.97). The open question was whether *any* toolchain satisfies both - and **1.95.0 does**:
  verified dbsp 0.320 compiles clean in release on 1.95, and the full nuthatch suite (47 tests) +
  clippy are green. Bumped `rust-toolchain.toml` and CI to 1.95.0. So the ExEx build can reuse the
  core with no toolchain fork - RFC-0003 is feasible with no hardware spend (build + unit-test against
  reth's ExEx test harness; a real node is only needed for the published latency soak).
- **2026-07-15 - RFC-0002: `dev` honours vendored deployment blocks.** A nest that vendors
  per-contract `start_block`s was storing them but the indexer ignored them - a cold start always used
  the `--backfill` tip offset, so "index this nest" never meant "from deployment". Now a cold start
  backfills from the nest's **earliest** vendored `start_block` (clamped to the tip) when present, else
  the `--backfill` offset as before - via a pure, unit-tested `cold_start_block(...)`. Verified live on
  the Horizon nest against an archive Arbitrum RPC: `dev` logs "backfilling from deployment block
  42449585". (Deploy blocks were detected reliably against an archive node - public
  sequencer/non-archive endpoints give inconsistent historical `eth_getCode`.) 47 tests.
- **2026-07-15 - RFC-0002: robustness fixes from Horizon dogfooding.** Authoring the Horizon nest
  (three real Arbitrum contracts, derived views) surfaced two engine bugs, both now fixed and
  regression-tested. **(1)** The read-only `/sql` guard checked `starts_with("select"/"with")` on raw
  text, so a query opening with a `-- comment` (or `/* … */`) was wrongly rejected - it now skips
  leading SQL comments before checking. **(2)** A nest view that UNIONs several event tables
  cascade-failed when one of them had no sealed data yet (common on a sparse/low-traffic contract):
  the whole view silently vanished. Now every *declared* table resolves - a sealed one as a real
  view, an unsealed one as an **empty typed view** (columns and their `*_dec`/`*_overflow` siblings
  reconstructed from `schema.json`, `WHERE false`) - so derived views compute over sparse data instead
  of disappearing. Verified live: the full Horizon model (`allocations`, `indexers`, `global`,
  time-bucketed rollups) computes on real Arbitrum data - 390 active allocations, 5 indexers, 70,030
  GRT indexing rewards - with the empty `operators`/`delegations` views present, not fatal. 44 tests.
- **2026-07-15 - RFC-0002 step 5: `nuthatch check` (invariant/parity framework).** A nest ships
  `checks/*.sql` - each a read-only query over its sealed data (per-event tables + derived views) -
  and `nuthatch check [name]` runs them, comparing each result to a recorded expected fixture
  (`checks/expected/<name>.json`), printing a row-level diff on mismatch and exiting non-zero. For the
  Horizon nest those fixtures are the deployed subgraph's answers at a pinned block, so this *is* the
  parity check; the framework is generic (any nest can ship invariants). Hermetic by design - it
  compares committed fixtures, not a live endpoint, so it runs in CI with no network. `--update`
  re-records fixtures from current results (authoring). Verified live: recorded 5-row fixtures on USDC,
  a matching run passed (exit 0), a tampered fixture failed with a clear diff (exit 1). 43 tests.
- **2026-07-15 - RFC-0002 step 4a: nest-defined derived-entity views.** A nest can ship
  `views/*.sql` - DuckDB views over its per-event tables (e.g. fold Created/Resized/Closed into a
  current-allocation view) - and the analytical `/sql` surface now loads them, in sorted filename
  order (so `20-*.sql` can build on `10-*.sql`), after the per-event table views. Best-effort: a view
  over a not-yet-sealed table, or a bad statement, is skipped with a debug log rather than failing
  the surface. Point-reads deliberately skip them (they touch only raw tables). This is the serving
  side of the Horizon nest's derived entities; DuckDB views read *sealed* data, so derived entities
  lag the tip by the finality window (raw tables stay tip-fresh) - the honest freshness tradeoff the
  RFC documents, and the concrete motivation for IVM generalisation later. 42 tests.
- **2026-07-15 - RFC-0002 step 3: `init --from` + config schema versioning.** A nest is just a repo
  (committed `nuthatch.toml` + vendored ABIs), so publishing one is `git push` and consuming one is
  `nuthatch init --from <git-url | ./dir>` - no registry service, deliberately. `--from` clones (shallow)
  or copies the nest, strips the clone's `.git`, and **validates** it: the toml parses at a supported
  schema version and the decode registry builds from the vendored ABIs (nothing is re-resolved - the
  nest is self-contained). New `schema_version` in `[nest]` (default 1); a nest declaring a newer
  version is rejected with a clear upgrade message - the guard that makes consuming third-party nests
  safe. Verified live: `init --from` over both a local dir and a git repo produced a runnable nest
  (`dev` indexed it with no ABI resolution); the version guard and the `addresses`/`--from` conflict
  both fire. 41 tests (+2).
- **2026-07-15 - RFC-0002 step 2: `block_timestamp` implicit column.** Every row now carries
  `block_timestamp` (u64 unix seconds) from the block header - the RFC-0001 amendment the time-bucketed
  aggregation views need. It's batch-fetched: after decoding a window the indexer collects the distinct
  blocks that produced rows and asks for their timestamps in a *single* JSON-RPC batch (one round-trip
  even for a dense window), via new `RpcClient::block_timestamps` / `Source::block_timestamps`.
  Best-effort - a block the endpoint can't answer stores 0. Verified live on USDC: hot rows carry a
  current timestamp, and `date_trunc('minute', to_timestamp(block_timestamp))` yields clean per-minute
  rollups over sealed data. 39 tests.
- **2026-07-15 - RFC-0002 step 1: chain registry + Arbitrum One + L2 finality.** The chain registry
  generalises beyond mainnet - each chain now carries a **finality policy** and an `eth_getLogs`
  window, so an L2 is a data entry, not a fork of the indexing loop. New `arbitrum-one` (chain 42161,
  keyless RPCs) uses a `FinalizedTag` policy: it prefers the node's L1-aware `finalized` block tag
  (correct by construction on an L2), falling back to a fixed depth (~7.5 min) when an endpoint
  doesn't serve the tag. `Source` gained `finalized()`; the seal ceiling is now a pure, unit-tested
  `seal_ceiling(finality, tip, tag)`. Mainnet keeps `Depth(64)`/window 20; Arbitrum uses window 2000
  (sparse events, fast blocks). Verified live: `init 0x00669A…eF03 --chain arbitrum-one` resolved the
  Horizon staking proxy via Sourcify (28 tables); `dev` sealed exactly up to Arbitrum's live
  `finalized` block (484091237, *not* the depth fallback), 2000-block windows. 39 tests (+5).
- **2026-07-15 - RFC-0001 finished to the letter.** Closed the last deviations between the shipped
  indexer and RFC-0001's design. **u256 SQL ergonomics (§2):** every big-integer column now gets two
  derived DuckDB view columns - `{col}_dec` (the value as `DECIMAL(38,0)` when it fits, else NULL) and
  `{col}_overflow` (true when the exact value exceeds 38 digits) - so analytics can `SUM(value_dec)`
  without hand-casting text. **Implicit provenance columns (§2):** every table now carries `block_hash`
  and `_seq` (a deterministic monotonic ordering key = `block << 20 | log_index`, not a mutable
  counter - re-executable by construction) alongside the existing `block_number/tx_hash/log_index/
  address`. **Indexed dynamic types** get a `_hash`-suffixed column name (the topic holds
  `keccak(value)`, not the value). Added the acceptance tests the RFC named: golden decodes for an
  address-heavy event (Uniswap V3 `PoolCreated`) and an indexed-string event, plus a cross-table
  `/sql` JOIN. Verified live on USDC: `SUM(value_dec)` over 8,736 transfers, `block_hash`/`_seq`
  present on every row. 34 tests green (+4). **RFC-0001 is now complete in spirit and letter.**
- **2026-07-15 - Correctness gaps closed: i128 balances + IVM restart-replay.** Two teeth-baring
  fixes to the balance view. **(1) i128 base units.** The view accumulated in i64, so any transfer
  above ~9.2e18 base units - barely ~9.2 tokens of an 18-decimal token - was *silently dropped*. The
  circuit, deltas, and storage now use i128 (max ~1.7e38); balances serialise as decimal strings
  (JSON numbers can't carry i128, and a client parsing a huge balance as f64 would corrupt it). On
  live WETH, **34 holders exceed i64::MAX** (top ~10,001 WETH = 1.0e22 base units) - every one of
  them previously mis-counted. **(2) Restart-replay.** The view is derived, not persisted, so it's now
  reconstructed from the durable facts on a warm restart, using the same circuit that maintains it
  live: sealed (immutable) segments fold to one net-per-address row directly in DuckDB (`HUGEINT` =
  i128 - no replaying millions of transfers), and only the small un-sealed hot tail is replayed. Both
  paths verified live: a cold-only restart reproduced 791/791 holders exactly; a hot-only restart
  replayed 840 transfers to reproduce 309/309. Transfer column names are read from the registry
  (USDC `from/to/value`, WETH `src/dst/wad`), never hardcoded. 30 tests green (+3). _RFC-0008 P0 for
  the compliance angle; both were the last known correctness gaps._
- **2026-07-15 - RFC-0001 step 6: multi-contract footprint re-measure (RFC-0001 complete).** Measured
  the full embedded pipeline on a genuine three-contract nest - USDC + WETH + DAI, **23 tables** - with
  everything live at once: combined `eth_getLogs`, per-table decode, per-table Parquet sealing + hot
  pruning, DuckDB SQL, and the IVM balance view (5,005 holders). **Peak RAM ~58 MB** (vs ~37 MB for a
  single contract), sealing 16,986 rows across 11 tables and pruning the hot store - still **2.8%** of
  the 2 GB budget, well under the 256 MB CI gate. Confirmed cross-contract serving: `/tables` returns
  all 23, WETH `c1__deposit` and DAI `c2__transfer` serve and query by their own columns. README status
  table + footprint section refreshed to the generalised (multi-contract, 8-tool) reality. This closes
  RFC-0001 - the transfer-only indexer is now a general ABI-driven multi-contract one, end to end.
- **2026-07-15 - RFC-0001 step 5: generalised serving from the registry.** The API and AI surface
  now describe the *whole* data model, not just transfers. `GET /tables` lists every decoded table
  with its columns, Solidity types and topic0; `GET /table/{name}?limit=N` returns recent rows merged
  across the hot tip and the sealed segments (deduped by `(block, log_index)`, hot wins), with optional
  `from_block`/`to_block`. Two matching MCP tools (`tables`, `table`) bridge the same endpoints - the
  tool count is now 8. `init` builds the registry up front and writes `schema.json` (`{registry_hash,
  tables}`); `llms.txt` and the Claude skill enumerate the real tables instead of hand-waving at them.
  Verified live on USDC (17 tables): `/tables` and both MCP tools return the full schema, `/table`
  serves merged hot+cold rows and 404s on an unknown table. 27 tests green. _Remaining: step 6
  (footprint re-measure on a multi-contract nest + README table refresh)._
- **2026-07-15 - RFC-0001 step 4: per-table cold storage.** Sealing generalises from transfer-only
  to every table: rows are grouped by their `table` field and each becomes its own content-addressed
  Parquet segment; `manifest.json` is now `{tables: {name: [segments]}}`. DuckDB exposes one view per
  table (`{alias}__{event}`); `/sql` queries any table and `/entity` point-reads search all tables
  across the hot→cold seam. **Hot-store pruning is restored** - the whole finalized range is pruned
  once every table's segment is durable (single global watermark). Row storage is unified (all rows
  are typed JSON with a `table` field; big ints render as decimal when they fit u128). Verified live
  on USDC: 2,893 rows sealed across 5 tables (transfer/approval/mint/burn/authorization_used) and
  pruned; `/sql` per-table (2,737 transfers, 292 approvals); a pruned row served via the DuckDB
  fallback. 27 tests green. _Remaining: step 5 (generalised `/tables` + `/table/{name}` serving,
  MCP + `llms.txt` regenerated from a schema manifest); step 6 (footprint re-measure)._
- **2026-07-15 - RFC-0001 step 3: multi-contract decode wired end-to-end.** `dev` now drives the
  `DecodeRegistry`: one combined `eth_getLogs` (all addresses × all topic0s) → decode *every* declared
  event of *every* contract → per-table rows in the hot store. The hardcoded Transfer path is retired
  (`decode.rs` deleted). Transfer-shaped rows keep the balance view, sealing, and the `transfers` SQL
  view working unchanged; non-transfer rows are stored generically (visible via `/entities`; per-table
  sealing + SQL land in step 4). Reorg rollback is table-agnostic (multi-table convergence test).
  **Proxy resolution at init** - EIP-1967 + legacy-OZ implementation slots - so USDC resolves to its
  FiatToken implementation (17 tables) instead of the bare proxy. Verified live: 2,844 rows across
  `usdc__transfer`/`approval`/`burn`/`authorization_used`, 1,444 holders. 28 tests green. _Step-3 limit:
  the hot store isn't pruned yet (step 4 does per-table seal + prune); only the transfer table is in `/sql`._
- **2026-07-15 - RFC-0001 step 2: multi-contract `init` + `nuthatch.toml` v2.** `init` now takes N
  addresses (+ optional `--alias`), resolves each ABI to `abis/{alias}.json`, and auto-detects each
  deployment block via an `eth_getCode` binary search (~25 calls - verified live: USDC→6,082,465,
  WETH→4,719,568). Config is now a `[nest]` header + `[[contracts]]` array; v1 single-contract files
  migrate transparently on load. `dev` runs the existing single-contract Transfer path on the nest's
  primary contract (and warns about the rest) until step 3 generalises decode + storage to every
  contract via the `DecodeRegistry`. 30 tests green (config migrate/roundtrip, alias validation,
  deploy binary-search, address normalisation).
- **2026-07-14 - RFC-0001 step 1: ABI-driven decode engine.** New `src/registry.rs` - a
  `DecodeRegistry` built from N contract ABIs (via alloy-json-abi / alloy-dyn-abi) maps topic0 →
  per-`{alias}__{event}` tables, filters by emitting address, and decodes any log into typed rows
  using the RFC-0001 type mapping (address / uint & int by width / bytesN / string / arrays→JSON /
  indexed-dynamic→hash). Records a stable, order-independent content hash for verifiability, and
  skips+counts anonymous events. 7 golden/property tests (real USDC Transfer, multi-contract table
  routing, type mapping, registry-hash stability, anonymous skip). Foundation only - not yet wired
  into the pipeline (steps 2-6: multi-contract init, generic storage, per-table sealing, serving);
  `dead_code` allowed on the module until integration removes it.
- **2026-07-14 - Slice 6 (first half): ingestion behind a `Source` trait.** Decode, hot store,
  sealing, IVM, and serving are now oblivious to where blocks come from - the indexer sees only
  `Arc<dyn Source>` (`tip` / `block_hash` / `logs`). `RpcSource` is the working impl (RPC polling, no
  node). `ExExSource` (feature = "exex") is the "no third-party" sovereignty upgrade - native-block-
  time tip latency from a colocated reth node - **designed and stubbed** with the push→pull bridge
  (reth's `CanonStateNotification` push → the loop's pull) implemented and tested; the reth wiring
  itself is deferred to a node environment (reth is an enormous compile that needs a synced node).
  See [`docs/exex-design.md`](docs/exex-design.md). No `#[cfg]` forks of business logic - adding ExEx
  is one new impl. Verified: 18 default tests + the exex stub's bridge test green; live indexing still
  works through the trait. _Deferred: reth wiring; scaled Postgres mode (a `HotStore` trait, same pattern)._
- **2026-07-14 - Slice 5: MCP server + AI surface.** `nuthatch mcp` speaks the Model Context
  Protocol over stdio (newline-delimited JSON-RPC), so a coding agent can query a running index
  directly. Six tools - `status`, `schema`, `sql`, `entity`, `balance`, `top_balances` - not a thin
  one-endpoint wrapper; `schema` returns a semantic hint (the seed of the governed semantic layer).
  It's a thin **offline** bridge to the local `nuthatch dev` HTTP API, so it never contends with the
  single-writer store and nothing phones home. `nuthatch init` now scaffolds `llms.txt` and a
  `.claude/skills/nuthatch/` skill into the project so agents learn the real query surface instead of
  hallucinating it. Verified: 18 tests green; a live MCP session (initialize → tools/list → tools/call)
  bridged `status`/`sql`/`top_balances` to a running index. _Deferred: the governed semantic layer
  + NL queries, streaming subscribe, Ollama/BYO-key AI authoring._
- **2026-07-14 - Slice 4 (first cut): WASM transform runtime.** Ported from
  [liminal](https://github.com/lodestar-team/liminal) with the brief's key change - **the WIT call
  boundary is a whole batch (Arrow IPC), not one event** (liminal was per-event; that can't keep up
  with backfill). A transform is a `wasm32-wasip2` component exporting `nuthatch:transform/stage`;
  the host (wasmtime 44) loads it with **zero capabilities** - base WASI only, no http/kv/filesystem
  - so it's deterministic by construction and its purity is checkable from the component's imports
  alone (`wasm-tools component wit`), no code inspection. Ships a pure example component
  (`large-transfers`: keeps transfers ≥ 1,000 USDC) and a `nuthatch transform <component.wasm>` CLI.
  Verified: 16 tests green incl. an end-to-end host-loads-real-wasm test; live run fed 2,470 USDC
  transfers → 525 filtered facts, deterministic. _Deferred: effectful worlds (http/kv-granted,
  annotations-only), wiring transforms as a live indexing stage, and signed pipeline manifests._
- **2026-07-14 - Slice 3: DBSP declarative views (the IVM core).** The first derived entity -
  per-address token balances - is now a **declarative incremental view**, not a hand-rolled handler.
  Balance is stated as Σ(in) − Σ(out) and maintained by a DBSP circuit: a new transfer is a +1 delta,
  and a **reorg is the same transfer re-fed with weight −1** (a retraction) - the identical circuit
  serves backfill and tip. Served at `/balances` and `/balance/{address}`. Verified: a deterministic
  golden test proves incremental maintenance + retraction convergence; live run derived 2,257 holder
  balances (top holder correctly the zero/burn address), **peak RAM 36.9 MB**. 14 tests green.
  _Known limits (this slice): balances accumulate in i64 base units (fine for USDC-class tokens); the
  view is in-memory and rebuilt per process - a warm restart resumes indexing but does not yet replay
  prior balances (persistence/replay is a later slice)._
- **2026-07-14 - Slice 2 complete: DuckDB SQL + hot-store pruning.** A read-only `/sql` endpoint
  runs analytical queries over the sealed segments via an embedded, memory-capped DuckDB (segments
  attached read-only; ingestion never writes DuckDB). Once a range is sealed and catalogued, its
  rows are pruned from the redb hot store - and `/entity/{id}` transparently falls back to DuckDB for
  pruned rows, so point-reads work seamlessly across the hot→cold seam. Verified live: sealed +
  pruned a 2,497-row segment, `/sql` aggregations correct, a pruned id resolved via the cold path;
  **peak RAM 37 MB** with the full pipeline. Binary is now 44 MB (DuckDB bundled). 13 tests green.
- **2026-07-14 - Slice 2 (in progress): Parquet sealing.** Once a block range passes finality
  (a conservative 64-block depth for now), its entities are sealed to an immutable, content-addressed
  (sha256) Snappy Parquet segment under `segments/`, catalogued in `manifest.json` with block bounds
  and row count; a monotonic `sealed_through` watermark advances so each block seals exactly once. The
  hot store is deliberately *not* pruned yet - point-reads keep hitting redb until the DuckDB serving
  path lands. Verified live: sealed a 2,355-row segment for finalized mainnet USDC; round-trips through
  Arrow in tests (10 tests green). The append-only cold layer never sees a reorg, by construction.
- **2026-07-14 - Slice 2 (in progress): reorg safety.** Block-hash checkpoints + `rollback_to`
  in the hot store; the indexer detects when its last committed block falls off the canonical
  chain and rolls back to the deepest surviving checkpoint. Reorgs land *only* in the mutable hot
  store - the invariant that lets later slices seal to immutable Parquet strictly past finality. A
  proptest asserts convergence: any random fork depth + alternate branch reaches the same state as
  indexing the winning branch directly (7 tests green). Verified live: no false reorgs on mainnet.
- **2026-07-14 - Slice 1 gate closed.** 5 deterministic golden decode tests (fixed USDC-transfer
  fixture → exact output) pass; measured peak RAM **~33 MB** indexing 7,013 transfers - 1.6% of the
  2 GB budget. Both non-negotiables (tests + footprint) met, so slice 2 is unblocked.
- **2026-07-14 - Slice 1: walking skeleton.** `init` (ABI via Sourcify v2, Etherscan fallback) →
  `dev` (RPC log polling with round-robin failover) → deterministic ERC-20 `Transfer` decode →
  redb hot store → axum HTTP API. Verified alive against live mainnet USDC, keyless: 170+ transfers
  indexed in ~1.5s with correct decimal values. Scope: one chain, Transfer-only, RPC-poll, redb-only.

_Next: consolidation - a `HotStore` trait for scaled Postgres mode, CI (test + RAM-budget gate), and closing known gaps (IVM restart-replay, i128 balances). reth ExEx wiring lands in a node environment._
