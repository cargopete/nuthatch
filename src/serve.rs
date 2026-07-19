//! The API surface. Point-reads hit redb directly (the hot path). Everything is local; nothing
//! phones home. This is where the MCP server and SQL surface will grow in later slices.

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::analytics;
use crate::exposure::ExposureView;
use crate::registry::TableSchema;
use crate::store::Store;
use crate::velocity::VelocityView;
use crate::views::BalanceView;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// How many analytical (DuckDB) queries may run at once across `/sql` and cold `/table` reads. Each
/// DuckDB query is already capped at 512 MB / 2 threads (see `analytics`), so this bounds the whole
/// analytical surface's worst-case footprint — the real DoS multiplier is *concurrency*, not any one
/// query. Kept small to stay well inside the embedded RAM budget; this is node self-protection, not
/// per-caller rate-limiting (that needs identity and belongs in a gateway).
pub const SQL_MAX_CONCURRENCY: usize = 2;
/// Wall-clock deadline for a single analytical query; a runaway (e.g. cartesian) is interrupted.
const SQL_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on rows materialised from one analytical query — bounds the Rust-side result buffer, which
/// lives outside DuckDB's own memory limit. Beyond this the result is truncated and flagged.
const SQL_MAX_ROWS: usize = 50_000;
/// Reject absurdly long query strings before they reach the planner.
const SQL_MAX_QUERY_LEN: usize = 16 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub address: String,
    pub chain: String,
    pub dir: PathBuf,
    pub balances: BalanceView,
    /// Direct counterparty-exposure to the labeled set (RFC-0008 C1) — served at `/exposure/{addr}`.
    pub exposure: ExposureView,
    /// Windowed per-address velocity view (RFC-0008 C3) — served at `/flags?kind=velocity`.
    pub velocity: VelocityView,
    /// Single-transfer threshold in base units, if configured (RFC-0008 C3) — for `/`'s flag summary.
    pub threshold: Option<i128>,
    /// Velocity flag threshold in base units, if configured — the cutoff `/flags?kind=velocity` uses.
    pub velocity_threshold: Option<i128>,
    /// Whether the built-in admin UI (`/_admin/`) is served (RFC-0010 Part A).
    pub admin_enabled: bool,
    /// When the bind is off-localhost, the token a request must present (`?token=…`) to reach the admin
    /// UI (SEC-5). `None` on a localhost bind (open) — the env var merely *enabling* the route off-
    /// localhost, without checking it per request, was security theater.
    pub admin_token: Option<String>,
    /// Static nest metadata for the admin UI's Nest tab (`/nest`): contracts, templates, factories,
    /// webhooks, registry hash. Computed once at startup.
    pub nest_info: Arc<serde_json::Value>,
    /// The nest's table schemas (from the decode registry) — the source of truth for `/tables`.
    pub tables: Arc<Vec<TableSchema>>,
    /// Admission control for the analytical (DuckDB) surface: bounds how many `/sql` and cold
    /// `/table` queries run at once so a burst can't multiply DuckDB's per-query footprint past the
    /// process budget. Constructed with [`SQL_MAX_CONCURRENCY`] permits.
    pub sql_gate: Arc<Semaphore>,
}

/// Build a nest's router — every per-nest route plus the request-count layer, bound to `state`. Split
/// out of [`run`] so a roost (RFC-0012) can mount many of these under `/<nest>/…` prefixes; a solo
/// `dev` serves exactly one at the root. Identical routes either way — a nest can't tell it's co-hosted.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(summary))
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(metrics_handler))
        .route("/tables", get(tables))
        .route("/schema", get(schema_doc))
        .route("/table/{name}", get(table))
        .route("/entities", get(entities))
        .route("/entity/{id}", get(entity))
        .route("/sql", get(sql))
        .route("/explain", get(explain))
        .route("/balances", get(balances))
        .route("/balance/{address}", get(balance))
        .route("/exposure/{address}", get(exposure))
        .route("/flags", get(flags))
        .route("/nest", get(nest))
        .route("/_admin", get(admin_index))
        .route("/_admin/", get(admin_index))
        // Count every served request for `/metrics` (the operator's billing signal).
        .layer(axum::middleware::from_fn(count_request))
        .with_state(state)
}

