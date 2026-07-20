# Progress log

Newest first. One entry per push, tracking the [build order](CLAUDE.md#build-order-vertical-slices-each-ends-runnable).

- **2026-07-20 - Hardening: seal-direct backfill retries a transient RPC failure instead of aborting.**
  The §5 dogfood surfaced it live: a single `--seal-direct` window that hit a 403 from one endpoint (while
  the others throttled under concurrency) aborted the *entire* backfill. A per-attempt fetch already fails
  over across endpoints (`RpcClient::call`), but the concurrent seal-direct pipeline had no window-level
  retry, so an all-endpoints-at-once blip propagated straight through `res?` and killed the run — unlike
  the tip loop, which retries. Added `retry_transient` (bounded attempts, capped exponential backoff, base
  parameterised for tests) around both RPC calls in each backfill window (`getLogs` + `block_timestamps`);
  a window now waits and retries up to 5× before giving up, so a rate-limit or provider blip no longer
  wastes a long backfill. 2 new tests (recovers-after-two-failures, gives-up-after-max); 196 lib tests,
  clippy `-D warnings` clean.

- **2026-07-20 - RFC-0018 §5: dogfooded on the real horizon-nest / graph-network-nest — fork → instance,
  proven byte-identical; plus the bug it caught.** The RFC's acceptance case was a measured, byte-identical
  fork: `graph-network-nest` differed from `horizon-nest` by exactly one line (`name =`). Made it real:
  horizon gained the §1 brain (a `semantic.toml` describing all 12 authored views + the key event tables)
  and was reauthored as Starlark (`lib.star` defining a `graph_horizon(name, ...)` factory + a thin
  `nest.star` entry); graph-network became a **6-line `load()`-based instance** of it (−29 lines of
  duplicated `nuthatch.toml`, +6 of `nest.star`). Parity proven at three levels: **config** (a resolved
  `graph-network` Config is byte-identical to horizon's original TOML, modulo name), **counts** (a
  bounded Arbitrum backfill of both, then a per-table row-count digest over a pinned finalized range —
  identical across all 21 non-empty event tables), and **content** (md5 of the ordered reward-row content
  matched: `3bb7965d…`, 613 rows, 9 indexers on both). Pointing the feature at the exact repos it was built
  for immediately caught a real bug: `NestFileLoader` computed the catalogue root as `nest_dir.parent()`,
  which is empty for a relative `--dir graph-network-nest`, breaking every `//pkg:file` load — fixed by
  canonicalizing the nest dir first (+ regression test). This is the whole thesis demonstrated end to end:
  one reusable, semantically-described unit; its instances share the logic instead of forking it.

- **2026-07-20 - RFC-0018 §2b: `load()` composition — reuse a nest instead of forking it.** §2a made
  config *computable*; §2b makes it *composable*, delivering the RFC's headline property in full: a
  nest is a function, not a fork. A `nest.star` can now `load()` a factory function another nest
  *defines* and instantiate it — turning `graph-network-nest` (today a byte-identical fork of
  `horizon-nest`) into a one-line **instance** of it. The contract is **library-defines /
  entry-instantiates**, chosen for its lack of magic: a reusable nest is a `.star` that defines a
  factory and never self-instantiates; a thin entry `load()`s and calls it once. This falls out of the
  host design rather than being bolted on — a `load()`ed module is evaluated **without** the collector,
  so a stray top-level `nest()` in a library errors clearly, while the factory it defines calls
  `nest()` only when the *entry* invokes it (back in the entry's collector-bearing evaluator). The
  loader (`NestFileLoader`) is **confined** two ways: `load("lib.star", …)` relative to the nest dir,
  and `load("//pkg:file.star", …)` under a catalogue root (`$NUTHATCH_CATALOGUE`, else the nest dir's
  parent so sibling checkouts resolve with zero config) — `..` and any path escaping the root are
  refused before a byte is read. `contract()`'s leading `alias`/`address`/`start_block` are now
  positional-or-named so the RFC's `def erc20(...)` wrappers read naturally. Acceptance proof is a test
  where `graph-network` loads horizon's shared factory and inherits its staking contract + adds one of
  its own — no copy of horizon's config in sight; plus confinement + self-instantiation-rejection
  tests. 10 starlark tests, full suite green (193 lib + integration), clippy clean, drift gate clean.
  Builder skill `config-as-code.md` gains the composition section. Next: §5 — dogfood the real
  `horizon-nest` / `graph-network-nest` repos (fork → instance) with their `checks/` parity harness.


- **2026-07-20 - RFC-0018 §2a: an optional Starlark (`nest.star`) config front-end — "a nest is a
  function, not a fork."** A nest can now be authored as a `nest.star` that *computes* its config
  (loop over a basket of addresses that share an ABI instead of copy-pasting `[[contracts]]` blocks)
  as an opt-in alternative to `nuthatch.toml`. It is pure sugar: a `.star` and its equivalent `.toml`
  deserialize to a **byte-identical `Config`**, because both funnel through the same serde derives —
  so the win is authoring ergonomics, never new capability, and everything downstream (pack, mount,
  semantic layer, views) neither knows nor cares which front-end produced the config. The **§4 gate
  passed on all three axes** before a line of host code: `starlark` 0.14.2 is Apache-2.0 (one-way
  compatible with the AGPL-3.0 core, not on the forbidden list, transitive tree all MIT/Apache/BSD);
  binary-size delta **+4.56 MB** (72.6 → 77.1 MB release, +6.3%) for a full hermetic interpreter atop
  a binary already carrying DuckDB+wasmtime+arrow — within tolerance; and it's teachable (builder-skill
  `config-as-code.md`, drift-gate clean). The host (`src/starlark_config.rs`) exposes exactly **four
  closed builtins** (`nest`/`contract`/`factory`/`template`) mapping 1:1 to the config structs;
  `nest()` (called once) assembles a `serde_json::Value` and deserializes it to `Config`. Hermetic by
  construction — no clock, randomness, network, or FS — so it's a *description*, not a program, runs
  once at load, and never touches the deterministic data path (non-negotiable #4 untouched).
  `Config::load` gains the precedence hook (`nest.star` present ⇒ use it, else TOML as before). The
  acceptance bar is a round-trip test proving a loop-authored 2-contract nest equals its hand-written
  TOML twin; 6 new tests, full suite green (188 lib + integration), drift gate green, clippy clean.
  Next: §2b (restricted `load()` for composition, fork → instance) and §5 (dogfood on horizon-nest /
  graph-network-nest).


- **2026-07-19 - RFC-0018 §1b: `init` scaffolds the views layer + the builder skill teaches it.** The
  logic layer is now *discoverable the moment you `init`*, without moving the zero-authoring floor.
  `init` scaffolds `views/` with a commented, ready-to-uncomment starter view derived from the nest's
  own first table (`count(*)` + block range) + a `views/README.md`; `semantic.toml` gains a commented
  `[view.*]` stub so an author knows where to describe a view they enable. The starter is entirely
  comments — a no-op that validates clean — so the happy path is unchanged. Scaffolding is idempotent
  (`scaffold_views` never clobbers an existing `views/`, so `add` on a nest with authored views is
  safe). New `skills/nuthatch-builder/views.md` (under the RFC-0017 drift gate) teaches authoring a
  view, describing it in `semantic.toml`, and the three footguns (reserved-word `"from"`/`"to"`,
  big-int `_dec`, hot∪cold recompute-per-query). Verified live: a fresh `init` produces `views/10-
  example.sql` + `README.md`, the commented starter passes `nuthatch check`, and `semantic.toml`
  carries the `[view.*]` stub. That completes RFC-0018 §1 (the alive layer). 183 lib tests, clippy
  `-D warnings` clean. Next: §2 — the Starlark front-end, behind its licence/size gate.

- **2026-07-19 - RFC-0018 §1a: authored SQL views become a validated, drift-gated, described layer.**
  `analytics::define_nest_views` already loaded `views/*.sql` into the hot∪cold `/sql` surface — but
  silently: a broken view was swallowed to `debug!` and dropped, invisible to every describe-surface and
  unchecked for drift (the exact "a broken thing is worse than an absent one" anti-pattern RFC-0016
  fixed for semantics). This promotes the hidden hook to a first-class layer, entirely outside the
  deterministic data path (views are read-only `CREATE VIEW`s, recomputed per query). New
  `analytics::validate_nest_views` sets up the empty base surface and *binds* each view against the
  schema — so a view referencing a renamed/absent table or column simply fails to bind, which means
  **validation IS the drift gate** (no SQL parsing), each failure fuzzy-matched to a fix hint via the
  RFC-0016 `sql_errors` path. Surfaced loudly now, not swallowed: `nuthatch check` **fails** on a
  broken/drifted view, and `dev` startup **warns** with the hint — while the live query loader stays
  fault-isolated (a bad view never disables its siblings). `semantic.toml` gains `[view.*]` sections
  and the composed `/schema` (+ the MCP `schema` tool) now renders authored views, so an agent finally
  *sees* `top_recipients` and what it means. Verified live: `nuthatch check` on a nest with a
  `SELECT * FROM transfers` view prints `✗ view … hint: closest is c0__transfer` and fails. 182 lib
  tests, clippy `-D warnings` clean. Next: §1b (`init` scaffolds `views/` + starter + semantic stub +
  builder-skill `views.md`), then the Starlark front-end (§2, behind its licence/size gate).


- **2026-07-19 - 0.5.x hardening 4/N: a `/ready` endpoint + a loud RPC-stall signal.** Unattended
  operation needs a supervisor to tell "up and healthy" from "up but stuck." `/health` stays plain
  liveness (`200 "ok"`); new **`/ready`** is readiness — JSON (`tip`, `last_block`, `lag_blocks`,
  `sealed_through`, `last_poll_unixtime`, `seconds_since_poll`, `stalled`) returning **200 when fresh**
  and **503 when stalled** (no successful source poll within 90 s ⇒ every RPC endpoint is down). A
  just-started node that has never polled gets grace (never-polled ≠ stalled). Backing it: the tip loop
  (solo + roost) now records `METRICS.mark_poll_ok()` on every successful poll — exposed as
  `nuthatch_last_poll_unixtime` in `/metrics` — and logs a failed poll with **escalating** severity
  (`escalate_stall`: a warn on the first miss, then an error every ~60 s of "all RPC endpoints
  unreachable → indexing STALLED"). Retries never drop blocks (the same window re-fetches), so a stall
  is loud but self-healing. This closes the honest-stall-reporting half of the RPC-resilience item.
  Verified live: `/ready` → `{"ready":true,"stalled":false,…}` 200. (Per-nest labelled metrics — the
  SEC-9 refactor + GraphOps billing primitive — is its own slice; the poll/stall signal is legitimately
  process-global, one cursor per process.) 178 lib tests, clippy `-D warnings` clean.


- **2026-07-19 - 0.5.x hardening 3/N: corrupt/missing-segment recovery.** A sealed segment whose file was
  missing or corrupt broke *every* `/sql` query for its table — `read_parquet([…])` over the manifest's
  file list throws if any one file is unreadable, and the whole view fails. Two fixes make a corrupt
  segment reduce a table's cold data instead of crash-looping the node: (1) `analytics::define_views`
  now skips a manifest segment whose file is gone from disk (logs it, keeps the survivors) — so a
  missing/quarantined file never fails the query; (2) new `seal::verify_and_quarantine` runs once at
  `dev` startup (before the view rebuild scans segments): each manifest segment is hash-verified against
  its recorded content address, and a corrupt/tampered (`sha256` mismatch) or unreadable one is moved to
  a sibling `quarantine/` dir with a loud error, then indexing continues. Sealed data is immutable +
  content-addressed, so a hash mismatch is unambiguous corruption. Idempotent (an already-quarantined,
  now-missing segment isn't re-counted). Tests: `seal.rs` quarantines a corrupted segment and leaves the
  intact one; `analytics.rs` proves a query survives a deleted segment file. 177 lib tests, clippy
  `-D warnings` clean.
- **2026-07-19 - 0.5.x hardening 2/N: health-aware RPC failover.** The RPC client had round-robin
  failover but no memory of which endpoints were down — so a dead provider cost a full 20 s request
  timeout on every call that round-robined onto it (~1/N of calls), dragging tip-follow latency during a
  partial outage. Now each endpoint carries a health state: a failed call puts it in a 30 s cooldown and
  `endpoint_order` tries healthy endpoints first, cooling ones only as a last resort (soonest-to-recover
  first); a successful call clears it. So a dead provider is skipped after one failure and failover is
  immediate. The tip loop already retries the same window on failure (no silent gaps) — that's unchanged.
  The cooldown clock is wall-time used only for "try again after" (never in the deterministic data
  path). Corrupt/missing-segment recovery (the other half of the resilience item — a corrupt segment
  currently fails every `/sql` query) is split to its own slice. The loud all-endpoints-down stall
  signal lands with the health endpoint. 176 lib tests, clippy `-D warnings` clean.
- **2026-07-19 - 0.5.x hardening 1/N: security pass on the serving surface + CI supply-chain gate.** A
  full security review of `serve.rs`/`mcp.rs`/`webhooks.rs`/`analytics.rs`/`abi.rs`/`rpc.rs` (the surface
  a hosted platform will front) — **no criticals**: the read-only SQL gate holds three-deep (SEC-7 is
  safe — leading-keyword gate + single-statement `prepare` + ephemeral read-only connection; `COPY … TO`
  can't lead a `SELECT`/`WITH`), no SSRF (Sourcify/Etherscan/RPC hosts are fixed constants; user/chain
  input never enters a URL authority), no file-read via `/sql` (denylist + `allowed_directories`). Fixed
  the real disclosure/robustness findings: **`/nest` no longer leaks webhook URLs** (they embed secrets
  in the path — now reduced to scheme+host via `webhook_host`); **`/sql`/`/explain` errors are
  path-scrubbed** (`sanitize_sql_error` redacts the nest-dir prefix DuckDB embeds — SEC-11 restored for
  the error-prone endpoint); **`screen_status` escapes `'`→`''`** (injection, low-sev but closed at
  source); **constant-time admin-token compare** (`ct_eq`, no timing side-channel); **webhook delivery is
  now concurrent** (SEC-8 — `buffer_unordered(8)`, so one dead sink no longer throttles the drain;
  redb-single-writer preserved by applying store mutations in a second phase). New **`deny.toml` +
  cargo-deny CI job** (advisories + licences + bans + sources): the AGPL-compatible licence gate is now
  *enforced* (verified `ittapi`/`r-efi` resolve to their permissive arm, no GPL/LGPL-only sneaks in);
  three transitive RUSTSEC advisories ignored with written rationale (quick-xml DoS ×2 — not reachable,
  no untrusted XML through object_store; wasmtime-wasi FilePerms — tracked for a runtime bump). Deferred
  to later hardening slices: the `/sql` hot-scan RAM bound (perf), MCP path percent-encoding (low, no
  privilege gain). 175 lib tests + 7 e2e/integration suites, clippy `-D warnings` clean.
- **2026-07-19 - 🏷️ Release 0.5.0 — the delightful, agent-native release.** Version 0.4.0 → 0.5.0.
  Ships **three complete RFCs**. **RFC-0015 (the delightful core):** `nuthatch sql` REPL, **magical
  `init`** (omit `--chain` — it probes the known chains for the contract's bytecode), **live backfill
  feedback** (a progress line with events/sec + ETA, ending on a crisp "caught up to tip"), **`nuthatch
  add 0xAnother`** (grow a nest without re-init), the **MCP one-liner** (`mcp --print-config`), and
  copy-paste **systemd/Docker** prod recipes. **RFC-0016 (the agent-grade MCP):** the AI-native
  workstream, measure-first — an **eval harness** (fixture nest + 15-question oracle, CI-gated); the
  **governed semantic layer** (`semantic.toml` + a per-nest enriched `/schema` with the hot/cold
  coverage seam and derived footguns); **errors-as-prompts + `explain`** (SQL errors that teach,
  fuzzy-matched to the real schema; validate-without-executing); **result shaping + provenance**
  (compact tables, row caps, citable stamps); and **resources + prompts** (the full MCP surface). An
  agent now reads *this* nest's meaning, self-corrects from enriched errors, and cites its answers —
  with nothing in the deterministic data path touched. **RFC-0017 (the builder skill):** a
  repo-installable skill teaching an agent to *drive* nuthatch, with a `cli-reference.md` generated from
  the binary and a CI drift gate that fails on any hallucinated flag. Pending (honest): the RFC-0016
  Tier-B agent baseline and the RFC-0017 authoring eval both need a keyed agent run, not a typed number.
  171 lib tests + 8 e2e/integration suites, clippy `-D warnings` clean, footprint budget green.


- **2026-07-19 - RFC-0017: the builder skill — teaching coding agents to *drive* nuthatch.** A model
  asked to "set up nuthatch for this contract" today hallucinates flags (nuthatch is days old, in
  nobody's training data). New repo-installable skill at `skills/nuthatch-builder/` teaching an agent
  authoring knowledge (init/config/factories/compliance/roost/ops) — complementary to the per-nest MCP
  runtime knowledge (RFC-0016). Two rules keep it honest: **generate what can be generated**, and
  **CI-check the rest for drift.** New `src/skill.rs` renders `cli-reference.md` from clap's own
  metadata (the binary describing itself — every subcommand + flag, deterministic), emitted by a hidden
  `nuthatch skill-refs` subcommand. Authored files (`SKILL.md`, `workflows.md`, `compliance.md`,
  `troubleshooting.md`, `config-reference.md`) carry the recipes and the symptom→metric→remedy tables,
  written against the generated reference. **Drift gate** (`tests/skill_refs.rs`, CI): the committed
  `cli-reference.md` must equal what the binary generates now, AND every `--flag` mentioned in the
  authored prose must be a real flag — a skill that lies about a flag fails the build. README gains the
  one-line install (`cp -r skills/nuthatch-builder ~/.claude/skills/`). The authoring *eval* (an agent
  building a nest end-to-end, scored in the RFC-0016 Tier-B harness) is a follow-up — like the 0016
  baseline, a real score means a keyed agent run, not a typed figure. 171 lib tests + the drift gate;
  clippy `-D warnings` clean.
- **2026-07-19 - RFC-0016 S5: MCP resources + prompts — the AI-native workstream is complete.** The MCP
  server used a third of the protocol (tools only); now it advertises and implements **resources** and
  **prompts** too. `initialize` capabilities honestly list `tools`+`resources`+`prompts` (leaving room
  for a future `notifications`). Three **resources** with stable `nuthatch://…` URIs (`schema`,
  `tables`, `status`), each backed by an HTTP GET on the running nest — a client preloads context
  without burning a tool call. Three argument-taking **prompts** rendered client-side (no network):
  `profile-contract` (activity overview), `investigate-address {address}` (balances/exposure/flags/
  screening), `verify-a-number {claim}` (re-derive with provenance, minding the §2 footguns and using
  the §3 error hints). Verified live over the stdio bridge: capabilities, `resources/list`, and
  `prompts/get` interpolating its argument. **This closes RFC-0016** (S1 eval harness → S2 semantic
  layer → S3 errors-as-prompts + explain → S4 result shaping → S5 resources+prompts): an agent now
  reads *this* nest's meaning, self-corrects from enriched errors, gets context-shaped citable results,
  and can preload resources/prompts — the AI-native claim made literally true, with nothing in the data
  path touched. 171 lib tests; clippy `-D warnings` clean.
- **2026-07-19 - RFC-0016 S4: result shaping + provenance.** MCP responses now diverge from HTTP
  responses deliberately — curl and agents are different consumers. `/sql` gains an optional
  `max_rows` (clamped to the node cap) and a **provenance** block on every result (`as_of` block,
  `sealed_through`, `source`, `registry_hash`). The MCP `sql` tool passes a small default cap (200,
  agent-overridable via `limit`) and reshapes the JSON into a **compact aligned table** (measured
  smaller than the verbose per-row JSON — the density win for a context window), with **truncation as
  guidance** (`… truncated at N rows — aggregate (GROUP BY), tighten the WHERE, or raise \`limit\``)
  rather than silent cutoff, and a one-line **provenance stamp** (`— as of block N, sealed_through M,
  source hot+sealed, registry <hash8>`) so an agent can cite its answer against content-addressed
  data. Verified live: `/sql?…&max_rows=2` caps + stamps; the compact formatter is unit-tested for
  density, truncation guidance, and verbatim error relay. 169 lib tests; clippy `-D warnings` clean.
- **2026-07-19 - RFC-0016 S3: errors as prompts + `explain`.** A failed query is now a teaching
  opportunity instead of a round-trip tax. New `src/sql_errors.rs` classifies a DuckDB failure against
  the nest schema and appends a one-line fix hint (raw engine message always preserved): **unknown
  table** → fuzzy suggestion (`no table \`transfers\`; the closest is \`c0__transfer\``), **unknown
  column** → nearest real column, **reserved word** → `\`from\` is a reserved word and a column — double-
  quote it`, **big-int arithmetic** → `use \`value_dec\` for SUM/AVG`. Matching is off DuckDB's real
  message strings; suggestions come from a containment-then-Levenshtein matcher that only ever names
  real schema identifiers (never hallucinates) — the common agent slip (dropping the `{alias}__` prefix,
  pluralising) is caught by containment, genuine typos by edit distance. Wired into `GET /sql`, the MCP
  `sql` tool, and the `nuthatch sql` REPL's local path alike. New **`explain`** tool + `GET /explain`:
  validate a query WITHOUT executing it (binds tables/columns/types via a `LIMIT 0` wrapper, scans
  nothing) → `{valid:true}` or the same enriched error, so an agent checks a query's shape before
  spending a concurrency slot. Verified live on USDC across all four classes + explain valid/invalid. 8
  new unit tests; 166 lib tests + 6 e2e; clippy `-D warnings` clean.

- **2026-07-19 - RFC-0016 S2: the governed semantic layer + enriched `schema`.** The MCP `schema` tool
  was a static string, identical for every nest — it described the *shape* of the model, never *this*
  nest's data. Now it's composed per-nest from four layers. New `src/semantic.rs` + `semantic.toml`
  (beside `nuthatch.toml`): authored per-table/per-column **meaning**, generated at `init`/`add` from
  the ABI (honest "seeded from the ABI — edit to improve" fallbacks), with **derived-not-authored
  footguns** — reserved-word columns (`"from"`/`"to"`) and big-int columns (`value` → `value_dec`) are
  computed from the registry, so they're always present and always correct even if the author never
  opens the file. New `GET /schema` composes **structure** (registry) + **meaning** (semantic.toml) +
  **footguns** + live **coverage** (the hot/cold seam stated as numbers: `sealed_through` + tip); the
  MCP `schema` tool now relays it instead of the old static string. A **drift guard** (`dev` startup +
  every `/schema` call) warns loudly if the file describes a table/column the registry lacks — stale
  semantics are worse than none. `add` merges onto an existing file (authored text survives, footguns
  refresh). `semantic.toml` travels with `nest pack` and is hash-verified on `mount` for free (it's an
  authored input in the existing content-addressing). Verified live on USDC: the enriched `/schema`
  shows nest description, coverage seam, per-table meaning, and the footgun warnings. 159 lib tests + a
  new `semantic_layer` integration test (footgun derivation, no-drift, composed-doc golden); clippy
  `-D warnings` clean. NatSpec-from-Sourcify descriptions and sample-row evidence are follow-ups.
- **2026-07-19 - RFC-0016 S1: the eval harness (Tier A) — the measurement spine for agent-grade MCP.**
  The AI-native workstream is gated on measurement, not anecdote (the RFC-0004 rule applied to the MCP
  surface), so this is the first thing to land. New `eval/questions.toml` — 15 pinned questions across
  the classes agents trip on (aggregation, the `value`/`value_dec` big-int footgun, reserved-word
  columns `"from"`/`"to"`, coverage, filters, group-by) — each carrying a natural-language ask, the
  known-correct SQL (the oracle), and the expected result. New `tests/eval_harness.rs` (Tier A,
  CI-gated, no LLM) builds the fixture nest on the tape infra (10 blocks, 1–7 sealed / 8–10 hot),
  generates its `schema.json` so the typed view matches a real nest, and runs every question through
  the *same* hot∪cold surface an agent's `sql` tool hits — asserting each returns its `expect`
  (order-normalised, numeric-tolerant). This **proves the oracle** before any agent is scored against
  it: green = valid scoreboard, and a surface regression goes red before an agent eval ever runs.
  `eval/eval-report.schema.json` pins the Tier B report shape (model, temp, commit, question-set hash,
  first-try + overall pass rate). **Honesty note:** the Tier B *agent* baseline number is deliberately
  NOT published — a real score means running a keyed agent and committing the report, not typing a
  plausible figure (the house rule). The deterministic spine is in place; the baseline lands with the
  first keyed run. All 15 oracles pass; clippy `-D warnings` clean.
- **2026-07-19 - RFC-0015 slice 4: `nuthatch add 0xAnother` — grow a nest, no re-init.** The natural
  "one or many contracts" flow (RFC-0001): `add` loads the existing nest, resolves each new contract's
  ABI (Sourcify → Etherscan), vendors it, appends it to `nuthatch.toml`, and regenerates the derived
  artifacts (schema.json + the AI surface) — the chain, RPC endpoints, and screening config are already
  settled by `init` and left untouched (the chain is the nest's, never re-detected — one cursor, one
  chain). The next `dev` backfills the new contract from its own deployment block while the existing
  contracts resume from their stored cursor. Guardrails: refuses an address already in the nest, refuses
  `add` on a missing/invalid nest, and `--alias` is validated + collision-checked (auto-aliases continue
  the `c<N>` sequence past the existing contracts, skipping taken slots). Extracted a shared
  `write_nest_artifacts` so `init` and `add` regenerate schema.json/llms.txt/skill from one code path
  (no drift). Verified live: USDC (17 tables) → `add` WETH → 2 contracts / 21 tables; dup/missing/alias
  errors all fire. 153 lib tests, clippy `-D warnings` clean.
- **2026-07-19 - RFC-0015 slices 5+6: the AI one-liner and a dead-simple prod story.** **Slice 5** —
  wiring a coding agent to a nest is now one documented step: `nuthatch mcp --print-config` emits a
  copy-paste MCP client config (the `.mcp.json` block **and** the `claude mcp add nuthatch -- …`
  one-liner), pointing at this exact binary (absolute path via `current_exe`, so it works off `PATH`).
  And a human who runs bare `nuthatch mcp` in a terminal no longer hits a silent stdin block — when
  stdin is a TTY (no client driving it) it prints the same wiring guidance and exits, instead of
  hanging. **Slice 6** — the "I tried it locally → it's on my VPS" gap: `dev` *is* the serve command,
  and `docs/operators.md` now carries copy-paste **systemd** (unit + `MemoryMax=2G` + admin-token env)
  and **Docker** (multi-stage build + `docker run` with the nest dir mounted) recipes, plus the AI
  wiring. README gains a "Point an AI at it" section and links the deploy recipes. No new data
  capability — pure DX, honouring the binary+compose-only deployment scope. 154 lib tests, clippy
  `-D warnings` clean.
- **2026-07-19 - RFC-0015 slice 2: magical `init` — chain auto-detect.** `--chain` is now optional.
  Omit it and `init` probes every registered chain (mainnet → arbitrum-one → base) in parallel for the
  contract's `eth_getCode` and picks, in registry order, the first chain with bytecode there — so the
  user no longer has to know (or correctly spell) which chain their contract lives on. New
  `chains::all()` drives the probe; `detect_chain` fans `eth_getCode` across chains' default endpoints
  (best-effort per chain — an unreachable RPC reads as "not here", never a veto). Three outcomes:
  found-on-one (`✓ found on mainnet`), found-on-several (picks L1-first, discloses the others, `pass
  --chain to pick another`), found-nowhere (graceful bail nudging to explicit `--chain`/`--rpc`).
  Verified live: USDC → mainnet, ARB token → mainnet+arbitrum-one ambiguity note, `0x…dEaD` →
  graceful not-found. README quickstart drops `--chain`; chain list corrected to the three actually
  supported. 145 lib tests, clippy `-D warnings` clean.
- **2026-07-19 - RFC-0015 slice 3: live backfill feedback.** `dev` now shows a clean, honest sense of
  progress during the one long wait — the from-deployment catch-up — instead of log spam. New
  `progress::Backfill` reporter: on a TTY it draws a single carriage-returned line (`backfilling 73.2%
  │ block 18,400,000 │ 98,304 events │ 12,050 ev/s │ ETA 1m20s`, redrawn ~8×/s); piped/systemd gets a
  throttled 15 s heartbeat instead (never a stray `\r` in a journal); both end on a crisp `✓ caught up
  to tip at block N — X events in Ys, Z ev/s; now following`. ETA is computed from *block* rate so it
  stays meaningful over sparse wide-window ranges. Wired into both catch-up paths (the seal-direct bulk
  in `prepare` → "sealing history", and the hot window loop in `index_loop` → "backfilling"), fed
  per-window via a new `on_progress` callback on `backfill_direct_pipelined`/`_factory`. The reporter is
  pure presentation (no lock, no stored state — the deterministic core is untouched) and a `caught_up`
  latch fires the catch-up line exactly once, leaving steady-state tip-following silent. The old
  per-window/per-seal `info` lines (`blocks X..=Y: +N rows`, `sealed blocks…`) drop to `debug` — they
  were the spam; the watermark lives in `/metrics`. Verified live on USDC: clean single narrative,
  exactly one caught-up line. 152 lib tests + 6 e2e, clippy `-D warnings` clean.
- **2026-07-19 - docs: RFC-0016 (governed semantic layer + agent-grade MCP) and RFC-0017 (builder
  skill).** Two new RFCs record the AI-native workstream that turns "an agent *can* query nuthatch"
  into "an agent queries it *correctly, first try, and can prove it*." **RFC-0016** reframes the MCP
  server as a context-engineering problem (SQL is the IR, the agent is the compiler, nuthatch is the
  best compilation target) across five measure-first slices: an eval harness with a fixture nest and
  a pinned NL-question set scored by comparing *query results* (S1, gates everything); `semantic.toml`
  as authored-and-content-addressed nest input feeding an enriched `schema` tool (S2); errors-as-
  prompts + `explain` so agents self-correct in one round-trip (S3); results shaped for context
  windows with a provenance stamp (S4); resources + prompts (S5). Hard fence: nothing touches the
  data path, no LLM in core. **RFC-0017** is the complementary *builder* skill — a repo-installable
  `.claude/skills/` package teaching an agent to *drive* nuthatch (init/config/factories/roost/ops)
  before it has a nest, with CLI/config references generated from clap+serde and CI-checked for drift.
  RFC index + backlog cross-refs updated; RFC-0015 marked slice-1-shipped (the sql REPL, #83).
- **2026-07-18 - docs: consolidated backlog (infra track + RFC leftovers).** New `docs/backlog.md`
  gathers everything deferred/parked/not-done across RFCs 0001–0014 into one place (was scattered across
  fourteen "Non-goals"/"Open questions" sections). Four tracks: (1) **infra** — the shared blocker is a
  colocated reth node, which unblocks 0003 (ExEx) → 0014 (firehose traces/state); (2) **deferred
  engineering** gated on infra/benchmarks (0003, 0014, 0013 DataFusion); (3) **process** (grants 0006,
  launch 0007, the parked 0011 full graph-network migration); (4) **small increments** buildable now
  (proxy/EIP-1967 introspection, child-`end` conditions, SSE push, the 0012 live-parity proof). Notes
  the one node-independent 0014 slice worth building without the node (calldata decoder + `[extract]`
  config + schemas + volume guard). Linked from the RFC index.
- **2026-07-18 - RFC-0015 slice 1: the `nuthatch sql` REPL (first 0.5 "delightful core" work).** With no
  query argument, `nuthatch sql` now opens an interactive REPL — rustyline (history, line editing,
  Ctrl-C clears / Ctrl-D exits), dot-commands (`.tables`, `.schema <table>`, `.help`, `.exit`), and a
  formatted table per query; a query error prints but never ends the session. Refactored the query path
  into a `SqlBackend` (Local store when `dev` is stopped, HTTP fallback to the running instance when it
  holds the single-writer redb) opened once and reused across the whole REPL session. `.tables`/`.schema`
  are canned `information_schema` queries, so they work identically over both backends. This is the
  terminal-native query surface the RFC calls the single biggest UX lever — `init → dev → sql` and you're
  poking at your contract's data in a real REPL, no curl, no browser. Verified live on Arbitrum. 148
  tests, clippy `-D warnings` clean.
- **2026-07-18 - 🏷️ Release 0.4.0 — the hardening release.** Version 0.3.0 → 0.4.0. Cut after a
  five-dimension codebase audit and a sweep that fixed **2 critical security bugs** (blob-mount RCE,
  `/sql` arbitrary file-read), **2 high data-corruption bugs** (atomic seal/prune + structural SQL
  disjointness, schema-drift view survival), added the **first e2e test harness** (a `TapeSource`
  scripted-chain double closing the reorg-detection gap + the RFC-0012 roost-parity acceptance item),
  **batched the tip-loop writes** (one txn per window vs per row), and cleared the correctness (partial
  timestamps, column type-flip) and defensive (checked decode, nest-name validation, manifest fsync,
  error-body sanitisation) fixes. Shipped the **core delight**: `nuthatch sql "<query>"` — terminal-
  native SQL over the live tip ∪ sealed history, the down-payment on RFC-0015's "delightful core"
  direction. README rewritten to **sell the one thing nuthatch is best at** (contract → local SQL) with
  the full feature set below the fold; RFC-0015 records the 0.5 north star. Deferred items (benchmarks,
  larger perf refactors, COR-5, the remaining lows) recorded in [backlog.md](backlog.md) with rationale.
  151 tests (145 unit + 6 e2e), clippy `-D warnings` clean, footprint budget green.
- **2026-07-18 - 0.4.0 hardening: defensive fixes (audit low-severity, the ones that earn their churn).**
  **COR-11:** a decoded `uint≤64` with dirty high bits above its declared width would panic `to::<u64>()`
  and kill the ingestion task (single-log DoS on RPC-derived data); now saturates. **SEC-10:** roost nest
  names are restricted to `[A-Za-z0-9_-]` (they're both a path segment and a route prefix — no `/`/`..`/
  empty escaping the nests dir). **COR-9:** `save_manifest` now fsyncs the temp file's bytes before the
  rename and the dir entry after, so the atomic manifest swap survives power loss, not just `kill -9`.
  **SEC-11:** internal (500) error bodies returned raw anyhow chains with absolute paths (layout
  disclosure); now log the detail, return a generic message. 148 tests (+1), clippy clean. **Judged
  defer-worthy** (backlogged, with rationale): COR-5 factory-tip-cap recovery (fails *safe* — loud error,
  needs tip-loop surgery), COR-6 reserved-column collision (rare, needs a schema decision), COR-7 roost
  reorg fan-out blast radius (defensible under single-boundary), COR-8 i128-band balance drop (exotic),
  COR-10 `_seq` truncation (unreachable under gas limits), SEC-7 `WITH`-DML gate (ephemeral in-memory
  only), SEC-8 concurrent webhook delivery (nice-to-have), SEC-9 per-nest metrics (bigger refactor).
- **2026-07-18 - `nuthatch sql "<query>"` — terminal-native querying (the core delight).** The core user
  wants their contract's data; until now that meant HTTP `/sql` + curl. New one-shot `nuthatch sql`
  runs read-only SQL over the live tip ∪ sealed history and prints an aligned table (`--json` for
  piping). It's redb-lock-aware: queries the local files when `dev` is stopped, and transparently falls
  back to the running instance's HTTP `/sql` when `dev` holds the store — so the same command works
  whether or not the indexer is up. Same guarded engine as `/sql` (timeout, row cap, the SEC-2 file-read
  denylist). This is the 0.4 down-payment on the RFC-0015 "delightful core" direction — the full REPL is
  0.5. Also refreshed the badly-stale `main.rs` "walking skeleton" module doc. Verified live on Arbitrum
  (both the local-files and HTTP-fallback paths, table + json). 147 tests, clippy clean.
- **2026-07-18 - 0.4.0 hardening: SEC-5 admin token actually enforced.** `NUTHATCH_ADMIN_TOKEN` merely
  *enabled* the `/_admin` route off-localhost — it was never checked per request, so setting it to
  anything served the admin UI to the internet unauthenticated (security theater). Now off-localhost the
  route requires the token as `?token=…` on each request (401 otherwise); localhost stays open. Threaded
  a new `admin_token: Option<String>` through `build_nest`/`spawn_nest`/`spawn_roost` into `AppState`.
  Tests: token missing/wrong → 401, correct → 200; localhost open. 147 lib + 6 e2e tests, clippy clean.
- **2026-07-18 - 0.4.0 hardening: correctness fixes (COR-3 partial timestamps, COR-4 column-type flip).**
  **COR-3:** `rpc::block_timestamps` returned a *partial* map when a load-balanced/archive-vs-full RPC
  omitted a block's timestamp; the seal path then defaulted it to `block_timestamp=0` and sealed that
  permanently — breaking determinism (a re-run against a healthy endpoint yields a different content
  hash). Now a partial response is an error, so the window retries (like a total failure), never seals
  0. **COR-4:** the empty-table view typed columns by *storage kind* (`u64` → UBIGINT) while seal + the
  hot temp table type by *column name* (only the four counters are UBIGINT) — so a `u64`-storage event
  field like a `uint24 fee` flipped from UBIGINT (empty) to VARCHAR (once a row sealed), making the same
  `AVG(fee)` query valid then error (nasty for an AI writing SQL against the schema). Empty views now
  type by name too. Tests: empty-view-types-by-name; 146 tests, clippy clean. (Sweep continues: admin-
  token enforcement, factory tip-cap recovery, the low-severity list, benchmarks, README, the cut.)
- **2026-07-18 - 0.4.0 hardening: e2e test harness + batch-write throughput.** Two more sweep items.
  **E2E harness** (new `tests/`, first integration tests): a `TapeSource` scripted-mutable-chain double
  that actually answers `block_hash` and gives non-zero timestamps (the existing doubles return
  `block_hash → None`, making `detect_reorg` a no-op — the C1 gap), driving the full init→seal→serve→
  reorg pipeline offline + deterministically. Tests: land→seal→query golden (content-hashes byte-
  identical across runs), real-HTTP serve, **reorg convergence proptest** (reorged nest == a clean run
  over the post-reorg chain — closes C1), reorg-below-finality-halts (C3), **roost==solo byte-identical
  parity** (closes RFC-0012's open acceptance item), seal idempotence (COR-1). **PERF-2:** the tip loop
  committed one redb txn (an fsync) *per row* — 2,000 logs = 2,000 fsyncs, capping tip-follow far below
  the decode rate; now the whole window (rows + annotations + checkpoint + watermark) commits in ONE
  atomic txn (`Store::commit_window`), which is also *more* crash-consistent (a crash lands on a clean
  window boundary, never mid-window). 151 tests (145 unit + 6 integration), clippy `-D warnings` clean;
  the e2e reorg/crash-safety suite guarded the hot-path refactor. Sweep so far: security + data-
  corruption + e2e + write-throughput; still to come: bound the `/sql` hot scan, medium/low correctness,
  benchmarks, README, the 0.4.0 cut.
- **2026-07-18 - 0.4.0 hardening (audit-driven): security + data-corruption fixes.** A five-dimension
  audit of the codebase (coverage, core correctness, serving/security, performance, e2e) surfaced two
  critical security bugs and two data-corruption bugs, now fixed. **SEC-1:** `nest mount` trusted
  manifest file paths verbatim → a hostile blob (`../` or absolute path) could write outside the target
  (arbitrary file write); now only `Normal` path components are accepted. **SEC-2:** `/sql`'s read-only
  gate only checked the leading keyword, so DuckDB table functions (`read_text`/`glob`/…) could read any
  file the process can (leaking `nuthatch.toml` secrets); now a comment-proof function-call denylist
  refuses them, plus a `allowed_directories` lockdown for defense-in-depth. **COR-1:** `maybe_seal`
  advanced the sealed watermark then pruned hot in separate steps — a crash between them left a block
  range permanently in *both* layers (double-counted in `/sql` and every balance rebuild); prune +
  watermark are now one atomic redb txn (watermark last, idempotent re-seal), and `/sql` keeps hot∪cold
  disjoint *structurally* by `sealed_through` (cold = segments ≤ watermark, hot = rows > watermark).
  **COR-2:** `read_parquet` lacked `union_by_name`, so the first ABI-versioned column drift across
  segments made the whole table's view silently vanish; fixed. Tests: blob-traversal reject, `/sql`
  file-read reject (case + comment-split), overlap-not-double-counted, schema-drift-survives. 145 tests
  (+7 across the two PRs), clippy `-D warnings` clean. First fixes of the full 0.4.0 hardening sweep
  (medium/low correctness, perf, an e2e harness, and benchmarks still to come).
- **2026-07-18 - RFC-0013 §3: the hot tip is now SQL-queryable.** `/sql` used to see only sealed
  segments (DuckDB over Parquet) — the unsealed tip in redb was invisible to SQL, so a fresh-but-
  unsealed table read "does not exist". Now each table's DuckDB view is `sealed Parquet UNION ALL
  hot-tip`: the hot rows are scanned from redb (`Store::hot_rows_by_table`, one bounded scan — sealed
  rows are pruned from hot) into a per-table temp table (`analytics::load_hot_temp`) typed to match the
  Parquet exactly (four counter columns `UBIGINT`, the rest canonical-text `VARCHAR`; columns
  data-derived from the rows like `seal::rows_to_batch`, so no `schema.json` needed). Hot and cold are
  disjoint by block, so the union is exact — no dedup. `query_hot_cold` is the new `/sql` entry point;
  the cold-only `query_guarded` stays for trusted point-reads and the `/table` cold fill (which merges
  hot itself). **Chosen HOW — DuckDB, not DataFusion:** a dependency spike showed DataFusion is
  premature (under MSRV 1.85 cargo picks DataFusion 48 → arrow 55, clashing with our arrow 56; +~100
  crates atop a binary that already bundles DuckDB) — exactly the weight RFC-0013 §4 says to
  benchmark-gate first. So the §3 goal shipped on the engine we already have; a DataFusion
  `TableProvider` stays the documented, gated destination. **Gate met:** hot-only-queryable test,
  hot∪cold federation test (cold-only sees 2 sealed rows, hot+cold sees 3), clippy `-D warnings` clean;
  verified live on Arbitrum (`SELECT count(*)`, `GROUP BY` top-senders over unsealed `arb__transfer`).
  141 tests (+2).
- **2026-07-18 - RFC-0012 roost slice 7: example + operators docs — RFC-0012 COMPLETE.** A runnable
  two-nest roost example at `examples/roost/` (the ARB token + native USDC, both on Arbitrum One) with a
  README covering `roost dev`, the `/nests` roster + `/<name>/…` routing, footprint/`max_rss`, and the
  `nest pack`/`mount` blob flow; plus a "Roosts" section in `docs/operators.md`. **Verified live**
  against a public Arbitrum RPC (no paid quota): both nests mount under one shared cursor and index real
  transfers, and `/nests` reports **~110 MB resident** for the two-nest roost against a ~300 MB
  projection — comfortably inside the 2 GB per-runtime budget, and the honesty-rule RSS number the RFC
  asked for. **RFC-0012 is now Implemented — all 7 slices** (§0 brief amendment; roost layout/serving,
  shared cursor, factory nests, shared reorg fan-out, footprint model; `nest pack`/`mount`; docs +
  example). One nest, one command still works unchanged; N nests on one chain now share a cursor, a
  reorg boundary, and a footprint budget — per-nest tables byte-identical to solo by construction. The
  single open acceptance item is a sustained byte-identical-vs-solo parity run over a longer range
  (holds by construction — the shared cursor runs the same per-window code as solo `dev`).
- **2026-07-18 - RFC-0012 roost slice 4: per-runtime footprint model.** The density-honesty piece.
  `roost.toml` gains an optional `max_rss_mb` (default 2048 — the CLAUDE.md ≤2 GB per-runtime budget).
  Before starting, `roost dev` computes a rough RSS **projection** — a fixed roost base (120 MB, paid
  once for serving + runtime) plus, per nest, a base (90 MB: hot store + decode registry + the always-on
  balance view) and a 40 MB chunk per active IVM view (exposure if the nest has labels, velocity if
  flagged, the discovered-child registry if it's a factory) — logs it, and **refuses the mount** with an
  actionable message if it would exceed `max_rss`. The `/nests` roster now carries per-nest
  `estimated_rss_mb` plus the roost's `projected_rss_mb`, `max_rss_mb`, and the **real** `rss_bytes`
  (reusing `metrics::rss_bytes()`), so the estimate can be calibrated against measurement — the honesty
  rule: the model is labelled an estimate, the refusal is a real gate. **Gate met:** `estimate_nest_rss_mb`
  scales-with-views unit test; boot smoke — two static nests project 300 MB, and `max_rss_mb = 150`
  refuses with "projects ~300 MB but max_rss is 150 MB — raise max_rss, drop a nest, or split". 139
  tests (+1), clippy `-D warnings` clean. **RFC-0012 slices 1–6 are now complete**; only slice 7 (a
  runnable two-nest example + operators docs) remains. Live multi-nest RSS + table-parity numbers come
  from the same live acceptance run as 2a.
- **2026-07-18 - RFC-0012 roost slice 3: shared reorg detection + fan-out.** The last piece of the roost
  core. `handle_reorg` was split into detection (`detect_reorg`, unchanged) and a sync
  `rollback_reorg(ancestor)` (retract the three IVM views, drop reorged children, roll back the hot
  store). A solo nest still detects on its own cursor; the roost now detects a reorg **once** — at the
  most-caught-up nest's boundary (every tip nest checkpoints the same blocks with the same hashes, so
  any is a valid reference) — and fans `rollback_reorg` out to every mounted nest. One detection (a
  handful of block-hash calls) instead of N, one observable reorg boundary. The subtle bug a naive
  fan-out would introduce: bumping a *behind* (still-backfilling) nest's `LAST_BLOCK` up to the ancestor,
  claiming blocks it never indexed — so `rollback_reorg` no-ops for any nest already at/below the fork,
  leaving its cursor put. Finality was already shared (one finality height per chain drives every nest's
  sealing). Behaviour-preserving for solo `dev`: the split is transparent (the guard can't trigger when
  a nest detects on its own cursor), and every reorg property test passes unchanged. **Gate met:** the
  existing store-level reorg proptests + golden, plus a fan-out test — two nests at different heights,
  one shared reorg: the caught-up nest rolls back to the fork, the behind nest is spared with its cursor
  uncorrupted. 138 tests (+1), clippy `-D warnings` clean. Live multi-nest reorg convergence over a
  chain folds into the same live acceptance as 2a. **The roost core (slices 1–3) is complete** — layout,
  serving, shared cursor, factory support, shared reorg. Remaining RFC-0012 work is slice 4 (footprint
  model: pre-mount RSS estimate + `max_rss` refusal + per-nest `/metrics`) and slice 7 (a runnable
  two-nest example + operators docs).
- **2026-07-18 - RFC-0012 roost slice 2b: factory nests in a roost.** Lifts the slice-2a restriction —
  factory/template nests (RFC-0009) can now be co-mounted with static nests under the shared cursor.
  `NestIngest::owns` gained a second demux mode: a **static** nest (non-empty address filter) routes a
  log by emitting **address**; a **factory** nest (empty filter — topic0-only, children discovered at
  runtime) routes by **topic0**, so it catches its factory-creation events and its discovered children
  regardless of their (arbitrary) addresses. `union_filter` now drops the address filter for the whole
  fetch if *any* mounted nest is a factory — an empty `getLogs` address list means "any address", which
  the factory needs; static co-tenants over-fetch but demux back to exactly their own logs, so per-nest
  output stays byte-identical to solo. The decode + inline child-discovery path (`decode_window`/
  `process_window`) is unchanged — each factory nest discovers its own children from its own routed logs
  exactly as it does solo. The `spawn_roost` factory refusal is removed. **Gate met:** `log_owned`
  both-modes test, `union_filter`-goes-topic0-only-with-a-factory test (137 tests, +2), clippy `-D
  warnings` clean; boot smoke with a factory + static nest co-mounted (factory recognised — "1 template,
  1 rule, topic0-only tip fetch" — both mount under one cursor, API live). Live factory-in-roost
  child-discovery parity over a chain folds into the same live acceptance as 2a. Next: slice 3 — shared
  reorg detection + fan-out (one detection → every nest converges), the last piece of the roost core.
- **2026-07-18 - RFC-0012 roost slice 2a: the shared cursor (one `getLogs` feeds N nests).** The
  density win. `nuthatch roost dev` now drives every mounted nest from ONE cursor: `indexer::spawn_roost`
  builds each nest and spawns a single `roost_index_loop` that does one `source.tip()` + one **union
  `getLogs`** per window, then demuxes each returned log to the nest(s) that own it (by emitting address,
  `NestIngest::owns`) and runs it through the **same** `process_window` a solo `dev` uses — so per-nest
  tables are byte-identical to running each nest alone, by construction rather than re-implementation.
  Backfill stays per-nest (each nest `prepare`s its own history; the cursor only couples at the tip); a
  `min`-based global cursor lets nests mounted at different heights self-heal, and a nest with zero owned
  logs in a window still advances + checkpoints + seals exactly as it would solo. Two behaviour-
  preserving refactors set this up first (the `NestIngest` extraction, then `index_loop` taking a
  `NestIngest` + a reusable `prepare` method) so the solo and roost paths literally share code — the live
  Helsinki `dev` deploy runs the identical path, unchanged. **Gate met:** demux unit tests (`owns` case-
  insensitive, `union_filter` dedups across nests, demux-reproduces-the-solo-address-filter), two-nest
  boot smoke (mounts both nests, one shared cursor, API live, per-nest `/<name>/…` routing), bogus-RPC
  behaviour identical to solo (no regression). 135 tests (+3), clippy `-D warnings` clean. The full
  **live** two-nest table-parity run over a real chain is the remaining acceptance evidence (folds in
  with a live demo, as the RFC-0011 pilot proved delegation parity byte-for-byte). Chosen HOW: static
  nests only — **factory nests are refused in a roost** (`spawn_roost` bails with a clear message),
  deferred to slice 2b, because their topic0-only discovery would force the whole union fetch topic0-only
  and tangle the address demux. Reorg is still per-nest here; shared detection + fan-out is slice 3.
- **2026-07-18 - RFC-0012 roost slice 2a groundwork: extract `NestIngest` from `index_loop`.** A
  strictly behaviour-preserving refactor — the enabling step for the shared cursor. The single-nest
  tip-following loop's per-nest state (store, decode registry, the three IVM views, labels/screener,
  flags, alerts/webhooks, factory + discovered-child registry, finality, the getLogs filter) is grouped
  into a `NestIngest` struct with two methods: `handle_reorg` (detect + retract views + drop children +
  roll back the hot store) and `process_window` (decode → store → IVM-feed → screen → checkpoint → seal
  → deliver webhooks). `index_loop` now builds one `NestIngest` and drives it; the `--seal-direct`
  phase-0 backfill, warm-restart, cold-start cursor, adaptive chunker and getLogs fetch stay inline. The
  point: the coming shared-cursor driver (slice 2a) will drive *N* `NestIngest`s through the **same**
  per-window code, so "byte-identical vs solo" holds by construction rather than by re-implementation.
  Zero behaviour change (so zero risk to the live Helsinki deploy): all 132 tests pass unchanged
  (reorg property + golden-decode tests included), clippy `-D warnings` clean, fmt clean. The extraction
  was done under a tight behaviour-preserving spec and every moved line reviewed against the original
  (notably: the reorg/seal path, and the `next = to + 1` advance which moved to the caller — verified
  nothing in the seal tail reads `next`). Next: the shared cursor itself — one poll, union filter,
  demux each log to the owning nest's `NestIngest`, with path-equivalence as the gate.
- **2026-07-18 - RFC-0012 roost slice 1: layout + serving (`nuthatch roost dev`).** The first slice of
  the multi-nest runtime — a **roost** hosting many nests on one chain. A `roost.toml` (`[roost]`
  chain/chain_id/rpc_urls + a `nests` list) at the roost root names the shared chain and the mounted
  nests, each a `nests/<name>/` directory exactly as a standalone nest. `roost dev` brings them all up
  and serves them behind one listener: a `GET /nests` roster plus every nest's full API under its
  `/<name>/…` prefix (`serve::run_roost` + `Router::nest`; the per-nest routes are byte-identical to a
  solo `dev`, just prefixed). Chain identity is **hoisted to the roost** (it's what the shared cursor
  will key on) and every mounted nest's `[nest].chain`/`chain_id` is validated against it — a mismatch
  is a hard error, because a different chain needs its own roost (its own cursor). Deliberately naive on
  ingestion this slice: **one cursor per nest**, to land routing + per-nest isolation before the
  shared-cursor collapse (slice 2, where path-equivalence is the gate). Stores stay per-nest and
  isolated (own redb/segments/views) — one nest's bad view or runaway factory can't touch another's
  data (the CLAUDE.md non-negotiable). Refactored `indexer::run` into `spawn_nest` (build a nest's
  serve state + background tasks, minus binding a listener) + a thin `run` (serve + fate-share), so solo
  `dev` is unchanged (it's the roost-of-one) and the roost fate-shares the server with *all* nests'
  ingestion via `select_all`: any nest's loop dying exits the whole process non-zero (the
  single-failure-boundary rule, generalised). **Gate met:** valid-load, wrong-chain-reject,
  reserved/duplicate-name-reject, empty-list-reject tests; solo `dev` regression-clean (existing suite +
  live Helsinki deploy path unchanged); binary smoke (subcommand help, missing-`roost.toml` → exit 1
  with a clear error). 132 tests (+4), clippy clean. Chosen HOW: new `roost` command group + `src/
  roost.rs` (kept `dev` pristine rather than reworking it into a roost internally — lower risk to the
  live path); nest names validated against reserved routes (`/nests`, `/health`) since roster and nest
  prefixes share one path namespace. Next: slice 2 — collapse to one shared cursor per chain (union
  filter, `(nest, contract)` fan-out routing), with byte-identical-vs-solo as the acceptance gate.
- **2026-07-18 - RFC-0012 slice 6: `nuthatch nest mount` (verify + install a nest blob).** The other half
  of the pack/mount round-trip. `nest mount <blob> [--dir <target>] [--expect <hash>]` resolves a blob
  from the **local** filesystem (no network — the no-phone-home line holds by construction; a BYO
  transport stays a later wrapper), then verifies before installing: rejects a `blob_format_version`
  this build doesn't understand, checks the optional `--expect` content address *before* touching disk,
  and hashes every file against the manifest. It installs into `./<nest_name>/` (or `--dir`, refusing a
  non-empty target), then runs `verify_registry_reproduces` — regenerating the decode registry from the
  *installed* inputs and asserting it equals the manifest's pinned `registry_hash`. So a mount that
  succeeds proves the nest decodes exactly as its author's did: determinism carried across the wire, not
  just the disk. **Gate met:** pack→mount round-trip (registry reproduces from installed inputs),
  wrong-`--expect` reject (fails before disk), tampered-file reject, newer-format reject. 128 tests
  (+3), clippy clean. Chosen HOW: standalone verb installing a blob into a plain directory — the roost
  half of RFC-0012 slice 6 (mount *into a roost*, two nests one cursor) waits on the roost runtime (§2),
  itself gated on the §0 CLAUDE.md amendment. Next: the roost — starting with that amendment.
- **2026-07-18 - RFC-0012 slice 5: `nuthatch nest pack` (content-addressed nest blob).** First step of
  nest packaging — the deploy unit. `nest pack <dir>` bundles a nest's *authored inputs* (config, ABIs,
  views, labels, skills, schema, llms.txt) plus a canonical `manifest.json`, and prints the **blob
  hash** = `sha256` over the canonical manifest (fixed field order, files sorted by path, compact
  encoding — a Merkle root over the inputs). The manifest pins the *expected* `registry_hash`
  (regenerated from the inputs, not a stored artifact) + the generator version, so a later `mount` can
  reproduce the decode and assert it matches — extending determinism from the data path (RFC-0009's
  content-addressed segments) to the *authoring* path. Derived/runtime files (`nuthatch.redb`,
  `segments/`) are excluded; the blob hashes inputs only. New `src/blob.rs`, `nest` command group (kept
  distinct from the RFC-0008 compliance `pack`). **Gate met:** determinism test (same inputs →
  byte-identical canonical manifest + blob hash), registry-hash-pins-and-verifies test, changed-input-
  changes-hash test, derived-files-excluded test; verified live packing the real graph-gns-nest (blob
  `8572817d…`, reproducible). 125 tests (+3), clippy clean. Chosen HOW: blob is a content-addressed
  *directory* (dep-free, trivially reproducible; identity is the manifest hash, so a single-file tar
  container is a later wrapper). Next: slice 6 (`nest mount` — resolve + verify + install), then the
  roost (§2, which needs the §0 CLAUDE.md amendment first).

- **2026-07-18 - RFC-0011 pilot SHIPPED to production, then parked.** Two Lodestar panels now serve
  live from nuthatch instead of The Graph: the **delegation-activity feed** (HorizonStaking, byte-
  identical parity) and the **developer-activity chart** (L2GNS `SubgraphPublished`, ~0–1% documented
  divergence). Both on one Hetzner box (~86 MB RAM, two nests, Caddy TLS + basic-auth, path-routed to
  one URL), flag-gated with automatic subgraph fallback, source-aware "⚡ Indexed by nuthatch" badges.
  Verified in production + by an independent browser test. Writeups published on both the Lodestar blog
  and the new nuthatch blog (`nuthatch-frontend` gained a content-collection `/blog`). **The wedge is
  proven — a real dashboard with users, off The Graph, on an indexer we run.** Parked here: RFC-0011 as
  written is much bigger (published `graph-network-nest` repo, ~6 more panel groups, GraphOps-primary +
  Hetzner-shadow shape, env end-state, ingest-cron deletion, `checks/*.sql` parity harness, 30-day
  soak). Full done/not-done ledger written into the RFC-0011 doc's new "Implementation status" section.
  Natural resumption: step 2 (Indexer Directory) or promoting the ad-hoc nests into a published nest
  (overlaps RFC-0012). Next focus: **RFC-0012 (multi-nest runtime + nest packaging).**

- **2026-07-18 - Release v0.3.0.** Rolls up the whole robustness pass since v0.2.2 into a release the
  Helsinki deployment can run: the resumable/fail-fast backfill (C1), timestamp-retry (H4), pipelined
  shrink-retry + livelock floor (H2/H3), reorg-below-finality halt + detection fallback (M6/M7), atomic
  manifest (M8), and lazy IVM views (L10). The one deferred finding, M5 (redb write batching + moving
  seal/DuckDB off the async workers), is a benchmarked follow-up. Deployed to the box, replacing v0.2.1.

- **2026-07-18 - Backfill review M6/M7/M8/L10: reorg halt, checkpoint blind spot, atomic manifest, lazy views.**
  The medium/low findings. **M6:** a reorg whose common ancestor is *below* the sealed watermark is a
  finality violation this model can't repair (the doomed blocks are already immutable and pruned from
  hot) — it now halts loudly instead of silently producing an incomplete retraction and a sealed layer
  that disagrees with the chain. **M7:** `detect_reorg` gave up ("nothing to verify") when the top
  boundary had no stored hash (a transient `block_hash` failure at checkpoint time), a reorg blind spot;
  it now falls back to the newest checkpoint it *does* have. **M8:** the segment manifest — the
  catalogue a `kill -9`-survivable binary lives or dies by — was written in place, so a crash mid-write
  orphaned every `.parquet`; now written to a temp file and atomically `rename`d over. **L10:** the
  exposure and velocity IVM views spun up a DBSP circuit + dedicated thread for *every* nest, even ones
  with no labels / no velocity flag; `start(enabled)` now skips the circuit + thread when the nest can't
  feed it (apply no-ops, snapshot stays empty) — real relief on the ≤2 GB budget for a plain nest.
  122 tests, clippy clean. (The one remaining review finding, M5 — batch the per-row redb fsyncs and
  move seal/DuckDB work off the async workers — is a bigger perf refactor, deferred to its own slice.)

- **2026-07-17 - Backfill review H2/H3/H4: timestamp retry, pipelined shrink-retry, livelock floor.**
  The next batch of the deadlock-review findings, all "a transient hiccup quietly corrupts or hangs".
  **H4:** a whole-batch `block_timestamps` failure was `unwrap_or_default()`ed into an all-zeros map and
  sealed - baking `block_timestamp = 0` into immutable segments from a 5-second blip. Now the batch
  retries (4×, backing off) and, if it still fails, returns `Err` instead of zeroing: the backfill
  propagates (and resumes cleanly via C1), the tip loop skips and re-fetches the window. **H2:** the
  pipelined backfill used a fixed window with no shrink-retry, so an oversized `--window` against a
  capped provider aborted the whole run (while the sequential/factory paths quietly self-corrected -
  same flag, different behaviour). New `fetch_logs_splitting` halves the range and retries on a "too
  many results" cap, so the pipelined path self-corrects too. **H3:** every adaptive `too_large` arm
  floored the window at 1 block and retried forever - a single block whose logs exceed the provider cap
  was an infinite hang. All of them (sequential, both factory passes, tip loop, and the new splitter)
  now stop loudly with "block N alone exceeds the provider's getLogs result cap" instead of looping.
  **Gate met:** `fetch_logs_splitting_shrinks_then_fails_on_a_single_block` (100-block range against an
  8-block cap splits and returns all 100; a single over-cap block errors loudly). 122 tests (+1), clippy
  clean.

- **2026-07-17 - Backfill review C1: resumable seal-direct backfill + fail-fast lifecycle.** A critical
  review (prompted by "anything else like the deadlock?") turned up a family of silent-failure bugs;
  this fixes the worst. (1) **Fail-fast lifecycle:** `run()` previously awaited only the HTTP server and
  merely `abort()`ed the ingest task afterwards - so if indexing died (error or panic) the process kept
  serving stale data looking healthy. Now `run()` `select!`s over serve and ingest; an indexing
  error/panic propagates out as a non-zero exit. (2) **Resumable backfill:** the sealed watermark
  (`SEALED_THROUGH`) is now persisted after *every* segment (via an `on_seal` callback threaded into
  `backfill_direct_pipelined`/`_factory`), and phase-0 resumes from `watermark + 1` instead of
  restarting from `origin`. Before, any mid-backfill blip (one transient `getLogs` error) threw away the
  whole run, and re-running from origin on the *adaptive factory path* (non-deterministic window
  boundaries) could re-seal overlapping ranges under fresh content hashes - permanently double-counted
  segments. Resuming re-fetches nothing already sealed, so no duplication. **Gate met:**
  `resume_from_watermark` + `backfill_resumes_from_the_sealed_watermark` unit tests; the existing
  path-equivalence/determinism tests still green. 121 tests (+1), clippy clean. (Remaining review
  findings - timestamp-retry, pipelined shrink-retry/livelock, blocking-work offload, atomic manifest,
  reorg-below-finality halt, conditional IVM views - are the next slices.)

- **2026-07-17 - Fixed the seal-direct backfill deadlock (single-endpoint concurrency guard), v0.2.2.**
  Root-caused the backfill hang from the previous entry. It was **not** the RPC being slow, not the DBSP
  runtimes, and not core count - it was **high concurrency to a *single* RPC host**. A
  `--concurrency N` seal-direct backfill fires N `getLogs` (plus batched `block_timestamps`) at once;
  aimed at one host that stalls the whole tokio runtime - a lost wakeup that parks every worker and
  never fires, so even the 20s per-request timeout can't rescue it and the backfill hangs forever.
  Reproduced deterministically at `--concurrency 8` against a single URL (and *never* with the default
  3-4 endpoints, which spread the requests over separate connections). Confirmed by thread sampling:
  all workers parked, one on the idle I/O driver, zero in-flight requests, zero app frames. My earlier
  "environment-sensitive / mac-vs-box" read was wrong - every failing run happened to be pinned to a
  single endpoint (an arb1-only workaround), every passing run had several. **Fix:** cap the seal-direct
  backfill to sequential (`--concurrency 1`) when only one RPC endpoint is configured, with a warning to
  add endpoints for a parallel backfill; two or more keep the requested concurrency. Single-endpoint
  backfills are now slower but *finish* instead of hanging. **Gate met:** `safe_backfill_concurrency`
  unit test + reproduced the deadlock (concurrency 8, one host → hang), then verified the capped path
  drains steadily to completion (RPC counter climbing, not frozen). 120 tests green (+1), clippy clean.
  Released as v0.2.2. (The underlying reqwest/tokio stall under high single-host concurrency is a
  deeper issue parked behind the guard - worth a proper upstream-style repro later.)

- **2026-07-17 - Lodestar developer-activity panel live on nuthatch + a deadlock found.** Second
  Lodestar panel migrated (after the delegation feed): the Developer Activity chart (subgraphs
  published/week) now comes from a nuthatch nest indexing L2GNS `SubgraphPublished` on Arbitrum One.
  Validated to exact weekly parity on short windows and ~1% total divergence over 12 months (a handful
  of L1-origin/legacy subgraphs the network subgraph folds in that native L2 publishes don't emit) -
  documented, not silent, per RFC-0011 §2. The README now names Lodestar as the first production user.
  **Known bug found, not yet fixed:** the large static seal-direct backfill (125M Arbitrum blocks,
  `--window 50000`, `--concurrency 8`) **deadlocks** on the Helsinki box - the process parks in
  `futex_do_wait` with zero network activity, having sealed nothing. The *same binary and nest* backfill
  cleanly on a dev laptop, so it's an environment-sensitive race (timing/CPU), not the RPC (arb1 serves
  the box fast under sequential *and* concurrent load). Worked around by backfilling on the laptop and
  shipping the content-addressed segments + cursor to the box (which then only tip-follows - that path
  is fine) - which is really a proof of the portable-segments design, but the deadlock needs a real fix.
  Suspect the DBSP merger runtime interacting with the pipelined seal loop over many buffered windows.

- **2026-07-17 - `--window` override for sparse-contract backfills (RFC-0004 follow-up), release v0.2.1.**
  The static seal-direct backfill uses a fixed `eth_getLogs` block-window (the chain default, e.g. 2000
  on Arbitrum) - fine for a dense contract, but for a *sparse* one over a long range it means tens of
  thousands of near-empty requests. Backfilling 12 months of Graph GNS `SubgraphPublished` events (for
  Lodestar's developer-activity panel) at the 2000 default was ~62,000 requests / ~100 min; with
  `--window 50000` it's ~2,500 requests / **~4 min**, same 3,411 events. So `dev` gains a `--window`
  flag that overrides the chain default (a zero is ignored). Small, principled, and it's the same wall
  the eventual full graph-network-nest (RFC-0011) would hit. **Gate met:** `effective_window` unit test
  (override wins, zero ignored, default otherwise) + verified live (125M-block Arbitrum backfill in 4
  min). 119 tests green (+2), clippy clean. Released as v0.2.1.

- **2026-07-17 - Release v0.2.0.** Cuts a current release (v0.1.0 predated everything from RFC-0008
  onward). Since 0.1.0: the compliance pack (RFC-0008, screening/flags/alerts/effectful stages/signed
  manifest/audit), factories & dynamic discovery (RFC-0009, "it indexes Uniswap"), the admin UI +
  user webhooks + HMAC-signed egress (RFC-0010), and the per-contract event allowlist (RFC-0011). The
  release job builds `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` binaries. Cut because the
  first real deployment (serving Lodestar's delegation-events feed from a nest on the Helsinki box)
  should point only at a released build, per RFC-0011 §5.

- **2026-07-17 - RFC-0011 kickoff: the per-contract event allowlist (the blocking core amendment).**
  RFC-0011 (the graph-network nest + Lodestar migration) is mostly cross-repo work, but it forces one
  small change on RFC-0001 that blocks everything else: a per-contract `events = ["Transfer", ...]`
  allowlist in `[[contracts]]`. Without it, a nest that includes GraphToken (L2GRT) would index millions
  of irrelevant `Transfer` rows. So this ships that amendment: `Contract.events` (default empty = index
  every event, fully backward-compatible) threads into `ContractSpec` and a filter in the decode
  registry's `register_events`. Because the getLogs topic0 set is *derived from the registry decoders*,
  filtering the decoders narrows both the decode **and** the fetch - an unlisted event's topic0 isn't
  even requested. A typo (an event the ABI doesn't define) is a loud build error listing the known
  events, never a silent "indexes nothing" - which is the whole point at that scale. The registry
  content-hash reflects the filtered model, so a narrowed nest is honestly a different (smaller) data
  model, not the same one pretending. **Gate met:** allowlist-restricts-tables-and-topics test (2
  events → 1 table, 1 topic0, Approval no longer decodes), hash-changes test, unknown-event-build-error
  test, config round-trip test. 115 green (+3), clippy clean. The nest authoring + Lodestar panel
  migration + GraphOps/Hetzner shadow are the next, cross-repo, steps.

- **2026-07-17 - RFC-0010 Part B: signed webhook egress (HMAC).** The last egress-security piece: a
  `[[webhooks]]` entry can now carry a `secret`, and every delivery for it is signed with an
  `X-Nuthatch-Signature: sha256=<hex>` header so a receiver can verify the payload came from this nest
  and wasn't tampered with. HMAC-SHA256 is hand-rolled over `sha2` (RFC 2104, no new dependency) and
  proven against the standard reference vector. Crucially the signature covers the *exact bytes POSTed*:
  the worker sends the stored payload string verbatim and signs those same bytes, so there's no
  serialization skew between what's signed and what's sent. It threads through the one shared delivery
  worker, so both producers - compliance alerts (C5) and user webhooks - get signing for free; a webhook
  with no secret goes unsigned, unchanged. **Gate met:** HMAC known-answer test + a live-receiver test
  asserting the secreted webhook arrives with a correct `sha256=` header over the received body while an
  unsecreted one arrives bare. 112 tests green (+2), clippy clean. RFC-0010's tip-finality delivery path
  (hot, not-yet-sealed rows + retractions) stays deferred - it needs a hot-row SQL surface, which is a
  scaled-mode architectural piece, not a webhook detail.

- **2026-07-17 - RFC-0010 Part A: the built-in admin UI.** A single self-contained page (`src/admin.html`,
  ~13 KB, embedded via `include_str!`), served at `/_admin/`, no framework, no CDN, zero external
  requests - it only talks to this same-origin API. Four tabs: **Status** (live gauges - hot rows,
  tables, holders, exposure/velocity/outbox, IVM views - polled every 2s while visible, with a live tip
  dot), **Tables** (a browser: click a table, see its newest 100 rows merged hot+sealed), **SQL** (a
  runner over the read-only `/sql` surface, Ctrl/⌘-Enter to run), and **Nest** (a new `/nest` endpoint:
  contracts, templates, factories, webhooks, registry hash). Read-only by design - the UI observes, the
  CLI and files mutate. On by default on localhost; off-localhost it needs `NUTHATCH_ADMIN_TOKEN` or it
  self-disables with a log line, and `--no-admin` removes it entirely (for hosted deployments fronting
  their own dashboard). **Gate met:** served-when-enabled / 404-when-disabled test, `/nest` metadata
  test, and a budget test asserting the embedded UI is < 150 KB and makes no external requests. Rendered
  live against a real USDC nest (screenshot: clean dark UI, live data). 110 tests green (+2), clippy clean.

- **2026-07-17 - RFC-0010 Part B: user webhooks (the shared-engine second producer).** RFC-0010's
  reconciliation is "one delivery engine, two producers" - and the engine already exists (I built the
  durable outbox + at-least-once worker host-side in RFC-0008 C5). So this adds the second producer:
  `[[webhooks]]` config (name/table/where/url/batch_max/finality/since) and a `webhooks.rs` table
  predicate producer that, when a range seals, queries the newly-sealed rows of a webhook's table
  matching its `where`, batches them by `batch_max`, and pushes them onto the same outbox - the same
  worker POSTs them, never blocking the loop. The RFC's correctness catch is handled: `since` cursors
  each webhook (persisted in the hot store), and the default `"registration"` starts it at the tip so a
  `--seal-direct` backfill sealing all of history does *not* fire millions of rows; `"genesis"`/a block
  replays deliberately. **Gate met:** registration-suppression test (backfill history below the cursor
  delivers 0, a post-registration row delivers 1) + genesis-replay-with-where-and-batching test.
  Verified live on USDC: a `where CAST(value AS HUGEINT) > 500000000000` webhook delivered 114 big
  transfers in 14 batches of 10 to a real receiver, outbox drained clean. 108 tests green (+2), clippy
  clean. *(Still to come in Part B: the tip-finality path + retraction deliveries, HMAC signing, and
  folding the C5 alerts into this as webhook-over-annotation-tables. Part A - the admin UI - is a
  separate build, best done with design eyes on it.)*

- **2026-07-17 - RFC-0009 step 6: the `{template}__children` view + docs (RFC-0009 COMPLETE).** The
  finale of dynamic contract discovery. Every template now gets an auto-generated **`{template}__children`**
  DuckDB view over the sealed factory events - the discovered children with provenance (address,
  discovered_block, discovered_log_index, discovered_timestamp, parent_address), de-duplicated to the
  earliest discovery per address (`define_children_views` reads the nest's factory config, unions the
  factory tables per template, `QUALIFY row_number()` picks the earliest). So `SELECT * FROM
  "pool__children"` answers "which pools, discovered when, by which factory" in one query, and
  `discovered_timestamp` answers "pools created this week" without a join. New `docs/factories.md`
  (config, how tip/backfill/reorg/scale work, the children view); README status row; MCP schema hint.
  **Gate met:** children-view test (two pools + a duplicate discovery → 2 distinct, earliest wins,
  provenance columns correct). Verified **live on Uniswap V3**: `pool__children` listed **83 discovered
  pools** with block + real-timestamp provenance. 106 tests green (+1), clippy clean. **RFC-0009 is
  complete** - factories work at the tip, over history, reorg-safely, at scale, with a queryable
  children view; **it indexes Uniswap.** (Step 5a - ExEx-mode factories - is inherent via the
  local-filter path; explicit bridge-harness test remains a small `exex`-feature follow-up.)

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
