//! End-to-end reorg tests — the C1 coverage gap (`detect_reorg` has zero coverage today, because the
//! other test doubles answer `block_hash -> None` and make it a no-op).
//!
//! `TapeSource::reorg` rewrites the chain above a fork with new hashes+logs; the running
//! `spawn_nest` loop detects the divergence, rolls the hot store back, and re-indexes the canonical
//! replacement. A reorg *below* the sealed/finalized watermark must instead halt loudly.

mod common;

use std::sync::Arc;

use nuthatch::indexer;

use common::tape::*;

/// A canonical block `b` (variant 0): one USDC transfer, value `100*b`.
fn canonical_block(b: u64) -> BlockFixture {
    let a1 = account(1);
    let a2 = account(2);
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
    )
}

/// A replacement block `b` (variant 1 → distinct hash): one USDC transfer with a distinct value, so a
/// naive same-key overwrite would be visibly wrong unless a real rollback + re-index happened.
fn replacement_block(b: u64) -> BlockFixture {
    let a1 = account(3);
    let a2 = account(4);
    transfers_block(
        b,
        1,
        1_700_000_500 + b,
        USDC,
        &[(a1.as_str(), a2.as_str(), (7_000 + b) as u128)],
    )
}

/// Index a fresh nest over `tape` until it reaches `last_block == tip`, returning `(runtime, store)`.
async fn spawn_indexed(
    dir: &std::path::Path,
    tape: Arc<TapeSource>,
    tip: u64,
) -> (indexer::NestRuntime, nuthatch::store::Store) {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    let rt = indexer::spawn_nest(
        tape,
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
    let tip_str = tip.to_string();
    let landed = wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some(tip_str.as_str())
    })
    .await;
    assert!(landed, "nest did not index to block {tip} in time");
    (rt, store)
}

fn shutdown(rt: indexer::NestRuntime) {
    rt.ingest.abort();
    if let Some(w) = rt.alert_worker {
        w.abort();
    }
}

/// The core convergence property: after a reorg at `fork` (0 < fork < 10), the reorged nest's hot
/// state converges byte-for-byte to a clean nest indexed directly over the post-reorg chain.
async fn converge_after_reorg(fork: u64) {
    assert!((1..10).contains(&fork));

    // Reorged nest: index the canonical chain 1..=10, then reorg above `fork`.
    let reorged_dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=10u64 {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(10);
    let (rt, store) = spawn_indexed(reorged_dir.path(), tape.clone(), 10).await;

    // Rewrite blocks (fork, 10] with the replacement chain.
    let replacement: Vec<BlockFixture> = ((fork + 1)..=10).map(replacement_block).collect();
    tape.reorg(fork, replacement);

    // Convergence signal: block 10's stored row carries the replacement block hash (proving the
    // rollback + re-index actually ran, not a stale canonical row).
    let want_hash = block_hash(10, 1);
    let converged = wait_until(POLL_TIMEOUT, || {
        match store.get_entity(&nuthatch::store::Store::entity_key(10, 0)) {
            Ok(Some(raw)) => serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v["block_hash"].as_str().map(|h| h == want_hash))
                .unwrap_or(false),
            _ => false,
        }
    })
    .await;
    assert!(converged, "fork={fork}: reorg did not reconverge in time");
    let reorged_rows = store.entities_in_range(1, 10).unwrap();
    shutdown(rt);

    // Clean nest: index the post-reorg chain directly (1..=fork canonical, fork+1..=10 replacement).
    let clean_dir = tempfile::tempdir().unwrap();
    let clean_tape = Arc::new(TapeSource::new());
    for b in 1..=fork {
        clean_tape.insert_block(b, canonical_block(b));
    }
    for b in (fork + 1)..=10 {
        clean_tape.insert_block(b, replacement_block(b));
    }
    clean_tape.advance_tip_to(10);
    let (clean_rt, clean_store) = spawn_indexed(clean_dir.path(), clean_tape, 10).await;
    let clean_rows = clean_store.entities_in_range(1, 10).unwrap();
    shutdown(clean_rt);

    assert_eq!(
        reorged_rows, clean_rows,
        "fork={fork}: reorged hot state must equal a clean run over the post-reorg chain"
    );
}

// Proptest over random fork depths. Each case drives a full reorg + a clean reference nest, so the
// case count is kept low (the loop's ~2 s idle re-poll bounds each reorg's detection latency); this is
// plenty to exercise the ancestor-walk across every window boundary. A single shared multi-thread
// runtime backs all cases.
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]
    #[test]
    fn reorg_converges_to_canonical(fork in 1u64..10) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(converge_after_reorg(fork));
    }
}

/// A reorg *below* the sealed/finalized watermark is a finality violation this model can't repair —
/// the doomed blocks are already in immutable sealed segments. The loop must halt loudly (return an
/// error), not silently corrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reorg_below_finality_halts() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    for b in 1..=10u64 {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(10);

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
    let ingest = rt.ingest;

    let landed = wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some("10")
    })
    .await;
    assert!(landed, "nest did not index to the tip in time");

    // Seal [1,8]: finalize through 8, then push two empty blocks so a window processes and seals.
    tape.advance_finalized_to(8);
    tape.insert_block(11, empty_block(11, 0, 1_700_000_111));
    tape.insert_block(12, empty_block(12, 0, 1_700_000_112));
    tape.advance_tip_to(12);
    let sealed = wait_until(POLL_TIMEOUT, || store.sealed_through() >= 8).await;
    assert!(sealed, "range [1,8] did not seal in time");

    // Reorg at block 5 — below the finalized watermark (8). The replacement rewrites blocks 6..=12.
    let mut replacement: Vec<BlockFixture> = (6..=10).map(replacement_block).collect();
    replacement.push(empty_block(11, 1, 1_700_000_611));
    replacement.push(empty_block(12, 1, 1_700_000_612));
    tape.reorg(5, replacement);

    // The ingest loop must END with an error (not run forever, not exit cleanly).
    let outcome = tokio::time::timeout(POLL_TIMEOUT, ingest)
        .await
        .expect("ingest loop should have halted, not run forever");
    let inner = outcome.expect("ingest task should not panic");
    let err = inner.expect_err("a sub-finality reorg must halt with an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("finality") || msg.contains("sealed"),
        "expected a finality-violation error, got: {msg}"
    );

    if let Some(w) = rt.alert_worker {
        w.abort();
    }
}