pub async fn run(listen: &str, state: AppState) -> Result<()> {
    bind_and_serve(listen, router(state)).await
}

/// Serve many nests behind one listener (RFC-0012 roost, slice 1): a `/nests` roster plus every nest's
/// full API under its `/<name>/…` prefix. Chain identity and the cursor are still per-nest at this
/// slice (the shared cursor is slice 2); this lands the routing + per-nest isolation of the serving
/// surface first. Each nest's routes are byte-identical to a solo `dev`, just prefixed.
pub async fn run_roost(
    listen: &str,
    roster: serde_json::Value,
    nests: Vec<(String, AppState)>,
) -> Result<()> {
    let roster = Arc::new(roster);
    let mut app = Router::new()
        .route("/health", get(|| async { "ok" }))
        // `GET /nests` — the roster (name, chain, registry hash, table count) across mounted nests.
        .route(
            "/nests",
            get(move || {
                let r = roster.clone();
                async move { Json((*r).clone()) }
            }),
        );
    for (name, state) in nests {
        // `Router::nest` re-roots the whole per-nest router under `/<name>`, so `/lodestar/tables`,
        // `/lodestar/sql`, `/lodestar/_admin/` … all resolve to that nest's isolated state.
        app = app.nest(&format!("/{name}"), router(state));
    }
    bind_and_serve(listen, app).await
}

/// Bind `listen` and serve `app` until a shutdown signal — the shared tail of [`run`]/[`run_roost`].
async fn bind_and_serve(listen: &str, app: Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;
    tracing::info!("API live on http://{listen}  (try GET /  and  /metrics)");
    // A loud one-liner when bound off-localhost: the guards bound *how much*, but *who* is the
    // operator's gateway's job — never expose this straight to the internet without one.
    if !is_localhost(listen) {
        tracing::warn!(
            "listening on {listen} (not localhost): the /sql surface is guarded (timeout + row cap \
             + {SQL_MAX_CONCURRENCY} concurrent) but has NO authentication — put a gateway in front \
             before exposing it publicly. See docs/operators.md."
        );
    }
    // Graceful shutdown on SIGTERM/SIGINT: axum drains in-flight requests, then this returns so the
    // caller can abort the ingest task(s) (progress is checkpointed, so a restart resumes cleanly).
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    tracing::info!("shutdown signal received; API stopped");
    Ok(())
}

/// The built-in admin UI (RFC-0010 Part A) — a single self-contained page, embedded in the binary.
const ADMIN_HTML: &str = include_str!("admin.html");

/// `GET /_admin/` — serve the admin UI when enabled, else 404 (it's off, or the bind is public with
/// no token). The page is read-only and talks only to this same-origin API; no external requests.
#[derive(Deserialize)]
struct AdminQuery {
    token: Option<String>,
}

async fn admin_index(State(s): State<AppState>, Query(q): Query<AdminQuery>) -> impl IntoResponse {
    if !s.admin_enabled {
        return (StatusCode::NOT_FOUND, "admin UI disabled").into_response();
    }
    // SEC-5: off-localhost the route requires the token per request. On localhost `admin_token` is
    // `None` and the page is open. The compare is constant-time (SEC review) so a timing side-channel
    // can't recover the token byte-by-byte.
    if let Some(required) = &s.admin_token {
        if !q.token.as_deref().is_some_and(|t| ct_eq(t, required)) {
            return (
                StatusCode::UNAUTHORIZED,
                "admin UI requires a valid ?token=",
            )
                .into_response();
        }
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ADMIN_HTML,
    )
        .into_response()
}

