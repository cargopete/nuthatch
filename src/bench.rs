//! `nuthatch bench backfill` — honest, reproducible backfill throughput measurement (RFC-0004).
//!
//! Runs the real fetch → decode → store path over a *pinned* block range and reports sustained
//! events/sec, wall-clock, peak RSS, and RPC requests, emitting a `bench-report.json`. Measure
//! first, optimise second: nothing here optimises anything — it establishes the baseline that the
//! seal-direct / adaptive-chunker / pipeline work (later slices) must each beat on a before/after.
//!
//! The house rule (RFC-0004): every published number traces to a report artifact produced here, with
//! date, provider, hardware, and commit. No hand-typed numbers on the website.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::chains;
use crate::cli::BackfillBenchArgs;
use crate::config::Config;
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::source::Source;
use crate::store::Store;

/// One run's raw measurements.
struct Run {
    events: u64,
    wall_clock_s: f64,
    events_per_sec: f64,
    peak_rss_mb: u64,
    rpc_requests: u64,
}

/// The published artifact: medians across runs plus the pinned inputs and provenance.
#[derive(Debug, Serialize)]
pub struct BenchReport {
    pub bench: &'static str,
    pub label: Option<String>,
    pub nest: String,
    pub chain: String,
    pub from_block: u64,
    pub to_block: u64,
    pub blocks: u64,
    pub window: u64,
    pub seal_direct: bool,
    pub concurrency: usize,
    pub runs: usize,
    /// Medians across runs.
    pub events: u64,
    pub wall_clock_s: f64,
    pub events_per_sec: f64,
    pub peak_rss_mb: u64,
    pub rpc_requests: u64,
    pub commit: Option<String>,
}

