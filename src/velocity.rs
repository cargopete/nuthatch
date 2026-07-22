//! Velocity flags (RFC-0008 C3): per-address outbound volume within a block-window, maintained
//! incrementally by DBSP - the same IVM machinery as balances/exposure, applied to "how much did this
//! address move recently?". A **velocity flag** is an (address, window) whose outbound volume reaches
//! a configured threshold.
//!
//! The window is a **tumbling block-bucket**, not a true sliding window: a transfer at block `b` falls
//! in the bucket starting at `(b / W) * W`. This is an honest approximation of "~24h volume" (blocks,
//! not wall-clock - `W ≈ 7200` on 12s-block mainnet) that stays exactly maintainable incrementally: a
//! reorg re-feeds the transfer with weight −1 and its bucket's volume retracts, exactly like a balance
//! (so a flag that a reorg invalidates disappears). A true sliding window would need per-block aging;
//! the tumbling approximation is documented rather than faked.
//!
//! Two linear DBSP aggregates per (address, window) - summed volume and a transfer count - because a
//! tuple aggregate isn't linear, and splitting them keeps both incrementally maintained *and* seedable
//! on restart from a pre-summed cold aggregate (see `rebuild_velocity`).

use anyhow::{anyhow, Context, Result};
use dbsp::utils::Tup2;
use dbsp::{IndexedZSetReader, OrdZSet, OutputHandle, RootCircuit, Runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::{Arc, RwLock};

/// Field separator inside an encoded velocity key (ASCII Unit Separator).
const SEP: char = '\u{1f}';

/// The window-start block for `block` under window size `w`: the tumbling bucket boundary.
pub fn window_start(block: u64, window: u64) -> u64 {
    (block / window.max(1)) * window.max(1)
}

fn key(address: &str, window_start: u64) -> String {
    format!("{address}{SEP}{window_start}")
}

/// One contribution to the velocity view: for `key`, `volume` moved across `count` transfers, applied
/// with `weight` (+1 insert, −1 retraction). Seeds carry a pre-summed `count`.
#[derive(Debug, Clone)]
pub struct VelItem {
    key: String,
    volume: i128,
    count: i128,
    weight: i64,
}

pub type VelocityBatch = Vec<VelItem>;

/// The velocity contribution of one transfer: the **sender** moves `value` in `block`'s window. (Only
/// outbound volume - the AML-relevant "how much did X push out"; empty on a zero/over-i128 value.)
pub fn velocity_deltas(
    from: &str,
    block: u64,
    value: i128,
    weight: i64,
    window: u64,
) -> VelocityBatch {
    vec![VelItem {
        key: key(from, window_start(block, window)),
        volume: value,
        count: 1,
        weight,
    }]
}

/// Seed a pre-summed velocity aggregate on restart (from a cold DuckDB fold): `count` transfers
/// totalling `volume` for `key`.
pub fn seed_item(key: String, volume: i128, count: i128) -> VelItem {
    VelItem {
        key,
        volume,
        count,
        weight: 1,
    }
}

/// One velocity flag: an address whose windowed outbound volume reached the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VelocityFlag {
    pub address: String,
    pub window_start: u64,
    pub count: i128,
    pub volume: i128,
}

type KeyAmt = Tup2<String, i128>;
type CircuitHandles = (
    (dbsp::ZSetHandle<KeyAmt>, dbsp::ZSetHandle<KeyAmt>),
    (
        OutputHandle<OrdZSet<Tup2<String, i128>>>,
        OutputHandle<OrdZSet<Tup2<String, i128>>>,
    ),
);

struct VelocityCircuit {
    circuit: dbsp::DBSPHandle,
    volume_in: dbsp::ZSetHandle<KeyAmt>,
    count_in: dbsp::ZSetHandle<KeyAmt>,
    volume_out: OutputHandle<OrdZSet<Tup2<String, i128>>>,
    count_out: OutputHandle<OrdZSet<Tup2<String, i128>>>,
    volumes: HashMap<String, i128>,
    counts: HashMap<String, i128>,
}

impl VelocityCircuit {
    fn new() -> Result<Self> {
        let (circuit, ((volume_in, count_in), (volume_out, count_out))) =
            Runtime::init_circuit(1, build_circuit)
                .map_err(|e| anyhow!("failed to build velocity circuit: {e}"))?;
        Ok(Self {
            circuit,
            volume_in,
            count_in,
            volume_out,
            count_out,
            volumes: HashMap::new(),
            counts: HashMap::new(),
        })
    }