/// `GET /nest` — static nest metadata for the admin UI's Nest tab (RFC-0010 Part A).
async fn nest(State(s): State<AppState>) -> impl IntoResponse {
    Json((*s.nest_info).clone())
}

/// Whether `listen` binds only the loopback interface.
pub fn is_localhost(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "[::1]")
}

/// Resolves when the process is asked to stop — SIGTERM (systemd/Docker) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// Middleware: bump the request counter, then pass through.
async fn count_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    crate::metrics::METRICS.inc_http();
    next.run(req).await
}

/// `GET /metrics` — Prometheus text exposition (RFC-0005 §6).
async fn metrics_handler() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        crate::metrics::METRICS.render(),
    )
}

async fn summary(State(s): State<AppState>) -> impl IntoResponse {
    let count = s.store.count().unwrap_or(0);
    let last_block = s.store.get_meta("last_block").ok().flatten();
    Json(json!({
        "name": "nuthatch",
        "chain": s.chain,
        "address": s.address,
        "entities": count,
        "last_block": last_block,
        "sealed_through": s.store.get_meta("sealed_through").ok().flatten(),
        "holders": s.balances.holders(),
        "exposure_entries": s.exposure.entries(),
        "velocity_buckets": s.velocity.entries(),
        "alert_outbox": s.store.outbox_len(),
        "tables": s.tables.len(),
        "views": ["balances (IVM)", "exposure (IVM)", "velocity (IVM)"],
        "endpoints": [
            "/health",
            "/tables",
            "/table/{name}?limit=100",
            "/entities?limit=100",
            "/entity/{block:012}-{log_index:06}",
            "/sql?q=SELECT count(*) FROM \"<alias>__<event>\"",
            "/balances?limit=100",
            "/balance/{address}",
            "/exposure/{address}",
            "/flags?kind=threshold|velocity",
        ],
    }))
}

/// List every table and its columns (the decoded data model).
async fn tables(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({ "count": s.tables.len(), "tables": &*s.tables }))
}

#[derive(Deserialize)]
struct TableQuery {
    limit: Option<usize>,
    from_block: Option<u64>,
    to_block: Option<u64>,
}

/// The enriched schema document (RFC-0016 §2): the composition of registry **structure**, the
/// authored **meaning** from `semantic.toml`, the derived **footguns**, and live **coverage** (the
/// hot/cold seam as numbers). Assembled per call from this running nest — the MCP `schema` tool
/// relays it, so an agent reads *this* nest's data model, not a static string. Plain text.
async fn schema_doc(State(s): State<AppState>) -> impl IntoResponse {
    let sem = crate::semantic::load(&s.dir).ok().flatten();
    if let Some(sem) = &sem {
        for w in crate::semantic::drift(&s.tables, sem) {
            tracing::warn!("semantic.toml drift: {w}");
        }
    }
    let coverage = crate::semantic::Coverage {
        sealed_through: s.store.sealed_through(),
        tip: s
            .store
            .get_meta("last_block")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    let doc = crate::semantic::compose(&s.tables, sem.as_ref(), Some(&coverage));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        doc,
    )
}

