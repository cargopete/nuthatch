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

/// RFC-0020 slice 2b — the compatible hot-upgrade: two real `spawn_nest` indexers (old + new version)
/// run concurrently against the same scripted chain; the endpoint serves the OLD version until the NEW
/// one catches up, then `await_catchup_and_flip` atomically re-points the served backing to the new
/// version. Deterministic (no network, no sleeps driving the pipeline) — the old/new backings are told
/// apart by their `dir`. This is the full concurrent-reindex-then-flip proven end to end.
#[tokio::test]
async fn compatible_hot_upgrade_flips_backing_after_catchup() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let old_cfg = scaffold_nest(old_dir.path(), "usdc", USDC);
    let new_cfg = scaffold_nest(new_dir.path(), "usdc", USDC);

    // One scripted chain both versions follow: five blocks, one USDC transfer each.
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=5u64 {
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
    tape.advance_tip_to(5);

    let old_rt = indexer::spawn_nest(
        tape.clone(),
        old_dir.path().to_path_buf(),
        old_cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn old");
    let new_rt = indexer::spawn_nest(
        tape.clone(),
        new_dir.path().to_path_buf(),
        new_cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn new");

    let old_store = old_rt.state.store.clone();
    let new_store = new_rt.state.store.clone();
    let new_state = new_rt.state; // handed to the flip
    let shared = serve::SharedNest::new(old_rt.state);

    // Before the flip, the endpoint is backed by the OLD version.
    assert_eq!(shared.current().dir.as_path(), old_dir.path());

    // Concurrent re-index + atomic flip: returns once the new version has caught up to the old.
    tokio::time::timeout(
        POLL_TIMEOUT,
        indexer::await_catchup_and_flip(
            &shared,
            &old_store,
            &new_store,
            new_state,
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("flip timed out")
    .expect("flip");

    // After the flip, the SAME endpoint is now backed by the NEW version, caught up to the tip.
    assert_eq!(shared.current().dir.as_path(), new_dir.path());
    assert_eq!(new_store.indexed_head().unwrap(), Some(5));

    old_rt.ingest.abort();
    new_rt.ingest.abort();
}

/// RFC-0020 slice 3 — the breaking path: two versions served on distinct endpoints over one listener.
/// The OLD version stays at the root (its consumers unchanged) but every response carries a
/// `Deprecation: true` header + a `Link` to the successor; the NEW version is served under `/next` and
/// is not deprecated. Distinct nest aliases (`usdc` vs `usdcv2`) make the two schemas tell-apart-able.
#[tokio::test]
async fn breaking_upgrade_serves_both_versions_with_old_deprecated() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let old_cfg = scaffold_nest(old_dir.path(), "usdc", USDC);
    let new_cfg = scaffold_nest(new_dir.path(), "usdcv2", USDC);

    // One block so both indexers have something to chew; `/schema` itself comes from the registry.
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    tape.insert_block(
        1,
        transfers_block(
            1,
            0,
            1_700_000_001,
            USDC,
            &[(a1.as_str(), a2.as_str(), 100)],
        ),
    );
    tape.advance_tip_to(1);

    let old_rt = indexer::spawn_nest(
        tape.clone(),
        old_dir.path().to_path_buf(),
        old_cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn old");
    let new_rt = indexer::spawn_nest(
        tape.clone(),
        new_dir.path().to_path_buf(),
        new_cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn new");

    let app = serve::two_version_router(
        serve::SharedNest::new(old_rt.state),
        "/next",
        serve::SharedNest::new(new_rt.state),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Old at root: its schema, plus a Deprecation header pointing at the successor.
    let old = client.get(format!("{base}/schema")).send().await.unwrap();
    assert_eq!(old.headers().get("deprecation").unwrap(), "true");
    assert!(old
        .headers()
        .get("link")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("successor-version"));
    let old_body = old.text().await.unwrap();
    assert!(old_body.contains("usdc__"), "old schema served at root");
    assert!(!old_body.contains("usdcv2__"), "root is the OLD version");

    // New under /next: its (different) schema, and NOT deprecated.
    let new = client
        .get(format!("{base}/next/schema"))
        .send()
        .await
        .unwrap();
    assert!(
        new.headers().get("deprecation").is_none(),
        "the new endpoint is not deprecated"
    );
    let new_body = new.text().await.unwrap();
    assert!(
        new_body.contains("usdcv2__"),
        "new schema served under /next"
    );

    old_rt.ingest.abort();
    new_rt.ingest.abort();
    server.abort();
}

/// RFC-0023 tier 1 — derive-first: the `total_supply` recipe computes ERC-20 `totalSupply()` from the
/// Transfer events already indexed (Σ minted − Σ burned), with **no eth_call**. Derive-correctness: the
/// derived value equals the hand-computed mints − burns — the thing a subgraph pays an archive node to
/// fetch, nuthatch derives for free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn total_supply_recipe_derives_mints_minus_burns() {
    use nuthatch::recipes;

    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    let zero = recipes::ZERO_ADDRESS;
    let (a1, a2) = (account(1), account(2));

    // mint 1000 → a1, mint 500 → a2, burn 200 from a1, and a normal a1→a2 transfer (no supply change).
    tape.insert_block(
        1,
        transfers_block(1, 0, 1_700_000_001, USDC, &[(zero, a1.as_str(), 1000)]),
    );
    tape.insert_block(
        2,
        transfers_block(2, 0, 1_700_000_002, USDC, &[(zero, a2.as_str(), 500)]),
    );
    tape.insert_block(
        3,
        transfers_block(3, 0, 1_700_000_003, USDC, &[(a1.as_str(), zero, 200)]),
    );
    tape.insert_block(
        4,
        transfers_block(
            4,
            0,
            1_700_000_004,
            USDC,
            &[(a1.as_str(), a2.as_str(), 100)],
        ),
    );
    tape.advance_tip_to(4);
    tape.advance_finalized_to(4);
    tape.insert_block(5, empty_block(5, 0, 1_700_000_005));
    tape.advance_tip_to(5);

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
    let store = rt.state.store.clone();
    assert!(
        wait_until(POLL_TIMEOUT, || store.sealed_through() >= 4).await,
        "transfers did not seal"
    );

    // Derived totalSupply = 1000 + 500 − 200 = 1300. No eth_call, no archive node.
    let rows = analytics::query(dir.path(), &recipes::total_supply_select("usdc")).unwrap();
    let v = &rows[0]["total_supply"];
    let got = v
        .as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| v.to_string());
    assert_eq!(
        got, "1300",
        "derived total_supply must equal Σ mints − Σ burns"
    );

    rt.ingest.abort();
    if let Some(w) = rt.alert_worker {
        w.abort();
    }
}

/// RFC-0020 slice 4 — segment reuse: a compatible update whose decode is unchanged mounts the old
/// version's sealed segments instead of re-indexing. Here a fresh nest, given ONLY the old's segments +
/// watermark (never having indexed a block itself), serves the sealed history — the true no-re-index
/// path, and a capability subgraphs structurally lack (their storage isn't content-addressed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compatible_upgrade_reuses_sealed_segments_when_decode_unchanged() {
    use nuthatch::{lifecycle, store::Store};

    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    // Same alias → same decode + same table names, so reuse is valid (a view/semantic-only update).
    let cfg = scaffold_nest(old.path(), "usdc", USDC);
    scaffold_nest(new.path(), "usdc", USDC);
    for d in [old.path(), new.path()] {
        std::fs::write(
            d.join("schema.json"),
            r#"{"registry_hash":"0xnest","tables":[]}"#,
        )
        .unwrap();
    }

    // Index ten blocks and seal [1,5] into the OLD version — inside a scope so every redb handle drops
    // before reuse reopens it (redb is single-writer).
    {
        let tape = Arc::new(TapeSource::new());
        let (a1, a2) = (account(1), account(2));
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
        let rt = indexer::spawn_nest(
            tape.clone(),
            old.path().to_path_buf(),
            cfg,
            None,
            false,
            1,
            Some(2),
            false,
            None,
        )
        .await
        .expect("spawn old");
        let store = rt.state.store.clone();
        assert!(
            wait_until(POLL_TIMEOUT, || store
                .get_meta("last_block")
                .ok()
                .flatten()
                .as_deref()
                == Some("10"))
            .await,
            "old did not index to the tip"
        );
        tape.advance_finalized_to(5);
        tape.insert_block(11, empty_block(11, 0, 1_700_000_100));
        tape.advance_tip_to(11);
        assert!(
            wait_until(POLL_TIMEOUT, || store.sealed_through() >= 5).await,
            "old did not seal [1,5]"
        );
        rt.ingest.abort();
        let _ = rt.ingest.await;
        drop(store);
    }

    // Mount the old's sealed segments into the fresh new nest.
    match lifecycle::reuse_segments(old.path(), new.path()).unwrap() {
        lifecycle::ReuseOutcome::Reused {
            sealed_through,
            segments,
        } => {
            assert_eq!(sealed_through, 5, "watermark carried over");
            assert!(segments >= 1, "at least one segment reused");
        }
        other => panic!("expected Reused, got {other:?}"),
    }

    // The new nest now serves the reused sealed history WITHOUT ever having indexed a block.
    assert!(new.path().join("segments/manifest.json").exists());
    {
        let new_store = Store::open(&new.path().join("nuthatch.redb")).unwrap();
        assert_eq!(
            new_store.sealed_through(),
            5,
            "new resumes past the reused range"
        );
    }
    let rows = analytics::query(
        new.path(),
        &format!(
            "SELECT block_number FROM \"{}\" ORDER BY block_number",
            transfer_table("usdc")
        ),
    )
    .unwrap();
    let blocks: BTreeSet<u64> = rows
        .iter()
        .map(|r| r["block_number"].as_u64().unwrap())
        .collect();
    assert_eq!(
        blocks,
        (1..=5).collect::<BTreeSet<u64>>(),
        "the fresh new nest serves exactly the reused sealed segments"
    );
}
