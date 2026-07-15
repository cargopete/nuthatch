//! nuthatch — be your own indexer.
//!
//! The crate's modules are exposed as a library so a second front-end — notably `nuthatch-node`,
//! the colocated reth ExEx build (RFC-0003) — can reuse the *same* indexing core (decode → hot
//! store → seal → IVM → serve) rather than fork it. The `nuthatch` binary (`main.rs`) is one such
//! front-end; a reth-driven one is another, and both drive the pipeline through the `Source` trait.

pub mod abi;
pub mod analytics;
pub mod chains;
pub mod check;
pub mod cli;
pub mod config;
pub mod indexer;
pub mod mcp;
pub mod project;
pub mod registry;
pub mod rpc;
pub mod seal;
pub mod serve;
pub mod source;
pub mod store;
pub mod transform;
pub mod views;