/// Recent rows of one table, merged across the hot store and the sealed cold segments.
async fn table(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TableQuery>,
) -> impl IntoResponse {
    if !s.tables.iter().any(|t| t.table == name) {
        return not_found(&name);
    }
    let limit = q.limit.unwrap_or(100).min(1000);
    let in_range = |v: &Value| {
        let b = v.get("block_number").and_then(Value::as_u64).unwrap_or(0);
        q.from_block.map(|f| b >= f).unwrap_or(true) && q.to_block.map(|t| b <= t).unwrap_or(true)
    };

    // Hot rows (tip), newest first.
    let mut items: Vec<Value> = s
        .store
        .recent_by_table(&name, limit)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(r).ok())
        .filter(&in_range)
        .collect();

    // Fill from cold (sealed segments) if the hot store didn't satisfy the limit. This runs under
    // the same analytical admission gate as `/sql`; if it's saturated we serve the hot rows we
    // already have rather than pile more scan-heavy work on. Cold is an enrichment of the hot result,
    // so best-effort (`try_acquire`) is the right degradation — a point-read-ish endpoint shouldn't
    // 503 just because the analytical surface is busy.
    if items.len() < limit {
        if let Ok(permit) = Arc::clone(&s.sql_gate).try_acquire_owned() {
            let need = limit - items.len();
            let mut where_ = String::new();
            if let Some(f) = q.from_block {
                where_.push_str(&format!(" AND block_number >= {f}"));
            }
            if let Some(t) = q.to_block {
                where_.push_str(&format!(" AND block_number <= {t}"));
            }
            let sql = format!(
                "SELECT * FROM \"{name}\" WHERE 1=1{where_} ORDER BY block_number DESC, log_index DESC LIMIT {need}"
            );
            let dir = s.dir.clone();
            let guard = analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows: need,
            };
            if let Ok(Ok(out)) = tokio::task::spawn_blocking(move || {
                let _permit = permit; // held for the whole blocking query
                analytics::query_guarded(&dir, &sql, guard)
            })
            .await
            {
                items.extend(out.rows);
            }
        }
    }

    // Dedup by (block, log_index); hot wins over cold.
    let mut seen = std::collections::HashSet::new();
    items.retain(|v| {
        let id = (
            v.get("block_number").and_then(Value::as_u64),
            v.get("log_index").and_then(Value::as_u64),
        );
        seen.insert(id)
    });
    items.truncate(limit);
    Json(json!({ "table": name, "count": items.len(), "items": items })).into_response()
}

#[derive(Deserialize)]
struct EntitiesQuery {
    limit: Option<usize>,
}

async fn entities(State(s): State<AppState>, Query(q): Query<EntitiesQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    match s.store.recent(limit) {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .iter()
                .filter_map(|r| serde_json::from_str::<Value>(r).ok())
                .collect();
            Json(json!({ "count": items.len(), "items": items })).into_response()
        }
        Err(e) => error(format!("{e:#}")),
    }
}

async fn entity(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    // Hot path first (redb). On a miss, fall back to the sealed segments (DuckDB), so a point-read
    // keeps working across the hot→cold seam even after the hot store has been pruned.
    match s.store.get_entity(&id) {
        Ok(Some(raw)) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => return Json(v).into_response(),
            Err(e) => return error(format!("{e:#}")),
        },
        Ok(None) => {}
        Err(e) => return error(format!("{e:#}")),
    }

    match parse_id(&id) {
        Some((block, log_index)) => {
            let dir = s.dir.clone();
            let sealed =
                tokio::task::spawn_blocking(move || analytics::get_row(&dir, block, log_index))
                    .await;
            match sealed {
                Ok(Ok(Some(v))) => Json(v).into_response(),
                Ok(Ok(None)) => not_found(&id),
                Ok(Err(e)) => error(format!("{e:#}")),
                Err(e) => error(format!("{e}")),
            }
        }
        None => not_found(&id),
    }
}

#[derive(Deserialize)]
struct SqlQuery {
    q: String,
    /// Optional row cap for this request, clamped to `[1, SQL_MAX_ROWS]`. The MCP bridge passes a
    /// small value (default 200) so an agent's context isn't flooded (RFC-0016 §4); curl gets the
    /// full cap. Absent → the node cap.
    #[serde(default)]
    max_rows: Option<usize>,
}

