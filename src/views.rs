//! Incremental view maintenance (the IVM core) via DBSP. The first declarative view: per-address
//! token balances, derived from Transfer facts. This is the differentiator vs imperative indexers —
//! we never hand-write "on transfer, load balance, add, save". We state balance = Σ(in) − Σ(out) as
//! a circuit, and DBSP maintains it incrementally: a new transfer is a +1 delta, and a reorg is the
//! *same* transfer re-fed with weight −1 (a retraction). Backfill and tip use the identical circuit.
//!
//! Balances accumulate in **i128** base units. That matters: an i64 accumulator silently drops any
//! transfer above ~9.2e18 base units — barely ~9.2 tokens of an 18-decimal token — so large-supply
//! or high-decimal tokens would under-count. i128 (max ~1.7e38) comfortably holds any real token's
//! total supply in base units. Values that somehow exceed i128 are skipped by the caller.
//!
//! The view is in-memory but not ephemeral: `rebuild` reconstructs it from stored facts on a warm
//! restart (see `indexer::rebuild_balances`), so balances survive a process bounce.

use anyhow::{anyhow, Context, Result};
use dbsp::utils::Tup2;
use dbsp::{IndexedZSetReader, OrdZSet, OutputHandle, RootCircuit, Runtime};
use std::collections::HashMap;
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::{Arc, RwLock};

/// A balance delta fed to the circuit: (address, signed_value). Insert carries weight +1, a reorg
/// retraction carries weight −1.
type Delta = Tup2<String, i128>;
/// A batch of weighted deltas. Weight is the DBSP Z-set weight (i64): +1 insert, −1 retract.
pub type WeightedBatch = Vec<Tup2<Delta, i64>>;

/// The two deltas a single transfer contributes: +value to `to`, −value to `from`.
pub fn transfer_deltas(from: &str, to: &str, value: i128, weight: i64) -> WeightedBatch {
    vec![
        Tup2(Tup2(to.to_string(), value), weight),
        Tup2(Tup2(from.to_string(), -value), weight),
    ]
}

/// A single net-balance delta for one address, used to seed the view on restart from a pre-summed
/// cold aggregate (rather than replaying every underlying transfer).
pub fn seed_delta(address: String, net: i128) -> Tup2<Delta, i64> {
    Tup2(Tup2(address, net), 1)
}

/// The input and output handles of the balance circuit.
type CircuitHandles = (
    dbsp::ZSetHandle<Delta>,
    OutputHandle<OrdZSet<Tup2<String, i128>>>,
);

/// Owns the DBSP circuit. `step` applies a weighted batch and folds the resulting changes into
/// `balances`. Kept separate from the threading so it can be driven deterministically in tests.
struct BalanceCircuit {
    circuit: dbsp::DBSPHandle,
    input: dbsp::ZSetHandle<Delta>,
    output: OutputHandle<OrdZSet<Tup2<String, i128>>>,
}

impl BalanceCircuit {
    fn new() -> Result<Self> {
        let (circuit, (input, output)) = Runtime::init_circuit(1, build_circuit)
            .map_err(|e| anyhow!("failed to build IVM circuit: {e}"))?;
        Ok(Self {
            circuit,
            input,
            output,
        })
    }

    /// Feed a batch, advance the circuit one transaction, and apply the emitted changes.
    fn step(
        &mut self,
        mut batch: WeightedBatch,
        balances: &mut HashMap<String, i128>,
    ) -> Result<()> {
        self.input.append(&mut batch);
        self.circuit
            .transaction()
            .map_err(|e| anyhow!("IVM transaction: {e}"))?;

        // The aggregate emits a change stream: a key whose balance moves from old→new appears as
        // (key,old,−1) and (key,new,+1); a key falling to zero appears only as (key,old,−1).
        let changes = self.output.consolidate();
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
        for (k, v) in set.iter() {
            balances.insert(k.clone(), *v);
        }
        for k in cleared {
            if !set.contains_key(&k) {
                balances.remove(&k); // balance returned to zero → drop the key
            }
        }
        Ok(())
    }
}

fn build_circuit(circuit: &mut RootCircuit) -> Result<CircuitHandles, anyhow::Error> {
    let (stream, handle) = circuit.add_input_zset::<Delta>();
    // Sum the signed values per address. aggregate_linear is the incremental Σ; the Z-set weight
    // carries insert/retract, so a retraction subtracts automatically.
    let balances = stream
        .map_index(|d: &Delta| (d.0.clone(), d.1))
        .aggregate_linear(|v: &i128| *v);
    let out = balances.map(|(addr, bal): (&String, &i128)| Tup2(addr.clone(), *bal));
    Ok((handle, out.output()))
}

/// A message to the circuit thread: a batch to apply, or a flush barrier that acks once every
/// previously-enqueued batch has been folded in (used to make `rebuild` synchronous).
enum Msg {
    Batch(WeightedBatch),
    Flush(SyncSender<()>),
}

