# RFC-0008: Compliance pack

- Status: Draft
- Author: Pete (cargopete)
- Date: 2026-07-14
- Depends on: P0 (i128/decimal balances, specified below); slice 4 (WASM transform
  runtime, shipped); the effectful-worlds host machinery (RFC-0006 M1 / ported in slice C4)
- Blocks: —

> Note: this RFC is written as an implementation brief (build-order slices with gates),
> aimed at stablecoin/fintech operators. It stays within every CLAUDE.md non-negotiable
> and adds a compliance-specific architecture on top of the existing slices.

## Architecture rules (additions to the existing non-negotiables)

1. **Pure/effectful split is the compliance architecture.** Anything that must be reproducible
   for an auditor is a *pure* stage (zero-capability wasm component or a DBSP view). Anything
   that touches the outside world (webhooks, kv) is an *effectful* stage whose outputs are
   **annotation facts only** — append-only, never mutating core entities. The deterministic
   core must remain replayable with effectful stages disabled.
2. **Screening is pure, list-as-data.** No screening API calls in the data path, ever.
   Sanctions/watch lists are content-addressed snapshots (sha256, same convention as Parquet
   segments) fed to a pure component as an input batch. List refresh is a host-side, out-of-band
   command. This makes every screening decision reproducible: (list snapshot hash, block range,
   component hash) → identical output.
3. **Batched Arrow boundary everywhere.** All new wasm stages use the existing whole-batch
   Arrow IPC WIT boundary from slice 4. Do NOT reintroduce liminal's per-event call boundary.
4. **Capability grants are the audit artifact.** An effectful component's imports must be
   checkable with `wasm-tools component wit` alone. The host grants capabilities per-component
   at composition time (port this pattern from liminal-host); grants are declared in the pack
   manifest, and the runtime refuses a component whose actual imports exceed its declared grant.
5. **Retraction-correct flags.** Every flag/alert raised on a block that reorgs out must be
   retracted through the same DBSP mechanism as balances (weight −1). No lingering false
   positives after a reorg. Alert sinks must emit retraction events too (`flag_retracted`).
6. **No scope creep:** no multi-hop taint tracing (direct exposure only), no DAG pipeline
   runner (linear decode → screen → views → sinks chain), no LLM anywhere in the data path.

## Prerequisite (do this first)

**P0 — i128/decimal balances.** The i64 base-unit limitation is disqualifying for treasury and
compliance use. Move balance accumulation and all threshold arithmetic to i128 (or a fixed-point
decimal wrapper) end-to-end: DBSP circuit, redb hot store encoding, Parquet schema, `/sql`
surface, MCP `balance`/`top_balances` tools. Add a golden test with amounts that overflow i64.
This is a known gap already noted in the README — close it before building anything on top.

## Build order (vertical slices, each ends runnable — keep the progress-log discipline)

### Slice C1 — Labels & annotation-facts substrate
The foundation everything else writes into.

- New fact kind: `annotation` — append-only table keyed by (subject address | tx hash | log id),
  with `kind` (e.g. `label`, `sanction_hit`, `threshold_flag`), `source` (component hash or
  manifest id), `list_snapshot` (content hash, nullable), block context, and payload (JSON or
  typed columns — prefer typed). Stored in the hot store, sealed to its own Parquet segments
  past finality, queryable via `/sql`.
- `labels` as a first-class annotation kind: user-supplied labeled address sets
  (`nuthatch labels import <csv|json>` → content-addressed snapshot + annotations).
- **Direct exposure view (DBSP):** a declarative join of transfers × labels → per-address
  "counterparty exposure to labeled set X" (counts + summed amounts, both directions).
  Served at `/exposure/{address}`. Reorg = retraction, same circuit as balances.
- Gate: golden test for the join incl. retraction convergence; RAM budget still green.

### Slice C2 — Pure sanctions screening component
- `nuthatch lists fetch ofac-sdn` / `eu-consolidated`: host-side (NOT data path) download +
  parse into a normalized address list, written as a content-addressed snapshot under
  `lists/` and recorded in `manifest.json`. Parsing lives in the host CLI; keep formats
  minimal (crypto-address entries only — do not attempt name/entity fuzzy matching, that is
  out of scope and a false-precision trap).
- `screen` wasm component: **pure, zero capabilities.** Inputs: transfer batch + list snapshot
  batch. Output: `sanction_hit` annotation facts (address, side, list snapshot hash).
  Purity checkable from imports alone.
- Wire as an optional live indexing stage (config: `[screening] lists = [...]`) and as a batch
  backfill command: `nuthatch screen --list <hash> --from <block> --to <block>` (replayable
  over sealed Parquet — this is the audit story: same inputs, same hashes, same output).
