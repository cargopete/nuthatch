//! Tiny chain registry. Ships sensible public-RPC defaults with round-robin failover — the
//! first-run killer is RPC friction, so out of the box you should not need to bring a key.
//! (The "no third-party" upgrade is to colocate with a reth node; that path comes later.)

pub struct Chain {
    pub name: &'static str,
    pub chain_id: u64,
    /// Tried in order, then round-robin, so a single flaky endpoint doesn't stall a run.
    pub rpc_urls: &'static [&'static str],
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
};

pub fn lookup(name: &str) -> Option<&'static Chain> {
    match name {
        "mainnet" | "ethereum" | "eth" => Some(&MAINNET),
        _ => None,
    }
}
