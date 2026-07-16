# RFC-0010: The admin UI and webhooks — ease-of-use parity

- Status: Draft (v2)
- Author: Pete (cargopete)
- Date: 2026-07-16 (v1: 2026-07-14)
- Depends on: RFC-0001 (Implemented — schema manifest drives the UI); RFC-0005 v2 §6
  (/metrics and query guards now live there, not here)
- Blocks: nothing; RFC-0008 C5 now *consumes* Part B's engine (see §Reconciliation)
- Priority (revised): admin UI strongly recommended before the public launch phase
  (RFC-0007 v2 Phase 2) — it is the screenshot; webhooks v0.2, EXCEPT the shared
  delivery engine, which lands whenever RFC-0008 C5 needs it (whichever comes first).
- Revision note: v2 reconciles Part B with the compliance pack's alert sink (one
  delivery engine, two producers), repoints /metrics to RFC-0005 v2, fixes the SSE
  assumption (streaming subscribe is not shipped; the UI polls until it is), adds
  the backfill-suppression rule for sealed webhooks (a correctness catch created by
  `--seal-direct`), and adds the operator-mode UI switches.

## Abstract — unchanged in spirit

A built-in, local-only admin UI (the PocketBase move) and outbound webhooks with
honest finality semantics. Both inside every non-negotiable: compiled into the single
binary, zero external resources, no phone-home, deterministic core untouched.

## Reconciliation with RFC-0008 C5 — NEW, and the reason this v2 exists

v1 of this RFC and RFC-0008's Slice C5 (alert-webhook effectful component) specify
the same operational machinery twice: durable outbox, at-least-once with retries,
delivery cursor, retraction events, never-block-the-indexer. Decision:

- **One delivery engine, host-side, built once** (this RFC's Part B): outbox in the
  hot store, ordered per-subscription delivery, bounded retry + dead-letter,
  retraction deliveries, HMAC signing, explicit host allowlist. It is operational
  machinery outside the deterministic core — exactly the kind of thing that should
  NOT be a WASM component (there is no purity/audit benefit to sandboxing our own
  egress loop; the audit artifact for egress is the *configured host list*, which
  the pack manifest already records).
- **Two producers feed it**: (1) user `[[webhooks]]` over event tables (this RFC);
  (2) compliance annotation deltas (`sanction_hit`, `threshold_flag`,
  `flag_retracted`) — RFC-0008 C5 therefore SHRINKS to configuration: an alert route
  is a webhook subscription over annotation tables, declared in the pack manifest.
  The effectful-component version of C5 is dropped; C4 (effectful worlds) remains
  fully motivated by enrichment components, unaffected.
- RFC-0008 needs a two-line v3 amendment recording this. Net effect: the compliance
  pack gets simpler and this engine gets a second, demanding customer on day one.

## Part A — Admin UI

### Goals — unchanged from v1

(Table browser with hot/cold provenance; SQL runner with history/EXPLAIN/export;
status page; view inspector; nest inspector incl. factories + child counts +
registry hash when RFC-0009 lands. Read-only in v1 — the UI observes, the CLI and
files mutate.)

### Non-goals — updated

Dashboarding/charting (Grafana exists). Multi-user auth. ~~/metrics formalized
here~~ — /metrics is specified and owned by RFC-0005 v2 §6 (operator channel); the
UI's status page consumes the same numbers over JSON, it does not define them.

### Design — three revisions

- Served from the binary at `/_admin/`, rust-embed assets, no framework, one JS +
  one CSS file, <150 KB embedded, consumes only public endpoints (plus read-only
  `/views`, `/nest`) — all unchanged from v1.
- **Refresh model (fixed):** v1 assumed SSE; streaming subscribe is NOT shipped
  (deferred in slice 5). The status page and table counts poll (default 2 s,
  visible-tab only); when streaming lands, the UI upgrades transparently. Do not
  block the UI on the streaming feature.
- **Binding rule (extended for operators):** unchanged locally (127.0.0.1 default;
  public bind requires `NUTHATCH_ADMIN_TOKEN` or the UI self-disables with a log
  line). NEW: `admin = false` in toml (or `--no-admin`) removes the routes and the
  embedded assets from memory entirely — hosted deployments (GraphOps) will want
  the UI off at the tenant edge and their own dashboards in front; per RFC-0005
  v2's dividing line, tenant-facing UI is the operator's layer.
- **The demo moment:** unchanged — backfill visibly filling tables is the
  screencast's second act. With `--seal-direct` now in `dev`, the status page should
  show the phased cold start explicitly (fast-seal progress, then live tail) —
  the 20× is invisible in curl output and *very* visible here.

### Acceptance — unchanged from v1
(Quickstart → tables visibly filling; ≤150 KB binary delta, ≤5 MB RSS delta;
headless-browser CI check asserting zero non-localhost fetches; degrades to a link
list without JS.)

## Part B — Webhooks

### Config — unchanged from v1, one addition

```toml
[[webhooks]]
name = "big-delegations"
table = "staking__stake_delegated"
where = "tokens > 100000000000000000000000"
url = "https://hooks.example.com/x"
batch_max = 50
finality = "sealed"       # "sealed" (default) | "tip"
since = "registration"    # NEW: "registration" (default) | block number | "genesis"
```

### Design decisions — v1 decisions stand, one added

- Finality is the honest knob (sealed never lies; tip carries `"finality":"tip"` and
  sends `{"retracted": true}` compensations) — unchanged, still the differentiator
  no competitor ships.
- At-least-once, ordered, (block, log_index) in every payload, bounded retry,
  dead-letter file, cursor in the hot store, engine outside the deterministic
  core — unchanged, now shared per §Reconciliation.
- Predicate evaluation micro-batched through the SQL engine — unchanged.
- Egress rules (explicit URLs, HMAC, no redirects, startup log) — unchanged.
- **NEW — backfill suppression (`since`), a correctness catch:** `--seal-direct`
  cold starts seal the *entire history* rapidly; a naive sealed-finality webhook
  would fire for millions of historical rows on first run. Default
  `since = "registration"`: a webhook's cursor initializes at the tip watermark
  when first registered, so only rows sealed *after* registration deliver.
  Intentional replays are explicit (`since = <block>` or `"genesis"`), and the
  pre-run row estimate + `--yes` gate (the RFC-0004 honesty pattern) applies to a
  historical replay exactly as it does to any large backfill.

### Non-goals — unchanged from v1
(No payload templating; no exactly-once; no queue sinks — Kafka egress remains a
scaled-mode/operator concern.)

### Acceptance — v1 items plus
- A `--seal-direct` cold start with a registered `since = "registration"` webhook
  delivers zero historical rows and the first post-registration sealed row.
- The compliance producer path: a fixture `sanction_hit` annotation delivers through
  the same engine with the pack-manifest host allowlist enforced (the C5 acceptance
  test, relocated here).

## Implementation plan — revised order

B0 (may precede A): the shared delivery engine (outbox, cursor, retries,
dead-letter, retractions) with the table-predicate producer on the sealed path —
this is the piece RFC-0008 C5 waits on.
A1: `/views` + `/nest` endpoints → app shell + table browser → SQL runner →
status page (poll-based) → embed + budgets + headless CI test.
B1: tip path + retraction deliveries; `since` semantics; docs with a Slack example.
Order rationale: A before B1 for demo value (unchanged), but B0 floats to whenever
the compliance pack needs it — the engine has two customers and either may pull it.

## Risks — v1 risks stand, one resolved, one added

- UI scope creep → read-only rule + 150 KB budget (unchanged).
- Blocking receivers → bounded channel + dead-letter (unchanged).
- Security posture drift → token rule + NEW `admin = false` for operators.
- RESOLVED: the C5 duplication risk — by §Reconciliation.
- NEW: **engine ordering under two producers** — annotation deltas and table rows
  share the outbox; ordering is per-subscription, never global, and the docs must
  say so (a compliance alert and a user webhook about the same block may deliver in
  either order relative to each other; each stream is internally ordered).

## Open questions

1. v1 Q1 (EXPLAIN ANALYZE in the SQL runner) — RESOLVED: yes; it runs under the
   RFC-0005 v2 query guards (statement timeout + row caps), which now exist as
   spec'd features rather than a hope.
2. v1 Q2 (webhooks on views) — stance unchanged (reject with a clear error), but
   the error message can now be precise: nest views exist (RFC-0002 step 4a) yet
   are DuckDB-over-sealed definitions without deltas; view webhooks arrive with IVM
   generalization. Say that in the rejection.
3. NEW: should the delivery engine expose its gauges (outbox depth, dead-letter
   count, per-subscription lag) on /metrics? Yes — add them to the RFC-0005 v2 §6
   metrics list; operators will alert on outbox depth before anything else.
