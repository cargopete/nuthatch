//! Tiny chain registry. Ships sensible public-RPC defaults with round-robin failover — the
//! first-run killer is RPC friction, so out of the box you should not need to bring a key.
//! (The "no third-party" upgrade is to colocate with a reth node; that path comes later.)
//!
//! The registry also carries each chain's finality policy and `eth_getLogs` window, so an L2 like
//! Arbitrum — different finality semantics, denser blocks — is a data entry here, not a fork of the
//! indexing loop.

/// How a chain decides a block is final enough to seal to the immutable cold layer. The sealing
/// invariant is unchanged either way: the columnar layer never sees a reorg, so this only sets *how
/// far behind the tip* we wait before a block is beyond reorg risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// Seal blocks at least `n` behind the tip. A conservative proxy for finality (Ethereum L1).
    Depth(u64),
    /// Prefer the node's L1-aware `finalized` block tag (correct by construction on an L2 like
    /// Arbitrum); fall back to `Depth(fallback_depth)` when the endpoint doesn't serve the tag.
    FinalizedTag { fallback_depth: u64 },
}

pub struct Chain {
    pub name: &'static str,
    pub chain_id: u64,
    /// Tried in order, then round-robin, so a single flaky endpoint doesn't stall a run.
    pub rpc_urls: &'static [&'static str],
    /// When a block is safe to seal (see `Finality`).
    pub finality: Finality,
    /// Block span per `eth_getLogs` call. Small on dense L1 (dodge result-size caps); large on a
    /// sparse L2 like Arbitrum where events are few but block heights climb fast.
    pub log_window: u64,
}

const MAINNET: Chain = Chain {
    name: "mainnet",
    chain_id: 1,
    rpc_urls: &[
        // Verified to serve keyless eth_getLogs (2026-07). Round-robin across them.
        "https://ethereum-rpc.publicnode.com",
        "https://eth.drpc.org",
        "https://eth-pokt.nodies.app",
        "https://eth.llamarpc.com",
    ],
    // ~2 epochs; real finality signals arrive with the ExEx mode. The `finalized` tag exists
    // post-merge but Depth keeps a single conservative policy until ExEx lands.
    finality: Finality::Depth(64),
    log_window: 20,
};

const ARBITRUM_ONE: Chain = Chain {
    name: "arbitrum-one",
    chain_id: 42161,
    rpc_urls: &[
        // Keyless Arbitrum One endpoints (2026-07). The official sequencer RPC first.
        "https://arb1.arbitrum.io/rpc",
        "https://arbitrum-one-rpc.publicnode.com",
        "https://arbitrum.drpc.org",
        "https://arb-pokt.nodies.app",
    ],
    // True finality is L1 confirmation of the batch (~10–20 min). Prefer the node's `finalized`
    // tag; else ~7.5 min at 250 ms blocks. Horizon is sparse, so the extra hot window is cheap.
    finality: Finality::FinalizedTag {
        fallback_depth: 1800,
    },
    // Arbitrum blocks are frequent but Horizon events are rare; a wide window keeps up cheaply.
    log_window: 2000,
};

const BASE: Chain = Chain {
    name: "base",
    chain_id: 8453,
    rpc_urls: &[
        // Keyless Base mainnet endpoints (2026-07). The official RPC first.
        "https://mainnet.base.org",
        "https://base-rpc.publicnode.com",
        "https://base.drpc.org",
        "https://base-pokt.nodies.app",
    ],
    // OP-stack L2: true finality is L1 confirmation. Base exposes the L1-aware `finalized` tag, so
    // prefer it (same policy as Arbitrum); the fallback (~30 min at 2 s blocks) only bites if an
    // endpoint doesn't serve the tag.
    finality: Finality::FinalizedTag {
        fallback_depth: 900,
    },
    // ~2 s blocks and busy — a moderate window that the adaptive chunker (RFC-0004 §2) tunes further.
    log_window: 1000,
};

pub fn lookup(name: &str) -> Option<&'static Chain> {
    match name {
        "mainnet" | "ethereum" | "eth" => Some(&MAINNET),
        "arbitrum-one" | "arbitrum" | "arb" | "arb1" => Some(&ARBITRUM_ONE),
        "base" | "base-mainnet" | "base-one" => Some(&BASE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrum_is_registered_with_l2_finality() {
        let c = lookup("arbitrum-one").expect("arbitrum-one in registry");
        assert_eq!(c.chain_id, 42161);
        assert_eq!(
            c.finality,
            Finality::FinalizedTag {
                fallback_depth: 1800
            }
        );
        assert!(
            c.log_window >= 1000,
            "sparse L2 wants a wide getLogs window"
        );
        assert!(!c.rpc_urls.is_empty());
        // Aliases resolve to the same chain.
        assert_eq!(lookup("arb").unwrap().chain_id, 42161);
        assert_eq!(lookup("arbitrum").unwrap().chain_id, 42161);
    }

    #[test]
    fn mainnet_uses_depth_finality() {
        let c = lookup("mainnet").unwrap();
        assert_eq!(c.finality, Finality::Depth(64));
        assert_eq!(c.log_window, 20);
    }

    #[test]
    fn base_is_registered_as_op_stack_l2() {
        let c = lookup("base").expect("base in registry");
        assert_eq!(c.chain_id, 8453);
        // OP-stack L2 → same finalized-tag policy as Arbitrum.
        assert!(matches!(c.finality, Finality::FinalizedTag { .. }));
        assert!(!c.rpc_urls.is_empty());
        assert_eq!(lookup("base-mainnet").unwrap().chain_id, 8453);
    }

    #[test]
    fn unknown_chain_is_none() {
        assert!(lookup("dogechain").is_none());
    }
}