/// Read-only analytical SQL over the sealed segments; one view per `{alias}__{event}` table.
///
/// Self-protecting, so a public `/sql` isn't a DoS vector: bounded concurrency (503 when the
/// analytical surface is saturated — a growing backlog is itself the attack), a per-query wall-clock
/// budget (a runaway is interrupted), a max query length, and a row cap (bounds the result buffer).
/// What's deliberately *absent* is authn / per-caller quotas: those need caller identity a sovereign
/// node doesn't have, so gating *who* may query and *how much* is a gateway's job, not the node's.
async fn sql(State(s): State<AppState>, Query(q): Query<SqlQuery>) -> impl IntoResponse {
    use crate::metrics::METRICS;
    if q.q.len() > SQL_MAX_QUERY_LEN {
        METRICS.inc_sql_rejected();
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("query too long: {} bytes (max {SQL_MAX_QUERY_LEN})", q.q.len()) })),
        )
            .into_response();
    }
    // Fail fast when the analytical surface is saturated rather than queue: a backlog of pending
    // DuckDB queries would itself exhaust memory/threads.
    let permit = match Arc::clone(&s.sql_gate).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            METRICS.inc_sql_rejected();
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "server busy: too many concurrent SQL queries" })),
            )
                .into_response();
        }
    };
    METRICS.inc_sql();
    let dir = s.dir.clone();
    let sql = q.q.clone();
    let store = s.store.clone();
    // Per-request row cap (RFC-0016 §4): the MCP bridge asks for a small number so an agent's context
    // isn't flooded; curl omits it and gets the node cap. Clamped so it can only ever tighten.
    let max_rows = q.max_rows.unwrap_or(SQL_MAX_ROWS).clamp(1, SQL_MAX_ROWS);
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit; // held for the whole blocking query, released on return
                              // Scan the hot tip (redb, blocking) inside the same blocking task, so `/sql` sees the unsealed
                              // rows alongside the sealed segments (RFC-0013). A scan failure degrades to cold-only.
        let hot = store.hot_rows_by_table().unwrap_or_default();
        let sealed_through = store.sealed_through();
        analytics::query_hot_cold(
            &dir,
            &sql,
            analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows,
            },
            &hot,
            sealed_through,
        )
    })
    .await;
    match result {
        // Provenance stamp (RFC-0016 §4): an agent can cite its answer against content-addressed data —
        // as of which block, what's sealed, and the registry it decoded with.
        Ok(Ok(out)) => Json(json!({
            "count": out.rows.len(),
            "truncated": out.truncated,
            "rows": out.rows,
            "provenance": {
                "as_of": s.store.get_meta("last_block").ok().flatten()
                    .and_then(|v| v.parse::<u64>().ok()),
                "sealed_through": s.store.sealed_through(),
                "source": "hot+sealed",
                "registry_hash": s.nest_info.get("registry_hash").and_then(Value::as_str),
            },
        }))
        .into_response(),
        Ok(Err(e)) => {
            // A guard rejection (timeout / interrupt) or a bad query — counted as a rejection.
            METRICS.inc_sql_rejected();
            // Errors as prompts (RFC-0016 §3): classify the failure against the schema and append an
            // actionable hint so an agent (or the REPL user) self-corrects in one round-trip. The raw
            // engine message is preserved but path-scrubbed (SEC review) — DuckDB embeds absolute
            // segment paths, which would leak the on-disk layout; the useful table/column detail stays.
            let raw = sanitize_sql_error(&format!("{e:#}"), &s.dir);
            let msg = match crate::sql_errors::enrich(&raw, &q.q, &s.tables) {
                Some(hint) => format!("{raw}\n\nhint: {hint}"),
                None => raw,
            };
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
        }
        Err(e) => error(format!("{e}")),
    }
}

