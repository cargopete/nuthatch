//! A single self-updating progress line for the one long wait on nuthatch's happy path: the
//! from-deployment backfill. It answers the three questions a user staring at a cold start actually
//! has — how far, how fast, how much longer — and ends on a crisp "caught up to tip".
//!
//! Two output modes, chosen by whether stderr is a terminal:
//! - **TTY:** one carriage-returned line, redrawn ~8×/s. No scrollback spam.
//! - **piped / systemd:** a throttled `tracing::info!` every ~15 s, so a journal gets a sane
//!   heartbeat instead of either silence or a flood — and never a stray carriage return in a file.
//!
//! The reporter is pure presentation: it holds no lock the indexer needs and never touches stored
//! state, so it can't perturb the deterministic core (non-negotiable 4). Timing uses `Instant`, so
//! it's monotonic and unaffected by wall-clock jumps.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Live feedback for a bounded backfill over `[from, to]` blocks. `tick` as windows complete;
/// `finish` when the range is done.
pub struct Backfill {
    from: u64,
    to: u64,
    label: &'static str,
    start: Instant,
    last_render: Instant,
    events: u64,
    tty: bool,
    /// True once we've drawn at least one TTY line (so `finish` knows to clear it).
    drew_line: bool,
}

/// Redraw cadence on a TTY — fast enough to feel live, slow enough not to spend the backfill
/// drawing.
const TTY_INTERVAL: Duration = Duration::from_millis(120);
/// Log cadence when not a TTY — a heartbeat, not a flood.
const LOG_INTERVAL: Duration = Duration::from_secs(15);

impl Backfill {
    /// `label` is the verb shown on the line ("backfilling", "sealing history"). `from`/`to` are the
    /// block range this reporter covers; `to` is the tip (or finalized ceiling) at the moment it started.
    pub fn new(label: &'static str, from: u64, to: u64) -> Self {
        let now = Instant::now();
        Self {
            from,
            to,
            label,
            start: now,
            // Back-date the first render so the opening tick draws immediately (feels responsive).
            last_render: now - TTY_INTERVAL,
            events: 0,
            tty: std::io::stderr().is_terminal(),
            drew_line: false,
        }
    }

    /// Record progress: the highest block now processed, and how many events that step added.
    /// Cheap and throttled internally, so it's safe to call on every window.
    pub fn tick(&mut self, block_reached: u64, new_events: u64) {
        self.events += new_events;
        let interval = if self.tty { TTY_INTERVAL } else { LOG_INTERVAL };
        if self.last_render.elapsed() < interval {
            return;
        }
        self.last_render = Instant::now();
        if self.tty {
            self.render_tty(block_reached);
        } else {
            tracing::info!(
                "{} {:.1}% — block {} of {}, {} events, {} ev/s",
                self.label,
                self.pct(block_reached),
                block_reached,
                self.to,
                self.events,
                self.events_per_sec() as u64,
            );
        }
    }

    /// The range is complete. Clears any TTY line and logs a one-line summary. `caught_up` picks the
    /// wording: `true` for the final tip-catch-up ("caught up to tip…"), `false` for an intermediate
    /// phase like the seal-direct bulk ("sealed history…").
    pub fn finish(mut self, block_reached: u64, caught_up: bool) {
        self.clear_line();
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let eps = (self.events as f64 / secs) as u64;
        if caught_up {
            tracing::info!(
                "✓ caught up to tip at block {block_reached} — {} events in {}, {eps} ev/s; now following",
                self.events,
                fmt_dur(secs),
            );
        } else {
            tracing::info!(
                "✓ {} done — {} events over blocks {}..={block_reached} in {}, {eps} ev/s",
                self.label,
                self.events,
                self.from,
                fmt_dur(secs),
            );
        }
    }

    fn render_tty(&mut self, block_reached: u64) {
        let eta = self.eta(block_reached);
        let mut err = std::io::stderr().lock();
        // `\r` to column 0, `\x1b[2K` clears the whole line so a shorter redraw leaves no tail.
        let _ = write!(
            err,
            "\r\x1b[2K  {} {:5.1}% │ block {} │ {} events │ {} ev/s │ ETA {}",
            self.label,
            self.pct(block_reached),
            block_reached,
            self.events,
            self.events_per_sec() as u64,
            eta,
        );
        let _ = err.flush();
        self.drew_line = true;
    }

    fn clear_line(&mut self) {
        if self.tty && self.drew_line {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            self.drew_line = false;
        }
    }

    fn pct(&self, block_reached: u64) -> f64 {
        let span = self.to.saturating_sub(self.from).max(1);
        let done = block_reached.saturating_sub(self.from).min(span);
        (done as f64 / span as f64) * 100.0
    }

    fn events_per_sec(&self) -> f64 {
        self.events as f64 / self.start.elapsed().as_secs_f64().max(0.001)
    }

    /// ETA from *block* rate, not event rate — it stays meaningful even when events are sparse (a
    /// wide-window backfill over millions of near-empty blocks still shows honest time-remaining).
    fn eta(&self, block_reached: u64) -> String {
        let span = self.to.saturating_sub(self.from).max(1);
        let done = block_reached.saturating_sub(self.from).min(span);
        let secs = self.start.elapsed().as_secs_f64().max(0.001);
        let blocks_per_s = done as f64 / secs;
        if blocks_per_s <= 0.0 {
            return "—".into();
        }
        fmt_dur(span.saturating_sub(done) as f64 / blocks_per_s)
    }
}

/// Compact human duration: `42s`, `3m12s`, `1h04m`.
fn fmt_dur(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_dur_scales_by_magnitude() {
        assert_eq!(fmt_dur(0.4), "0s");
        assert_eq!(fmt_dur(42.0), "42s");
        assert_eq!(fmt_dur(192.0), "3m12s");
        assert_eq!(fmt_dur(3840.0), "1h04m");
    }

    #[test]
    fn pct_is_bounded_and_proportional() {
        let b = Backfill::new("backfilling", 100, 1100);
        assert_eq!(b.pct(100).round() as u64, 0);
        assert_eq!(b.pct(600).round() as u64, 50);
        assert_eq!(b.pct(1100).round() as u64, 100);
        // Never exceeds 100% even if a caller reports past `to`.
        assert_eq!(b.pct(9999).round() as u64, 100);
        // Never underflows below `from`.
        assert_eq!(b.pct(0).round() as u64, 0);
    }

    #[test]
    fn eta_is_dash_before_any_progress() {
        let b = Backfill::new("backfilling", 0, 1000);
        // No blocks done yet → no rate → no estimate.
        assert_eq!(b.eta(0), "—");
    }

    #[test]
    fn tick_accumulates_events() {
        let mut b = Backfill::new("backfilling", 0, 1000);
        b.tick(100, 5);
        b.tick(200, 7);
        assert_eq!(b.events, 12);
    }
}
