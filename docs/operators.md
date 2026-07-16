# Running nuthatch as an operator

nuthatch is built to be **fronted**, not exposed raw. The dividing line, stated once and kept:
**gateways, authentication, metering, and multi-tenancy are the operator's layer**; nuthatch ships
the *guards* and *signals* that make fronting it safe and billable. The binary has no accounts, no
tenancy, and phones home to nobody — that stays true.

## Bind posture

The API defaults to `127.0.0.1:8288`. Bind it elsewhere with `--listen`. When bound off-localhost,
startup logs a loud warning: the `/sql` surface is guarded but **not authenticated** — the operator's
gateway decides *who* may query; the guards below bound only *how much*. Never expose `/sql` straight
to the internet.

## Query guards (`/sql`)

Node self-protection against any single query or a burst — not per-caller quotas (that needs identity
a single-tenant node doesn't have; it's the gateway's job). Current defaults:

| Guard | Default | What it bounds |
|---|---|---|
| statement timeout | 30 s | a runaway (e.g. cartesian) query is interrupted mid-flight |
| max result rows | 50,000 | the Rust-side result buffer (outside DuckDB's own memory limit) |
| max concurrent queries | 2 | the real DoS multiplier — a semaphore; excess returns `503` |
| max query length | 16 KiB | rejects absurd query strings before the planner |

Rejections are surfaced as HTTP errors (`400`/`503`) and counted in `/metrics`
(`nuthatch_sql_rejections_total`).

## Metrics (`/metrics`)

Prometheus text exposition — the endpoint to scrape, alert, and bill against. Key series:

- `nuthatch_tip_height`, `nuthatch_last_block`, `nuthatch_tip_lag_blocks` — is it keeping up?
- `nuthatch_sealed_through` — cold-layer watermark.
- `nuthatch_rows_decoded_total`, `nuthatch_rows_sealed_total`, `nuthatch_reorgs_total` — ingestion.
- `nuthatch_http_requests_total`, `nuthatch_sql_queries_total`, `nuthatch_sql_rejections_total`,
  `nuthatch_rpc_requests_total` — serving + upstream.
- `nuthatch_rss_bytes` — process memory (the footprint you provision against).

`/metrics` and `/health` are unauthenticated by design; scope them to your internal network at the
gateway if you don't want them public.

## Lifecycle

- **SIGTERM / SIGINT** (systemd, `docker stop`, Ctrl-C): the API drains in-flight requests and exits
  **0**; the ingest task's progress is checkpointed, so a restart resumes without gaps or duplicates
  (rows are keyed by `(block, log_index)` — idempotent).
- Data lives under the nest directory (`nuthatch.redb`, `segments/`). Back up the directory; sealed
  segments are content-addressed and safe to copy while running.

## Stability contract (0.x)

- **Config**: `nuthatch.toml` keys and the nest `schema_version` follow a deprecation policy — a key
  removed in 0.(n+1) warns in 0.n first.
- **Data layout**: redb tables, segment layout, `manifest.json`, and `schema.json` are versioned.
  The target within 0.x is **in-place-safe upgrades**; each release's notes state "in-place safe" or
  "reseal required" explicitly.
