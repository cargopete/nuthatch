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

use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub address: String,
    pub chain: String,
}

pub async fn run(listen: &str, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(summary))
        .route("/health", get(|| async { "ok" }))
        .route("/entities", get(entities))
        .route("/entity/{id}", get(entity))
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
        "event": "Transfer",
        "entities": count,
        "last_block": last_block,
        "endpoints": ["/health", "/entities?limit=100", "/entity/{block:012}-{log_index:06}"],
    }))
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
    match s.store.get_entity(&id) {
        Ok(Some(raw)) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => Json(v).into_response(),
            Err(e) => error(format!("{e:#}")),
        },
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found", "id": id }))).into_response(),
        Err(e) => error(format!("{e:#}")),
    }
}

fn error(msg: String) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": msg }))).into_response()
}
