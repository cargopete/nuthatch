//! nuthatch — be your own indexer.
//!
//! The crate's modules are exposed as a library so a second front-end — notably `nuthatch-node`,
//! the colocated reth ExEx build (RFC-0003) — can reuse the *same* indexing core (decode → hot
//! store → seal → IVM → serve) rather than fork it. The `nuthatch` binary (`main.rs`) is one such
//! front-end; a reth-driven one is another, and both drive the pipeline through the `Source` trait.

pub mod abi;
pub mod alerts;
pub mod analytics;
pub mod audit;
pub mod bench;
pub mod blob;
pub mod chains;
pub mod check;
pub mod chunker;
pub mod cli;
pub mod config;
pub mod distribution;
pub mod effectful;
pub mod exposure;
pub mod factory;
pub mod flags;
pub mod indexer;
pub mod labels;
pub mod lifecycle;
pub mod lists;
pub mod mcp;
pub mod metrics;
pub mod pack;
pub mod progress;
pub mod project;
pub mod recipes;
pub mod registry;
pub mod roost;
pub mod rpc;
pub mod screen;
pub mod seal;
pub mod semantic;
pub mod serve;
pub mod skill;
pub mod source;
pub mod sql_errors;
pub mod starlark_config;
pub mod store;
pub mod transform;
pub mod velocity;
pub mod views;
pub mod webhooks;
