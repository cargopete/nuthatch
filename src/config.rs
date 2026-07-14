//! The one config file a project has: `nuthatch.toml`. Deliberately tiny.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONFIG_FILE: &str = "nuthatch.toml";
pub const ABI_FILE: &str = "abi.json";
pub const DB_FILE: &str = "nuthatch.redb";

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub chain: String,
    pub chain_id: u64,
    pub address: String,
    pub rpc_urls: Vec<String>,
    /// The event this skeleton decodes. Only "Transfer" is wired up so far.
    #[serde(default = "default_event")]
    pub event: String,
}

fn default_event() -> String {
    "Transfer".to_string()
}

impl Config {
    pub fn load(dir: &Path) -> Result<Config> {
        let path = dir.join(CONFIG_FILE);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no {CONFIG_FILE} in {} — run `nuthatch init` first", dir.display()))?;
        toml::from_str(&raw).context("failed to parse nuthatch.toml")
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(self).context("failed to serialise config")?;
        std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}
