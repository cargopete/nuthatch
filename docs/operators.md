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

## Roosts (many nests, one runtime)

One process can host **many nests on the same chain** — a *roost* — sharing a single cursor, one chain
read, and one API, instead of a process per nest. Different chains still mean a process each (a second
chain is a second cursor). See [`examples/roost/`](../examples/roost) for a runnable two-nest example.

```
roost/
  roost.toml
  nests/
    lodestar/      # a nest dir, exactly as `nuthatch init` produces
    uniswap-v3/
```

```toml
[roost]
name = "arb-roost"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = ["https://arb1.example"]   # the roost owns the chain connection
nests = ["lodestar", "uniswap-v3"]    # dir names under nests/
# max_rss_mb = 2048                    # per-runtime RSS ceiling; a mount projected over it is refused
```

Run it with `nuthatch roost dev --dir roost/ --listen …`. Every nest's `[nest].chain`/`chain_id` must
match the roost's — a mismatch is refused at startup (a different chain needs its own roost).

- **Serving.** `GET /nests` is the roster (each nest's name, chain, registry hash, table count,
  `estimated_rss_mb`; plus the roost's `projected_rss_mb`, `max_rss_mb`, and real `rss_bytes`). Every
  nest's full API lives under its prefix — `/<name>/tables`, `/<name>/sql`, `/<name>/_admin/`, … `/sql`
  stays **per-nest scoped**: a query sees one nest's data.
- **Isolation.** Stores are per-nest (each keeps its own `nuthatch.redb` + `segments/` under
  `nests/<name>/`); only the cursor is shared. A bad view or runaway factory in one nest can't touch
  another's data. A reorg is detected once and rolled back across every nest.
- **Footprint.** The budget is per-*runtime*, not per-nest. `roost dev` projects RSS before starting
  and refuses a mount over `max_rss_mb` (default 2 GB). The projection is a rough estimate; `GET /nests`
  reports the real `rss_bytes` beside it — provision against the measurement. (A two-nest ERC-20 roost
  measures ~110 MB resident against a ~300 MB projection.)
- **Mixed nests.** Static and factory nests co-exist in one roost; nests may mount at different heights
  and each backfills its own history — the cursor only couples them at the tip.

## Deploy recipes (copy-paste)

`nuthatch dev` **is** the serve command — it backfills, follows the tip, and serves the API in one
process. "I tried it locally" → "it's running on my box" is just running that under a supervisor.

### systemd

```ini
# /etc/systemd/system/nuthatch.service
[Unit]
Description=nuthatch indexer
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=nuthatch
WorkingDirectory=/var/lib/nuthatch/mynest        # the nest directory (holds nuthatch.toml)
ExecStart=/usr/local/bin/nuthatch dev --listen 127.0.0.1:8288 --seal-direct --concurrency 8
Restart=on-failure
RestartSec=5
# Off-localhost the admin UI requires this; unset it and bind 127.0.0.1 to disable remote admin.
Environment=NUTHATCH_ADMIN_TOKEN=change-me
# Keep it inside the footprint budget; the box needs headroom for DuckDB queries.
MemoryMax=2G

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload && sudo systemctl enable --now nuthatch
journalctl -u nuthatch -f          # a clean progress line during backfill, then quiet tip-following
```

### Docker

No image is published yet; build the binary into a slim runtime and mount the nest directory:

```dockerfile
# Dockerfile
FROM rust:1.85 AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/nuthatch /usr/local/bin/nuthatch
ENTRYPOINT ["nuthatch"]
```

```sh
docker build -t nuthatch .
# The nest directory is the only state; mount it and expose the API on localhost.
docker run -d --name nuthatch --restart unless-stopped \
  -v "$PWD/mynest:/nest" -p 127.0.0.1:8288:8288 \
  -e NUTHATCH_ADMIN_TOKEN=change-me \
  nuthatch dev --dir /nest --listen 0.0.0.0:8288 --seal-direct --concurrency 8
```

Bind `0.0.0.0` **inside** the container but publish only to `127.0.0.1` on the host, and put a reverse
proxy (TLS + auth) in front — same posture as the bare-metal case. `docker stop` sends SIGTERM, which
drains and checkpoints cleanly (see Lifecycle).

## AI: point a coding agent at your data

The MCP server is compiled in. Wire a client to a running nest in one step — `nuthatch mcp
--print-config` prints exactly what to paste:

```sh
nuthatch dev &                       # the index the agent will query
nuthatch mcp --print-config          # copy-paste config for Claude Code / any MCP client
# Claude Code, directly:
claude mcp add nuthatch -- nuthatch mcp --url http://127.0.0.1:8288
```

`nuthatch mcp` is a thin, fully-offline stdio bridge to the local `dev` HTTP API — it never contends
with the single writer and nothing phones home. The client launches it; you ask your contract's data
in plain English and it writes the SQL. (The semantic layer that makes that first-try-correct is
RFC-0016.)

## Stability contract (0.x)

- **Config**: `nuthatch.toml` keys and the nest `schema_version` follow a deprecation policy — a key
  removed in 0.(n+1) warns in 0.n first.
- **Data layout**: redb tables, segment layout, `manifest.json`, and `schema.json` are versioned.
  The target within 0.x is **in-place-safe upgrades**; each release's notes state "in-place safe" or
  "reseal required" explicitly.
