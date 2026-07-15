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

use crate::analytics;
use crate::registry::TableSchema;
use crate::store::Store;
use crate::views::BalanceView;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub address: String,
    pub chain: String,
    pub dir: PathBuf,
    pub balances: BalanceView,
    /// The nest's table schemas (from the decode registry) — the source of truth for `/tables`.
    pub tables: Arc<Vec<TableSchema>>,
}

pub async fn run(listen: &str, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(summary))
        .route("/health", get(|| async { "ok" }))
        .route("/tables", get(tables))
        .route("/table/{name}", get(table))
        .route("/entities", get(entities))
        .route("/entity/{id}", get(entity))
        .route("/sql", get(sql))
        .route("/balances", get(balances))
        .route("/balance/{address}", get(balance))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;
    tracing::info!("API live on http://{listen}  (try GET /  and  /entities)");
    axum::serve(listener, app).await.context("server error")?;
    Ok(())
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
        "tables": s.tables.len(),
        "views": ["balances (IVM)"],
        "endpoints": [
            "/health",
            "/tables",
            "/table/{name}?limit=100",
            "/entities?limit=100",
            "/entity/{block:012}-{log_index:06}",
            "/sql?q=SELECT count(*) FROM \"<alias>__<event>\"",
            "/balances?limit=100",
            "/balance/{address}",
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

    // Fill from cold (sealed segments) if the hot store didn't satisfy the limit.
    if items.len() < limit {
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
        if let Ok(Ok(rows)) =
            tokio::task::spawn_blocking(move || analytics::query(&dir, &sql)).await
        {
            items.extend(rows);
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
}

/// Read-only analytical SQL over the sealed segments; one view per `{alias}__{event}` table.
async fn sql(State(s): State<AppState>, Query(q): Query<SqlQuery>) -> impl IntoResponse {
    let dir = s.dir.clone();
    let result = tokio::task::spawn_blocking(move || analytics::query(&dir, &q.q)).await;
    match result {
        Ok(Ok(rows)) => Json(json!({ "count": rows.len(), "rows": rows })).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        )
            .into_response(),
        Err(e) => error(format!("{e}")),
    }
}

/// Top balances from the IVM view, descending. Balances are in i64 token base units.
async fn balances(State(s): State<AppState>, Query(q): Query<EntitiesQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    let items: Vec<Value> = s
        .balances
        .top(limit)
        .into_iter()
        .map(|(address, balance)| json!({ "address": address, "balance": balance }))
        .collect();
    Json(json!({ "holders": s.balances.holders(), "count": items.len(), "items": items }))
}

/// Point-read a single address's derived balance.
async fn balance(State(s): State<AppState>, Path(address): Path<String>) -> impl IntoResponse {
    let address = address.to_ascii_lowercase();
    match s.balances.balance(&address) {
        Some(b) => Json(json!({ "address": address, "balance": b })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no balance", "address": address })),
        )
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
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
}