/// `GET /explain?q=…` — validate a query without executing it (RFC-0016 §3): bind it (catching
/// unknown tables/columns/type errors, with the same enriched hints as `/sql`) but scan nothing, by
/// wrapping it as `SELECT * FROM (<q>) LIMIT 0`. An agent checks a query's shape before spending a
/// concurrency slot on the real thing. Returns `{valid:true}` or the enriched error.
async fn explain(State(s): State<AppState>, Query(q): Query<SqlQuery>) -> impl IntoResponse {
    use crate::metrics::METRICS;
    if q.q.len() > SQL_MAX_QUERY_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "valid": false, "error": "query too long" })),
        )
            .into_response();
    }
    let permit = match Arc::clone(&s.sql_gate).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "valid": false, "error": "server busy" })),
            )
                .into_response()
        }
    };
    let dir = s.dir.clone();
    // Bind-only: the LIMIT 0 wrapper forces the planner to resolve every table/column/type without
    // materialising rows. A CTE (`WITH …`) is legal inside the subquery, so this covers both shapes.
    let probe = format!("SELECT * FROM ({}) AS _explain LIMIT 0", q.q);
    let store = s.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let hot = store.hot_rows_by_table().unwrap_or_default();
        let sealed_through = store.sealed_through();
        analytics::query_hot_cold(
            &dir,
            &probe,
            analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows: 1,
            },
            &hot,
            sealed_through,
        )
    })
    .await;
    match result {
        Ok(Ok(_)) => {
            Json(json!({ "valid": true, "note": "query binds; run it with the sql tool" }))
                .into_response()
        }
        Ok(Err(e)) => {
            METRICS.inc_sql_rejected();
            let raw = sanitize_sql_error(&format!("{e:#}"), &s.dir);
            let msg = match crate::sql_errors::enrich(&raw, &q.q, &s.tables) {
                Some(hint) => format!("{raw}\n\nhint: {hint}"),
                None => raw,
            };
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "valid": false, "error": msg })),
            )
                .into_response()
        }
        Err(e) => error(format!("{e}")),
    }
}

/// Top balances from the IVM view, descending. Balances are in i64 token base units.
async fn balances(State(s): State<AppState>, Query(q): Query<EntitiesQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    // Balances are i128 base units, serialised as decimal strings — JSON numbers can't carry i128
    // losslessly, and a client parsing a huge balance as an f64 would silently corrupt it.
    let items: Vec<Value> = s
        .balances
        .top(limit)
        .into_iter()
        .map(|(address, balance)| json!({ "address": address, "balance": balance.to_string() }))
        .collect();
    Json(json!({ "holders": s.balances.holders(), "count": items.len(), "items": items }))
}

/// Point-read a single address's derived balance.
async fn balance(State(s): State<AppState>, Path(address): Path<String>) -> impl IntoResponse {
    let address = address.to_ascii_lowercase();
    match s.balances.balance(&address) {
        Some(b) => Json(json!({ "address": address, "balance": b.to_string() })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no balance", "address": address })),
        )
            .into_response(),
    }
}

/// Direct counterparty-exposure for an address: how much it has transacted, directly, with the
/// labeled set (RFC-0008 C1). Amounts are i128 base units, serialised as decimal strings (same reason
/// as balances). A reorg retracts through the IVM view, so this is always the canonical-chain figure.
async fn exposure(State(s): State<AppState>, Path(address): Path<String>) -> impl IntoResponse {
    let address = address.to_ascii_lowercase();
    let items: Vec<Value> = s
        .exposure
        .exposure(&address)
        .into_iter()
        .map(|r| {
            json!({
                "label": r.label,
                "direction": r.direction,
                "count": r.count.to_string(),
                "amount": r.amount.to_string(),
            })
        })
        .collect();
    Json(json!({ "address": address, "count": items.len(), "exposure": items }))
}

#[derive(Deserialize)]
struct FlagsQuery {
    kind: Option<String>,
    limit: Option<usize>,
}

