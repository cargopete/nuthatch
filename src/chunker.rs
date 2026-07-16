//! Adaptive `eth_getLogs` range sizing (RFC-0004 §2).
//!
//! Providers cap `getLogs` by *result count* (and differ wildly: 10k here, "response size" there),
//! and a good block-window for dense USDC is a terrible one for sparse Horizon. Instead of a
//! per-chain constant that has to be hand-tuned per provider, one controller targets a response
//! budget (~2,000 logs) and adjusts the window multiplicatively from feedback: an overshoot or a
//! "result too large" error shrinks it, an undershoot grows it. The same code handles both density
//! extremes and self-heals into any provider's cap.

/// Target logs per `getLogs` response. Providers cap on result count, so this is a log (not decoded
/// event) budget — comfortably under the common 10k caps, with headroom for a growth step.
pub const TARGET_LOGS_PER_RESPONSE: u64 = 2_000;
/// Never go below one block (a single very dense block can still exceed the target — that's fine, we
/// can't split a block) or above this ceiling (keeps sparse-chain growth sane).
pub const MIN_WINDOW: u64 = 1;
pub const MAX_WINDOW: u64 = 100_000;

/// A block-window controller that converges on `target` logs per response.
#[derive(Debug, Clone)]
pub struct AdaptiveWindow {
    window: u64,
    target: u64,
    min: u64,
    max: u64,
}

impl AdaptiveWindow {
    /// Start from `initial` (typically the chain's default window), targeting `target` logs/response.
    pub fn new(initial: u64, target: u64, min: u64, max: u64) -> Self {
        Self {
            window: initial.clamp(min.max(1), max),
            target: target.max(1),
            min: min.max(1),
            max,
        }
    }

    /// A sensible controller for a chain whose default window is `initial`.
    pub fn for_window(initial: u64) -> Self {
        Self::new(initial, TARGET_LOGS_PER_RESPONSE, MIN_WINDOW, MAX_WINDOW)
    }

    /// The block span to request next.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// Feed back how many logs the last response held; adjust toward the target. Change is damped to
    /// at most 4× per step so a single sparse (0-log) or spiky window doesn't swing the window wildly.
    pub fn observed(&mut self, logs: u64) {
        let next = if logs == 0 {
            // Nothing came back — grow to cover more ground, but only 4× at a time.
            self.window.saturating_mul(4)
        } else {
            // Proportional to hit the target, clamped to a 4× move either direction.
            let scaled = (self.window as u128 * self.target as u128 / logs as u128) as u64;
            scaled.clamp((self.window / 4).max(1), self.window.saturating_mul(4))
        };
        self.window = next.clamp(self.min, self.max);
    }

    /// The provider rejected the range as too large — halve hard and (the caller) retry the range.
    pub fn too_large(&mut self) {
        self.window = (self.window / 2).max(self.min);
    }
}

/// Whether an RPC error looks like a result-size / range cap (so the caller shrinks and retries the
/// same range) rather than a transient failure. Matches the phrasings the major providers use.
pub fn is_result_too_large(err: &anyhow::Error) -> bool {
    let s = format!("{err:#}").to_ascii_lowercase();
    const CAP_MARKERS: &[&str] = &[
        "response size",            // Alchemy: "Log response size exceeded"
        "too many results",         // generic
        "query returned more than", // Infura: "query returned more than 10000 results"
        "more than 10000",
        "result set too large",
        "range is too", // "block range is too wide/large"
        "range too large",
        "too large", // catch-all for "* too large"
        "limit exceeded",
    ];
    CAP_MARKERS.iter().any(|m| s.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn shrinks_on_overshoot_grows_on_undershoot() {
        let mut w = AdaptiveWindow::new(100, 2_000, 1, 100_000);
        // Way over target → shrink (proportional, damped to 4× min).
        w.observed(8_000); // 100 * 2000/8000 = 25
        assert_eq!(w.window(), 25);
        // Under target → grow toward it.
        let before = w.window();
        w.observed(500); // 25 * 2000/500 = 100
        assert!(w.window() > before);
        assert_eq!(w.window(), 100);
    }

    #[test]
    fn zero_logs_grows_capped_at_4x() {
        let mut w = AdaptiveWindow::new(1_000, 2_000, 1, 100_000);
        w.observed(0);
        assert_eq!(w.window(), 4_000); // 4× growth, no division by zero
    }

    #[test]
    fn respects_bounds_and_damping() {
        let mut w = AdaptiveWindow::new(1_000, 2_000, 10, 5_000);
        w.observed(1_000_000); // huge overshoot → clamp to 4× shrink then min bound
        assert_eq!(w.window(), 250); // 1000/4, above min 10
        let mut w2 = AdaptiveWindow::new(4_000, 2_000, 1, 5_000);
        w2.observed(0); // 4× = 16000 but capped at max 5000
        assert_eq!(w2.window(), 5_000);
    }

    #[test]
    fn too_large_halves() {
        let mut w = AdaptiveWindow::new(2_000, 2_000, 1, 100_000);
        w.too_large();
        assert_eq!(w.window(), 1_000);
    }

    #[test]
    fn detects_provider_cap_errors() {
        assert!(is_result_too_large(&anyhow!(
            "Log response size exceeded. Try a smaller range"
        )));
        assert!(is_result_too_large(&anyhow!(
            "query returned more than 10000 results"
        )));
        assert!(is_result_too_large(&anyhow!("block range is too wide")));
        assert!(!is_result_too_large(&anyhow!("connection reset by peer")));
        assert!(!is_result_too_large(&anyhow!("HTTP status 521")));
    }
}
