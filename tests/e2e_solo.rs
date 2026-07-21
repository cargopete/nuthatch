//! End-to-end solo-pipeline tests: golden land → seal → query, and serving over real HTTP.
//!
//! Both drive the real `indexer::spawn_nest` background loop against a scripted [`TapeSource`], and
//! observe progress by bounded polling on the hot store / HTTP — no fixed sleeps drive the pipeline.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use nuthatch::{analytics, indexer, seal, serve};

use common::tape::*;

/// Query timeout for the read-only surface in tests — generous; these fixtures are tiny.
fn guard() -> analytics::QueryGuard {
    analytics::QueryGuard {
        timeout: Duration::from_secs(10),
        max_rows: 100_000,
    }
}

/// Drive one solo nest through land → seal, assert the hot/cold split, and return the sealed
/// `usdc__transfer` segment content-hashes. Called twice over identical fixtures to prove the sealed
/// content address is deterministic across runs.
async fn drive_land_seal_query(dir: &std::path::Path) -> Vec<String> {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    let tape = Arc::new(TapeSource::new());

    // Ten blocks, one USDC transfer each, distinct value 100*b. Finality stays at 0 → nothing seals.
    let a1 = account(1);
    let a2 = account(2);
    for b in 1..=10u64 {
        tape.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
            ),
        );
    }
    tape.advance_tip_to(10);

    // Small getLogs window so sealing has a boundary short of the full range.
    let rt = indexer::spawn_nest(
        tape.clone(),
        dir.to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let store = rt.state.store.clone();

    // Land: index all ten blocks into the hot store.
    let landed = wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some("10")
    })
    .await;
    assert!(landed, "nest did not index to the tip in time");

    // Hot rows for the whole range are present; nothing sealed yet.
    assert_eq!(
        store.entities_in_range(1, 10).unwrap().len(),
        10,
        "all ten transfers should be in the hot store"
    );
    assert_eq!(store.sealed_through(), 0, "nothing is final yet");
    assert!(
        seal::load_manifest(dir).unwrap().tables.is_empty(),
        "no segments before finality advances"
    );

    // Seal: finalize through block 5, then push an empty block 11 so a fresh window is processed and
    // the newly-finalized range seals to Parquet.
    tape.advance_finalized_to(5);
    tape.insert_block(11, empty_block(11, 0, 1_700_000_100));
    tape.advance_tip_to(11);

    let sealed = wait_until(POLL_TIMEOUT, || store.sealed_through() >= 5).await;
    assert!(sealed, "range [1,5] did not seal in time");

    // Segments now exist for the transfer table.
    let manifest = seal::load_manifest(dir).unwrap();
    let segs = manifest
        .tables
        .get(&transfer_table("usdc"))
        .expect("transfer table sealed");
    assert!(!segs.is_empty(), "expected at least one sealed segment");

    // Cold-only query sees ONLY the sealed subset (blocks 1..=5); the hot tip (6..=10) is invisible.
    let cold = analytics::query(
        dir,
        &format!(
            "SELECT block_number FROM \"{}\" ORDER BY block_number",
            transfer_table("usdc")
        ),
    )
    .unwrap();
    let cold_blocks: BTreeSet<u64> = cold
        .iter()
        .map(|r| r["block_number"].as_u64().unwrap())
        .collect();
    assert_eq!(
        cold_blocks,
        (1..=5).collect::<BTreeSet<u64>>(),
        "cold-only query must return exactly the sealed subset"
    );

    // Hot+cold query spans BOTH the sealed range and the live tip (blocks 1..=10).
    let hot = store.hot_rows_by_table().unwrap();
    let hc = analytics::query_hot_cold(
        dir,
        &format!(
            "SELECT block_number FROM \"{}\" ORDER BY block_number",
            transfer_table("usdc")
        ),
        guard(),
        &hot,
        store.sealed_through(),
    )
    .unwrap();
    let hc_blocks: BTreeSet<u64> = hc
        .rows
        .iter()
        .map(|r| r["block_number"].as_u64().unwrap())
        .collect();
    assert_eq!(
        hc_blocks,
        (1..=10).collect::<BTreeSet<u64>>(),
        "hot+cold query must span the sealed range and the hot tip"
    );

    let hashes: Vec<String> = segs.iter().map(|s| s.hash.clone()).collect();

    rt.ingest.abort();
    if let Some(w) = rt.alert_worker {
        w.abort();
    }
    hashes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn golden_land_seal_query_is_deterministic() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let h1 = drive_land_seal_query(d1.path()).await;
    let h2 = drive_land_seal_query(d2.path()).await;
    assert!(!h1.is_empty());
    assert_eq!(
        h1, h2,
        "sealed content-address hashes must be identical across two runs over identical fixtures"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_over_real_http() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = Arc::new(TapeSource::new());

    // Three transfers across three blocks; value = 100*b. All hot (finality stays at 0).
    let a1 = account(1);
    let a2 = account(2);
    for b in 1..=3u64 {
        tape.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
            ),
        );
    }
    tape.advance_tip_to(3);

    let rt = indexer::spawn_nest(
        tape.clone(),
        dir.path().to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let ingest = rt.ingest;
    let alert_worker = rt.alert_worker;

    // Bind our own listener and serve the real router on a task.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = serve::router(serve::SharedNest::new(rt.state));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Poll `/` for entities > 0 with a bounded timeout (no fixed sleep).
    let mut entities = 0u64;
    let start = std::time::Instant::now();
    while start.elapsed() < POLL_TIMEOUT {
        if let Ok(resp) = client.get(&base).send().await {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                entities = v["entities"].as_u64().unwrap_or(0);
                if entities >= 3 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(entities, 3, "expected all three transfers served at `/`");

    // GET / — summary shape.
    let root: serde_json::Value = client
        .get(&base)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(root["name"], "nuthatch");
    assert_eq!(root["chain"], "arbitrum-one");
    assert_eq!(root["entities"], 3);
    assert_eq!(root["last_block"], "3");

    // GET /tables — the decoded data model.
    let tables: serde_json::Value = client
        .get(format!("{base}/tables"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tables["count"].as_u64().unwrap() >= 1);
    let names: Vec<&str> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["table"].as_str())
        .collect();
    assert!(
        names.contains(&transfer_table("usdc").as_str()),
        "tables should list usdc__transfer, got {names:?}"
    );

    // GET /sql — count matches the fed transfers.
    let sql: serde_json::Value = client
        .get(format!("{base}/sql"))
        .query(&[(
            "q",
            format!("SELECT count(*) AS n FROM \"{}\"", transfer_table("usdc")),
        )])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sql["count"], 1, "one aggregate row");
    // DuckDB returns count as a number; compare loosely against 3.
    assert_eq!(
        sql["rows"][0]["n"].as_u64().unwrap(),
        3,
        "sql sees three rows"
    );

    // GET /entity/{id} — the block-1 transfer, value 100.
    let id = nuthatch::store::Store::entity_key(1, 0);
    let entity: serde_json::Value = client
        .get(format!("{base}/entity/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(entity["block_number"], 1);
    assert_eq!(entity["value"], "100");
    assert_eq!(entity["table"], transfer_table("usdc"));

    // GET /health — plain "ok".
    let health = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(health, "ok");

    ingest.abort();
    if let Some(w) = alert_worker {
        w.abort();
    }
    server.abort();
}