/// Compliance flags (RFC-0008 C3). `?kind=velocity` returns the live windowed velocity flags (address
/// volume ≥ the configured threshold within a block-window); `?kind=threshold` returns recent
/// `threshold_flag` annotations (hot store; the full sealed history is at `/sql SELECT * FROM
/// threshold_flag`). Omit `kind` for both. Amounts are i128 base units, serialised as decimal strings.
async fn flags(State(s): State<AppState>, Query(q): Query<FlagsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    let kind = q.kind.as_deref();

    let velocity = |s: &AppState| -> Vec<Value> {
        let threshold = s.velocity_threshold.unwrap_or(i128::MIN);
        s.velocity
            .flags(threshold)
            .into_iter()
            .take(limit)
            .map(|f| {
                json!({
                    "address": f.address,
                    "window_start": f.window_start,
                    "count": f.count.to_string(),
                    "volume": f.volume.to_string(),
                })
            })
            .collect()
    };
    // Recent threshold_flag annotations from the hot store (newest first). Sealed history via /sql.
    let threshold = |s: &AppState| -> Vec<Value> {
        s.store
            .recent_by_table("threshold_flag", limit)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_str::<Value>(r).ok())
            .collect()
    };

    match kind {
        Some("velocity") => Json(json!({
            "kind": "velocity",
            "threshold": s.velocity_threshold.map(|t| t.to_string()),
            "flags": velocity(&s),
        }))
        .into_response(),
        Some("threshold") => Json(json!({
            "kind": "threshold",
            "threshold": s.threshold.map(|t| t.to_string()),
            "flags": threshold(&s),
            "note": "recent hot flags; full sealed history: /sql?q=SELECT * FROM threshold_flag",
        }))
        .into_response(),
        Some(other) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown flag kind '{other}' (want: threshold, velocity)") })),
        )
            .into_response(),
        None => Json(json!({
            "threshold": { "configured": s.threshold.map(|t| t.to_string()), "flags": threshold(&s) },
            "velocity": { "configured": s.velocity_threshold.map(|t| t.to_string()), "flags": velocity(&s) },
        }))
        .into_response(),
    }
}

/// Parse an entity id `{block:012}-{log_index:06}` back into its components.
fn parse_id(id: &str) -> Option<(u64, u64)> {
    let (b, l) = id.split_once('-')?;
    Some((b.parse().ok()?, l.parse().ok()?))
}

fn not_found(id: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "id": id })),
    )
        .into_response()
}

fn error(msg: String) -> axum::response::Response {
    // SEC-11: internal error chains include absolute segment/dir paths (a filesystem-layout disclosure).
    // Log the detail server-side; hand the client a generic message.
    tracing::warn!("internal error serving request: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal error" })),
    )
        .into_response()
}