- Gate: deterministic golden test (fixture list + fixture transfers → exact annotations);
  a replay test proving live-stage output == backfill output over the same range.

### Slice C3 — Threshold & velocity views (pure DBSP)
- Declarative flag views, configured in `nuthatch.toml`:
  - `threshold`: single transfer amount ≥ N (travel-rule style, e.g. €1000/$3000 equivalents —
    thresholds are config, not code; no currency conversion in-core, thresholds are in token
    base units at this stage).
  - `velocity`: rolling per-address volume/count over a block-window (approximate 24h by
    block count; document the approximation honestly).
- Both emit `threshold_flag` annotation facts and are served at `/flags?kind=...`.
- Reorg → retraction (golden test required, as with balances).
- Gate: RAM check with views active on a live USDC run.

### Slice C4 — Effectful worlds (the liminal port)
- Port from `liminal-host`: per-component capability injection at composition time
  (wasmtime linker wiring for `wasi:http` outbound and a small kv world). Adapt to the
  batched Arrow boundary — the ported code is the *host-side granting machinery*, not
  liminal's pipeline runner or per-event WIT.
- New WIT world(s): `nuthatch:transform/effectful-stage` — same batch-in shape, but
  annotations-only output enforced by type (it cannot emit or mutate entities).
- Host verifies declared-vs-actual imports (`wasm-tools`-equivalent check via wasmtime
  introspection at load) and refuses over-privileged components. Grants (incl. allowed HTTP
  hosts) come from the pack manifest, not the component.
- Gate: a test that a component importing undeclared capabilities is rejected; an end-to-end
  test with a toy effectful component (kv-granted) producing annotations.

### Slice C5 — Alert sinks (webhook)
- `alert-webhook` effectful component: consumes flag/hit annotation deltas (including
  retractions), POSTs JSON to configured endpoints. `wasi:http` granted to exactly the
  configured hosts; nothing else. At-least-once delivery with a small durable outbox in the
  hot store (redb) + retry/backoff; document the delivery semantics.
- Multi-sink off the single existing cursor — no second cursor, no reconciliation layer.
  Sinks lag-tolerant: if a sink stalls, indexing does not (bounded outbox with a visible
  `/status` gauge; on overflow, drop-oldest with a loud log — never block the indexer).
- Config: `[[alerts]] kinds = ["sanction_hit","threshold_flag"] url = "..."`.
- Gate: end-to-end test with a local HTTP test server: flag raised → webhook fires;
  reorg → `flag_retracted` webhook fires.

### Slice C6 — Compliance pack manifest + audit surface
- `compliance-pack.toml`: declares components (by content hash), their capability grants,
  list snapshots, view configs, alert routing. Signed (ed25519, key in a local file — no key
  service). `nuthatch pack verify` checks signatures, component hashes, and grant conformance.
- Audit commands:
  - `nuthatch audit replay --pack <manifest> --from --to` → re-runs pure stages over sealed
    segments, diffs against stored annotations, prints a verdict (the "prove it" command).
  - `nuthatch audit report --from --to` → summary of hits/flags with list snapshot hashes
    and block bounds (markdown/JSON out).
- MCP: add `flags`, `exposure`, `screen_status` tools; extend `schema`'s semantic hints so
  agents can answer "was address X flagged and against which list version".
- `init` scaffolding: extend `llms.txt` + the `.claude/skills/nuthatch/` skill with the
  compliance query surface.
- Gate: full-pipeline integration test (screen + flags + exposure + webhook) on the USDC
  fixture; RAM budget green with everything active; README table + progress log updated.

## Testing & CI additions
- Every flag/annotation view gets the same golden-test + retraction-convergence treatment
  as balances (extend the existing proptest to cover annotation rollback).
- Add a CI gate: `audit replay` determinism check on a fixed fixture (bit-identical output).
- RAM budget gate now runs with the full compliance pack active.

## Out of scope (do not build, even if tempting)
- Multi-hop taint/graph tracing; entity/name fuzzy matching against sanctions lists;
  fiat-currency conversion in the core; a rules DSL; Kafka sink (webhook only for now);
  any hosted/telemetry component. If a slice seems to need one of these, stop and flag it.

## Definition of done
`nuthatch init <token> && nuthatch lists fetch ofac-sdn && nuthatch dev` on live mainnet USDC:
screening + threshold flags + exposure views active, webhook alert fires on a fixture-injected
hit in test, `nuthatch audit replay` reproduces stored annotations bit-identically, peak RAM
within budget, all tests green, README status table and progress log updated in the house style.
