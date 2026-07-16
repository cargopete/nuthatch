//! Prometheus `/metrics` — the operator's alerting and billing surface (RFC-0005 §6).
//!
//! Hand-rolled: a handful of process-global atomics plus text formatting, not a metrics framework —
//! the single-binary / minimal-deps rule stands, and the metric set is small and fixed. Gauges
//! (tip, watermark, RSS) are set to the latest value; counters (rows, queries) only ever increase.
//! Nothing here phones home: metrics are exposed on the same local API and scraped by the operator.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The one process-wide metrics registry. `const`-constructed, so it needs no lazy init.
pub static METRICS: Metrics = Metrics::new();

pub struct Metrics {
    // Ingestion — the "is it keeping up?" signals an operator alerts on.
    tip_height: AtomicU64,
    last_block: AtomicU64,
    sealed_through: AtomicU64,
    rows_decoded: AtomicU64,
    rows_sealed: AtomicU64,
    reorgs: AtomicU64,
    // Serving — the surface an operator bills against.
    http_requests: AtomicU64,
    sql_queries: AtomicU64,
    sql_rejections: AtomicU64,
    rpc_requests: AtomicU64,
}

impl Metrics {
    const fn new() -> Self {
        Self {
            tip_height: AtomicU64::new(0),
            last_block: AtomicU64::new(0),
            sealed_through: AtomicU64::new(0),
            rows_decoded: AtomicU64::new(0),
            rows_sealed: AtomicU64::new(0),
            reorgs: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            sql_queries: AtomicU64::new(0),
            sql_rejections: AtomicU64::new(0),
            rpc_requests: AtomicU64::new(0),
        }
    }

    pub fn set_tip(&self, v: u64) {
        self.tip_height.store(v, Relaxed);
    }
    pub fn set_last_block(&self, v: u64) {
        self.last_block.store(v, Relaxed);
    }
    pub fn set_sealed_through(&self, v: u64) {
        self.sealed_through.store(v, Relaxed);
    }
    pub fn add_rows_decoded(&self, n: u64) {
        self.rows_decoded.fetch_add(n, Relaxed);
    }
    pub fn add_rows_sealed(&self, n: u64) {
        self.rows_sealed.fetch_add(n, Relaxed);
    }
    pub fn inc_reorgs(&self) {
        self.reorgs.fetch_add(1, Relaxed);
    }
    pub fn inc_http(&self) {
        self.http_requests.fetch_add(1, Relaxed);
    }
    pub fn inc_sql(&self) {
        self.sql_queries.fetch_add(1, Relaxed);
    }
    pub fn inc_sql_rejected(&self) {
        self.sql_rejections.fetch_add(1, Relaxed);
    }
    pub fn inc_rpc(&self) {
        self.rpc_requests.fetch_add(1, Relaxed);
    }

    /// Render the registry as Prometheus text (`text/plain; version=0.0.4`).
    pub fn render(&self) -> String {
        let tip = self.tip_height.load(Relaxed);
        let last = self.last_block.load(Relaxed);
        // Blocks behind the tip. 0 (not negative) once caught up.
        let lag = tip.saturating_sub(last);
        let rss = rss_bytes();

        let gauge = |name: &str, help: &str, v: u64| {
            format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n")
        };
        let counter = |name: &str, help: &str, v: u64| {
            format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n")
        };

        let mut s = String::with_capacity(2048);
        s.push_str(&gauge(
            "nuthatch_tip_height",
            "Latest block height seen from the source.",
            tip,
        ));
        s.push_str(&gauge(
            "nuthatch_last_block",
            "Highest block the indexer has committed.",
            last,
        ));
        s.push_str(&gauge(
            "nuthatch_tip_lag_blocks",
            "Blocks the indexer is behind the source tip.",
            lag,
        ));
        s.push_str(&gauge(
            "nuthatch_sealed_through",
            "Highest block sealed to the immutable cold layer.",
            self.sealed_through.load(Relaxed),
        ));
        s.push_str(&gauge(
            "nuthatch_rss_bytes",
            "Resident set size of this process, in bytes.",
            rss,
        ));
        s.push_str(&counter(
            "nuthatch_rows_decoded_total",
            "Rows decoded since start.",
            self.rows_decoded.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_rows_sealed_total",
            "Rows sealed to Parquet since start.",
            self.rows_sealed.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_reorgs_total",
            "Reorgs detected and rolled back since start.",
            self.reorgs.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_http_requests_total",
            "HTTP API requests served since start.",
            self.http_requests.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_sql_queries_total",
            "Analytical /sql queries accepted since start.",
            self.sql_queries.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_sql_rejections_total",
            "Analytical /sql queries rejected (guard: timeout, too-large, over-capacity).",
            self.sql_rejections.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_rpc_requests_total",
            "Outbound JSON-RPC requests issued (incl. failover retries).",
            self.rpc_requests.load(Relaxed),
        ));
        s
    }
}

/// This process's resident set size in bytes. Linux via `/proc/self/status`; else `ps` (macOS/BSD);
/// 0 if neither answers (the gauge just reads 0 rather than failing the scrape).
pub fn rss_bytes() -> u64 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Ok(kb) = rest.trim().trim_end_matches("kB").trim().parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    let pid = std::process::id().to_string();
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
    {
        if let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            return kb * 1024;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_prometheus_text_with_lag() {
        let m = Metrics::new();
        m.set_tip(1_000);
        m.set_last_block(940);
        m.add_rows_decoded(5);
        m.inc_sql();
        m.inc_sql_rejected();
        let out = m.render();
        assert!(out.contains("# TYPE nuthatch_tip_height gauge"));
        assert!(out.contains("nuthatch_tip_height 1000"));
        assert!(out.contains("nuthatch_tip_lag_blocks 60")); // 1000 - 940
        assert!(out.contains("# TYPE nuthatch_rows_decoded_total counter"));
        assert!(out.contains("nuthatch_rows_decoded_total 5"));
        assert!(out.contains("nuthatch_sql_queries_total 1"));
        assert!(out.contains("nuthatch_sql_rejections_total 1"));
    }

    #[test]
    fn lag_never_goes_negative() {
        let m = Metrics::new();
        m.set_tip(100);
        m.set_last_block(120); // last ahead of a stale tip read
        assert!(m.render().contains("nuthatch_tip_lag_blocks 0"));
    }
}