pub async fn backfill(args: BackfillBenchArgs) -> Result<()> {
    if args.to < args.from {
        bail!("--to ({}) is before --from ({})", args.to, args.from);
    }
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, &config)?);

    let rpc_urls = match &args.rpc {
        Some(u) => vec![u.clone()],
        None => config.nest.rpc_urls.clone(),
    };
    let window = chains::lookup(&config.nest.chain)
        .map(|c| c.log_window)
        .unwrap_or(20);
    let addresses: Vec<String> = registry
        .addresses()
        .iter()
        .map(|a| format!("0x{}", hex::encode(a)))
        .collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    println!(
        "bench backfill: nest '{}' on {}, blocks {}..={} ({} blocks), window {}, {} run(s)",
        config.nest.name,
        config.nest.chain,
        args.from,
        args.to,
        args.to - args.from + 1,
        window,
        args.runs,
    );

    println!(
        "storage path: {}{}",
        if args.seal_direct {
            "seal-direct (decode → Parquet, no hot store)"
        } else {
            "hot store (decode → redb)"
        },
        if args.seal_direct && args.concurrency > 1 {
            format!(", {}-way concurrent fetch", args.concurrency)
        } else {
            String::new()
        }
    );

    let mut runs = Vec::with_capacity(args.runs);
    for run in 1..=args.runs {
        let r = one_run(
            &rpc_urls,
            &registry,
            &addresses,
            &topic0s,
            args.from,
            args.to,
            window,
            args.seal_direct,
            args.concurrency,
            run,
        )
        .await?;
        println!(
            "  run {run}/{}: {} events in {:.1}s = {:.0} ev/s, peak {} MB, {} rpc req",
            args.runs, r.events, r.wall_clock_s, r.events_per_sec, r.peak_rss_mb, r.rpc_requests
        );
        runs.push(r);
    }

    let report = BenchReport {
        bench: "backfill",
        label: args.label.clone(),
        nest: config.nest.name.clone(),
        chain: config.nest.chain.clone(),
        from_block: args.from,
        to_block: args.to,
        blocks: args.to - args.from + 1,
        window,
        seal_direct: args.seal_direct,
        concurrency: if args.seal_direct {
            args.concurrency
        } else {
            1
        },
        runs: args.runs,
        events: median_u64(runs.iter().map(|r| r.events)),
        wall_clock_s: round2(median_f64(runs.iter().map(|r| r.wall_clock_s))),
        events_per_sec: round2(median_f64(runs.iter().map(|r| r.events_per_sec))),
        peak_rss_mb: median_u64(runs.iter().map(|r| r.peak_rss_mb)),
        rpc_requests: median_u64(runs.iter().map(|r| r.rpc_requests)),
        commit: git_commit(),
    };

    let json = serde_json::to_string_pretty(&report)?;
    println!("\n{json}");
    if let Some(out) = &args.out {
        std::fs::write(out, &json).with_context(|| format!("failed to write {out}"))?;
        println!("\nwrote {out}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn one_run(
    rpc_urls: &[String],
    registry: &DecodeRegistry,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
    seal_direct: bool,
    concurrency: usize,
    run: usize,
) -> Result<Run> {
    let source = RpcClient::new(rpc_urls.to_vec())?;
    // A throwaway work dir per run (redb and/or Parquet segments) — never the nest's own database.
    let work = std::env::temp_dir().join(format!("nuthatch-bench-{}-{run}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    let rss = RssSampler::start();
    let start = Instant::now();
    let events = if seal_direct && concurrency > 1 {
        // Pipelined seal-direct: concurrent fetch, in-order deterministic sealing.
        crate::indexer::backfill_direct_pipelined(
            &source,
            registry,
            &work,
            addresses,
            topic0s,
            from,
            to,
            window,
            concurrency,
            |_| Ok(()), // bench doesn't persist a resume watermark
            |_, _| {},  // bench doesn't render progress
        )
        .await?
    } else if seal_direct {
        // Seal-direct: decode → Parquet, bypassing the hot store. Exactly the production path.
        crate::indexer::backfill_direct(
            &source, registry, &work, addresses, topic0s, from, to, window,
        )
        .await?
    } else {
        hot_store_backfill(
            &source, registry, &work, addresses, topic0s, from, to, window,
        )
        .await?
    };
    let wall_clock_s = start.elapsed().as_secs_f64();
    let peak_rss_mb = rss.stop();
    let _ = std::fs::remove_dir_all(&work);

    Ok(Run {
        events,
        wall_clock_s,
        events_per_sec: if wall_clock_s > 0.0 {
            events as f64 / wall_clock_s
        } else {
            0.0
        },
        peak_rss_mb,
        rpc_requests: source.request_count(),
    })
}

/// The baseline path: decode → redb hot store, with the same batched `block_timestamp` fetch the
/// live `dev` loop does — so the only thing that differs from seal-direct is the storage write.
#[allow(clippy::too_many_arguments)]
async fn hot_store_backfill(
    source: &RpcClient,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
) -> Result<u64> {
    let store = Store::open(&dir.join("bench.redb"))?;
    let mut events = 0u64;
    let mut next = from;
    while next <= to {
        let chunk_to = (next + window - 1).min(to);
        let logs = source
            .logs(addresses, topic0s, next, chunk_to)
            .await
            .with_context(|| format!("getLogs {next}..={chunk_to}"))?;
        let mut rows: Vec<_> = logs
            .iter()
            .filter_map(|log| registry.decode(log).ok().flatten())
            .collect();
        let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let ts = source.block_timestamps(&blocks).await.unwrap_or_default();
        for r in &mut rows {
            r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
            let key = Store::entity_key(r.block_number, r.log_index);
            store.put_entity(&key, &r.to_json().to_string())?;
            events += 1;
        }
        next = chunk_to + 1;
    }
    Ok(events)
}

/// Samples this process's resident set size on a background thread, tracking the peak.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak_kb: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_kb = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak_kb.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if let Some(kb) = current_rss_kb() {
                    p.fetch_max(kb, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        });
        Self {
            stop,
            peak_kb,
            handle: Some(handle),
        }
    }

    /// Stop sampling and return the peak RSS in MB.
    fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak_kb.load(Ordering::Relaxed) / 1024
    }
}

/// This process's RSS in KB. Linux via `/proc/self/status`; else `ps` (macOS/BSD). None if neither.
fn current_rss_kb() -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest.trim().trim_end_matches("kB").trim().parse().ok();
            }
        }
    }
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn median_f64(vals: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = vals.collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    median_of_sorted(&v).unwrap_or(0.0)
}

fn median_u64(vals: impl Iterator<Item = u64>) -> u64 {
    let mut v: Vec<f64> = vals.map(|x| x as f64).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    median_of_sorted(&v).unwrap_or(0.0).round() as u64
}

fn median_of_sorted(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The short commit the bench ran at (provenance for the report), or None outside a git checkout.
fn git_commit() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median_u64([10u64, 30, 20].into_iter()), 20);
        assert_eq!(median_u64([10u64, 20, 30, 40].into_iter()), 25); // even → mean of middle two
        assert_eq!(median_u64(std::iter::empty()), 0);
        assert!((median_f64([1.0, 3.0, 2.0].into_iter()) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn round_two_dp() {
        assert_eq!(round2(1234.5678), 1234.57);
    }
}