/// Constant-time string equality for the admin token — no early return on the first differing byte, so
/// an attacker can't time-recover the secret. The length check leaks only the length, which is fixed
/// for a random token.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Redact the nest-directory prefix from a DuckDB error before it's relayed to a `/sql`/`/explain`
/// client (SEC review). DuckDB embeds absolute segment paths (`…/mynest/segments/foo.parquet`) in
/// binder/IO errors; the useful part for the caller is the table/column/type detail, not the on-disk
/// layout. We keep the message and replace only the dir prefix with `<nest>`.
fn sanitize_sql_error(raw: &str, dir: &std::path::Path) -> String {
    let mut out = raw.to_string();
    // Both the canonical and as-configured forms — the error could carry either.
    for p in [dir.canonicalize().ok(), Some(dir.to_path_buf())]
        .into_iter()
        .flatten()
    {
        let s = p.display().to_string();
        if !s.is_empty() {
            out = out.replace(&s, "<nest>");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_only_identical_strings() {
        assert!(ct_eq("s3cret", "s3cret"));
        assert!(!ct_eq("s3cret", "s3creT"));
        assert!(!ct_eq("s3cret", "s3cre")); // length differs
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn sanitize_sql_error_redacts_the_nest_dir() {
        let dir = std::path::Path::new("/var/lib/nuthatch/mynest");
        let raw = "IO Error: No files found that match the pattern \
                   \"/var/lib/nuthatch/mynest/segments/usdc__transfer-abc.parquet\"";
        let out = sanitize_sql_error(raw, dir);
        assert!(
            !out.contains("/var/lib/nuthatch/mynest"),
            "dir prefix redacted"
        );
        assert!(out.contains("<nest>/segments/usdc__transfer-abc.parquet"));
        // The useful DuckDB detail (the message + filename) survives.
        assert!(out.contains("No files found"));
    }

    /// A minimal but real `AppState` — enough to drive the analytical handlers directly (no HTTP
    /// harness). `permits` seeds the admission gate so a test can saturate it.
    fn test_state(dir: &std::path::Path, permits: usize) -> AppState {
        AppState {
            store: Store::open(&dir.join("t.redb")).unwrap(),
            address: "0x0".into(),
            chain: "ethereum".into(),
            dir: dir.to_path_buf(),
            balances: BalanceView::start().unwrap(),
            exposure: ExposureView::start(true).unwrap(),
            velocity: VelocityView::start(true).unwrap(),
            threshold: None,
            velocity_threshold: None,
            tables: Arc::new(vec![]),
            sql_gate: Arc::new(Semaphore::new(permits)),
            admin_enabled: true,
            admin_token: None,
            nest_info: Arc::new(json!({ "name": "t" })),
        }
    }

    /// When the analytical gate is saturated, `/sql` fails fast with 503 rather than piling on.
    #[tokio::test]
    async fn sql_returns_503_when_gate_saturated() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), 1);
        // Hold the only permit — the gate is now saturated for the duration of the call.
        let held = Arc::clone(&state.sql_gate).try_acquire_owned().unwrap();
        let resp = sql(
            State(state.clone()),
            Query(SqlQuery {
                q: "SELECT 1".into(),
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
    }

    /// An over-length query string is rejected (400) before it ever reaches the planner.
    #[tokio::test]
    async fn sql_rejects_overlong_query() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let long = format!("SELECT {}", "1,".repeat(SQL_MAX_QUERY_LEN)); // well past the cap
        let resp = sql(
            State(state),
            Query(SqlQuery {
                q: long,
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A well-formed query passes the gate and runs (200) — the guard doesn't block legitimate use.
    #[tokio::test]
    async fn sql_serves_a_normal_query() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let resp = sql(
            State(state),
            Query(SqlQuery {
                q: "SELECT 1 AS n".into(),
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// RFC-0010 Part A: the admin UI serves when enabled and 404s when disabled (`--no-admin` or a
    /// public bind without a token). `/nest` returns the static nest metadata either way.
    #[tokio::test]
    async fn admin_ui_gated_and_nest_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let no_tok = || Query(AdminQuery { token: None });

        // Localhost (admin_token None): open.
        let resp = admin_index(State(state.clone()), no_tok())
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK, "admin UI open on localhost");

        state.admin_enabled = false;
        let resp = admin_index(State(state.clone()), no_tok())
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "admin UI 404s when disabled"
        );

        let resp = nest(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_token_enforced_off_localhost() {
        // SEC-5: with a required token set (off-localhost), the route must actually CHECK it per
        // request — not merely be enabled by the env var's presence.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        state.admin_token = Some("s3cret".into());

        let tok = |t: Option<&str>| {
            Query(AdminQuery {
                token: t.map(str::to_string),
            })
        };
        // No token → 401.
        assert_eq!(
            admin_index(State(state.clone()), tok(None))
                .await
                .into_response()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Wrong token → 401.
        assert_eq!(
            admin_index(State(state.clone()), tok(Some("nope")))
                .await
                .into_response()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // Correct token → 200.
        assert_eq!(
            admin_index(State(state), tok(Some("s3cret")))
                .await
                .into_response()
                .status(),
            StatusCode::OK
        );
    }

    #[test]
    fn admin_html_is_embedded_and_bounded() {
        assert!(ADMIN_HTML.contains("<title>nuthatch</title>"));
        // The RFC-0010 budget: the embedded UI stays well under 150 KB and pulls in nothing external.
        assert!(ADMIN_HTML.len() < 150 * 1024, "admin UI ≤ 150 KB");
        assert!(
            !ADMIN_HTML.contains("http://") && !ADMIN_HTML.contains("https://"),
            "admin UI makes no external requests (same-origin only)"
        );
    }
}