    fn step(&mut self, batch: VelocityBatch) -> Result<()> {
        let mut vol: Vec<Tup2<KeyAmt, i64>> = Vec::with_capacity(batch.len());
        let mut cnt: Vec<Tup2<KeyAmt, i64>> = Vec::with_capacity(batch.len());
        for it in batch {
            vol.push(Tup2(Tup2(it.key.clone(), it.volume), it.weight));
            cnt.push(Tup2(Tup2(it.key, it.count), it.weight));
        }
        self.volume_in.append(&mut vol);
        self.count_in.append(&mut cnt);
        self.circuit
            .transaction()
            .map_err(|e| anyhow!("velocity transaction: {e}"))?;

        fold_changes(&self.volume_out.consolidate(), &mut self.volumes, false);
        let dropped = fold_changes(&self.count_out.consolidate(), &mut self.counts, true);
        for k in dropped {
            self.volumes.remove(&k);
        }
        Ok(())
    }

    #[cfg(test)]
    fn flags(&self, threshold: i128) -> Vec<VelocityFlag> {
        rows_over(&self.counts, &self.volumes, threshold)
    }
}

/// Collect (address, window) entries whose volume ≥ `threshold`, descending by volume.
fn rows_over(
    counts: &HashMap<String, i128>,
    volumes: &HashMap<String, i128>,
    threshold: i128,
) -> Vec<VelocityFlag> {
    let mut out: Vec<VelocityFlag> = counts
        .iter()
        .filter_map(|(k, &count)| {
            let volume = volumes.get(k).copied().unwrap_or(0);
            if volume < threshold {
                return None;
            }
            let (addr, ws) = k.split_once(SEP)?;
            Some(VelocityFlag {
                address: addr.to_string(),
                window_start: ws.parse().ok()?,
                count,
                volume,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.volume
            .cmp(&a.volume)
            .then_with(|| a.address.cmp(&b.address))
            .then_with(|| a.window_start.cmp(&b.window_start))
    });
    out
}

fn fold_changes(
    changes: &OrdZSet<Tup2<String, i128>>,
    map: &mut HashMap<String, i128>,
    drop_on_clear: bool,
) -> Vec<String> {
    let mut set: HashMap<String, i128> = HashMap::new();
    let mut cleared: Vec<String> = Vec::new();
    changes
        .iter()
        .for_each(|(rec, (), weight): (Tup2<String, i128>, (), i64)| {
            if weight > 0 {
                set.insert(rec.0.clone(), rec.1);
            } else if weight < 0 {
                cleared.push(rec.0);
            }
        });
    for (k, v) in &set {
        map.insert(k.clone(), *v);
    }
    let mut dropped = Vec::new();
    for k in cleared {
        if !set.contains_key(&k) {
            if drop_on_clear {
                map.remove(&k);
                dropped.push(k);
            } else {
                map.insert(k, 0);
            }
        }
    }
    dropped
}

fn build_circuit(circuit: &mut RootCircuit) -> Result<CircuitHandles, anyhow::Error> {
    let (volume_stream, volume_h) = circuit.add_input_zset::<KeyAmt>();
    let volume = volume_stream
        .map_index(|d: &KeyAmt| (d.0.clone(), d.1))
        .aggregate_linear(|v: &i128| *v);
    let volume_out = volume
        .map(|(k, v): (&String, &i128)| Tup2(k.clone(), *v))
        .output();

    let (count_stream, count_h) = circuit.add_input_zset::<KeyAmt>();
    let count = count_stream
        .map_index(|d: &KeyAmt| (d.0.clone(), d.1))
        .aggregate_linear(|v: &i128| *v);
    let count_out = count
        .map(|(k, v): (&String, &i128)| Tup2(k.clone(), *v))
        .output();

    Ok(((volume_h, count_h), (volume_out, count_out)))
}

enum Msg {
    Batch(VelocityBatch),
    Flush(SyncSender<()>),
}

/// Cheap-to-clone handle to the live velocity view. Mirrors [`crate::exposure::ExposureView`].
#[derive(Clone)]
pub struct VelocityView {
    tx: Sender<Msg>,
    rows: Arc<RwLock<HashMap<String, (i128, i128)>>>,
    /// Flips false if the circuit thread dies on an error, so the ingest loop can fail loudly rather
    /// than serve frozen (and, for compliance, silently wrong) velocity flags. A disabled view stays healthy.
    healthy: Arc<AtomicBool>,
}

impl VelocityView {
    /// Start the velocity view. When `enabled` is false (no velocity flag configured) the DBSP circuit
    /// and its worker thread are not spawned - `apply` no-ops and `snapshot` stays empty (L10).
    pub fn start(enabled: bool) -> Result<Self> {
        let (tx, rx) = channel::<Msg>();
        let rows = Arc::new(RwLock::new(HashMap::new()));
        let healthy = Arc::new(AtomicBool::new(true));
        if !enabled {
            drop(rx);
            return Ok(Self { tx, rows, healthy });
        }
        let shared = rows.clone();
        let health = healthy.clone();
        std::thread::Builder::new()
            .name("nuthatch-velocity".into())
            .spawn(move || {
                let mut circuit = match VelocityCircuit::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("velocity circuit failed to start: {e:#}");
                        health.store(false, Ordering::SeqCst);
                        return;
                    }
                };
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Batch(batch) => {
                            if let Err(e) = circuit.step(batch) {
                                tracing::error!("velocity step failed: {e:#}");
                                health.store(false, Ordering::SeqCst);
                                break;
                            }
                            let mut out = shared.write().unwrap();
                            out.clear();
                            for (k, &c) in &circuit.counts {
                                out.insert(
                                    k.clone(),
                                    (c, circuit.volumes.get(k).copied().unwrap_or(0)),
                                );
                            }
                        }
                        Msg::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .context("failed to spawn velocity thread")?;
        Ok(Self { tx, rows, healthy })
    }

    /// Whether the circuit thread is alive; `false` means it died on an error and the view is frozen.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    pub fn apply(&self, batch: VelocityBatch) {
        if !batch.is_empty() {
            let _ = self.tx.send(Msg::Batch(batch));
        }
    }

    pub fn flush(&self) {
        let (ack, wait) = sync_channel(0);
        if self.tx.send(Msg::Flush(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// The current velocity flags at or above `threshold`, descending by volume.
    pub fn flags(&self, threshold: i128) -> Vec<VelocityFlag> {
        let map = match self.rows.read() {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let counts: HashMap<String, i128> = map.iter().map(|(k, &(c, _))| (k.clone(), c)).collect();
        let volumes: HashMap<String, i128> =
            map.iter().map(|(k, &(_, v))| (k.clone(), v)).collect();
        rows_over(&counts, &volumes, threshold)
    }

    /// Number of (address, window) buckets tracked - a small gauge for `/` and `/metrics`.
    pub fn entries(&self) -> usize {
        self.rows.read().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden test (C3 gate): windowed volume is maintained incrementally, flags fire at the
    /// threshold, and a reorg (retraction) drops a flag that its transfer no longer supports.
    #[test]
    fn velocity_flags_maintained_with_retraction() {
        let w = 100u64;
        let mut c = VelocityCircuit::new().unwrap();
        // alice sends 40 then 70 in the same window (blocks 10, 20 → window 0): volume 110.
        c.step(velocity_deltas("alice", 10, 40, 1, w)).unwrap();
        c.step(velocity_deltas("alice", 20, 70, 1, w)).unwrap();
        // A transfer in the next window (block 150 → window 100): separate bucket.
        c.step(velocity_deltas("alice", 150, 500, 1, w)).unwrap();

        // Threshold 100: window 0 (vol 110) and window 100 (vol 500) both flag.
        let flags = c.flags(100);
        assert_eq!(flags.len(), 2);
        assert_eq!(
            flags[0],
            VelocityFlag {
                address: "alice".into(),
                window_start: 100,
                count: 1,
                volume: 500
            }
        );
        assert_eq!(
            flags[1],
            VelocityFlag {
                address: "alice".into(),
                window_start: 0,
                count: 2,
                volume: 110
            }
        );

        // Threshold 200: only the second window survives.
        assert_eq!(c.flags(200).len(), 1);

        // Reorg: retract the 70 in window 0 → volume 40 < 100, flag clears.
        c.step(velocity_deltas("alice", 20, 70, -1, w)).unwrap();
        let flags = c.flags(100);
        assert_eq!(flags.len(), 1, "window-0 flag cleared after retraction");
        assert_eq!(flags[0].window_start, 100);
    }

    /// The C3 gate item: velocity arithmetic runs on the shipped i128 path - a windowed volume that
    /// overflows i64 is tracked, not truncated (a threshold view on i64 would be a compliance bug).
    #[test]
    fn velocity_volume_exceeds_i64() {
        let mut c = VelocityCircuit::new().unwrap();
        let big: i128 = 10_000_000_000_000_000_000; // > i64::MAX
        c.step(velocity_deltas("whale", 5, big, 1, 100)).unwrap();
        c.step(velocity_deltas("whale", 6, big, 1, 100)).unwrap();
        let flags = c.flags(big); // 2×big ≥ big
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].volume, 2 * big);
    }

    /// Seeding a pre-summed cold aggregate reproduces the same flags as replaying transfers.
    #[test]
    fn seeding_matches_replay() {
        let w = 100u64;
        let mut replay = VelocityCircuit::new().unwrap();
        for v in [10i128, 20, 70] {
            replay.step(velocity_deltas("a", 12, v, 1, w)).unwrap();
        }
        let mut seed = VelocityCircuit::new().unwrap();
        seed.step(vec![seed_item(key("a", window_start(12, w)), 100, 3)])
            .unwrap();
        assert_eq!(replay.flags(1), seed.flags(1));
    }

    #[test]
    fn view_thread_serves_after_flush() {
        let view = VelocityView::start(true).unwrap();
        view.apply(velocity_deltas("a", 10, 4242, 1, 100));
        view.flush();
        assert_eq!(view.flags(1000)[0].volume, 4242);
        assert_eq!(view.entries(), 1);
    }
}