/// Cheap-to-clone handle to the live balance view. Sends batches to the circuit thread; reads the
/// maintained balances from shared state.
#[derive(Clone)]
pub struct BalanceView {
    tx: Sender<Msg>,
    balances: Arc<RwLock<HashMap<String, i128>>>,
}

impl BalanceView {
    /// Spawn the circuit on its own thread (DBSP drives worker threads; keep it off the async pool).
    pub fn start() -> Result<Self> {
        let (tx, rx) = channel::<Msg>();
        let balances = Arc::new(RwLock::new(HashMap::new()));
        let shared = balances.clone();
        std::thread::Builder::new()
            .name("nuthatch-ivm".into())
            .spawn(move || {
                let mut circuit = match BalanceCircuit::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("IVM circuit failed to start: {e:#}");
                        return;
                    }
                };
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Batch(batch) => {
                            let mut map = shared.write().unwrap();
                            if let Err(e) = circuit.step(batch, &mut map) {
                                tracing::error!("IVM step failed: {e:#}");
                                break;
                            }
                        }
                        // Messages are processed in order, so by the time we see the barrier every
                        // prior batch is applied — ack unblocks the waiter.
                        Msg::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .context("failed to spawn IVM thread")?;
        Ok(Self { tx, balances })
    }

    /// Enqueue a weighted batch (built via `transfer_deltas`/`seed_delta`). Non-blocking; drops
    /// silently if the circuit thread has died (already logged there).
    pub fn apply(&self, batch: WeightedBatch) {
        if !batch.is_empty() {
            let _ = self.tx.send(Msg::Batch(batch));
        }
    }

    /// Block until every batch enqueued so far has been folded into the view. Used after a restart
    /// rebuild so the API serves complete balances from the first request.
    pub fn flush(&self) {
        let (ack, wait) = sync_channel(0);
        if self.tx.send(Msg::Flush(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    pub fn balance(&self, address: &str) -> Option<i128> {
        self.balances.read().ok()?.get(address).copied()
    }

    /// Top `n` addresses by balance, descending.
    pub fn top(&self, n: usize) -> Vec<(String, i128)> {
        let map = match self.balances.read() {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        let mut v: Vec<(String, i128)> = map.iter().map(|(k, val)| (k.clone(), *val)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    pub fn holders(&self) -> usize {
        self.balances.read().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic IVM golden test: insert transfers, check derived balances; then retract one
    /// (a reorg) and check the view converges to the state as if that transfer never happened.
    #[test]
    fn balances_are_maintained_incrementally_with_retraction() {
        let mut circuit = BalanceCircuit::new().unwrap();
        let mut bal = HashMap::new();

        // alice→bob 100, then bob→carol 30.
        circuit
            .step(transfer_deltas("alice", "bob", 100, 1), &mut bal)
            .unwrap();
        circuit
            .step(transfer_deltas("bob", "carol", 30, 1), &mut bal)
            .unwrap();
        assert_eq!(bal.get("alice"), Some(&-100));
        assert_eq!(bal.get("bob"), Some(&70));
        assert_eq!(bal.get("carol"), Some(&30));

        // Reorg: retract bob→carol 30. carol returns to zero (key dropped); bob back to −100+...
        circuit
            .step(transfer_deltas("bob", "carol", 30, -1), &mut bal)
            .unwrap();
        assert_eq!(bal.get("alice"), Some(&-100));
        assert_eq!(bal.get("bob"), Some(&100));
        assert_eq!(bal.get("carol"), None, "zero balance must drop the key");
    }

    /// The P0 regression: a transfer larger than i64::MAX must be tracked, not silently dropped.
    /// ~1e20 base units is ~100 tokens of an 18-decimal token — utterly ordinary, yet overflows i64.
    #[test]
    fn balances_hold_values_beyond_i64() {
        let mut circuit = BalanceCircuit::new().unwrap();
        let mut bal = HashMap::new();
        let big: i128 = 100_000_000_000_000_000_000; // 1e20 > i64::MAX (~9.2e18)
        assert!(big > i64::MAX as i128);

        circuit
            .step(transfer_deltas("0xmint", "0xwhale", big, 1), &mut bal)
            .unwrap();
        assert_eq!(bal.get("0xwhale"), Some(&big), "must not overflow/drop");
        assert_eq!(bal.get("0xmint"), Some(&-big));

        // Two more of the same accumulate past 2×i64::MAX without wrapping.
        circuit
            .step(transfer_deltas("0xmint", "0xwhale", big, 1), &mut bal)
            .unwrap();
        assert_eq!(bal.get("0xwhale"), Some(&(2 * big)));
    }

    /// A flush barrier returns only once queued work is drained — the property `rebuild` relies on.
    #[test]
    fn flush_waits_for_enqueued_work() {
        let view = BalanceView::start().unwrap();
        view.apply(transfer_deltas("a", "b", 42, 1));
        view.flush();
        assert_eq!(view.balance("b"), Some(42));
        assert_eq!(view.balance("a"), Some(-42));
    }
}
