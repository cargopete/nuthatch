//! The decode registry (RFC-0001): ABI-driven, deterministic event decode for N contracts.
//!
//! Replaces the hardcoded `Transfer` path. Given each contract's resolved ABI, we build one
//! immutable registry mapping topic0 → decoders (filtered by emitting address), and decode any log
//! into a typed row keyed to a per-(alias, event) table. No LLM ever sits here — it is deterministic
//! Rust, and the registry's content hash is recorded so re-execution is verifiable.

use alloy_dyn_abi::{DynSolValue, EventExt};
use alloy_json_abi::{Event, JsonAbi};
use alloy_primitives::{Address, B256};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::config::Config;
use crate::rpc::Log;
use std::path::Path;

/// How a Solidity value is stored canonically (exact form; SQL convenience forms are derived).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    Address,
    U64,
    I64,
    Word16, // 65..=128-bit int/uint, big-endian
    Word32, // >128-bit int/uint, big-endian
    Bool,
    FixedBytes,
    Bytes,
    Str,
    Json,   // arrays / tuples
    Hash32, // indexed dynamic type: the topic holds keccak(value), not the value
}

impl StorageKind {
    /// Map a Solidity type string (+ indexed flag) to its canonical storage kind.
    fn from_sol(ty: &str, indexed: bool) -> StorageKind {
        if indexed && is_hashed_when_indexed(ty) {
            return StorageKind::Hash32;
        }
        if ty == "address" {
            StorageKind::Address
        } else if ty == "bool" {
            StorageKind::Bool
        } else if ty == "string" {
            StorageKind::Str
        } else if ty == "bytes" {
            StorageKind::Bytes
        } else if let Some(bits) = ty.strip_prefix("uint").and_then(parse_bits) {
            uint_kind(bits)
        } else if let Some(bits) = ty.strip_prefix("int").and_then(parse_bits) {
            int_kind(bits)
        } else if ty.starts_with("bytes") {
            StorageKind::FixedBytes // bytes1..=bytes32
        } else {
            StorageKind::Json // arrays, tuples, and anything unrecognized
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            StorageKind::Address => "address",
            StorageKind::U64 => "u64",
            StorageKind::I64 => "i64",
            StorageKind::Word16 => "word16",
            StorageKind::Word32 => "word32",
            StorageKind::Bool => "bool",
            StorageKind::FixedBytes => "fixed_bytes",
            StorageKind::Bytes => "bytes",
            StorageKind::Str => "string",
            StorageKind::Json => "json",
            StorageKind::Hash32 => "hash32",
        }
    }
}

fn uint_kind(bits: usize) -> StorageKind {
    if bits <= 64 {
        StorageKind::U64
    } else if bits <= 128 {
        StorageKind::Word16
    } else {
        StorageKind::Word32
    }
}

fn int_kind(bits: usize) -> StorageKind {
    if bits <= 64 {
        StorageKind::I64
    } else if bits <= 128 {
        StorageKind::Word16
    } else {
        StorageKind::Word32
    }
}

/// `intN`/`uintN` default to 256 when N is omitted.
fn parse_bits(rest: &str) -> Option<usize> {
    if rest.is_empty() {
        Some(256)
    } else {
        rest.parse().ok()
    }
}

/// A dynamic (non-value) type whose indexed form is a keccak hash in the topic.
fn is_hashed_when_indexed(ty: &str) -> bool {
    ty == "string"
        || ty == "bytes"
        || ty.ends_with(']')
        || ty.starts_with('(')
        || ty.starts_with("tuple")
}

/// A canonically-encoded decoded value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Address([u8; 20]),
    U64(u64),
    I64(i64),
    Word16([u8; 16]),
    Word32([u8; 32]),
    Bool(bool),
    Bytes(Vec<u8>),
    Str(String),
    Json(String),
    Hash32([u8; 32]),
}

impl Value {
    /// LLM/HTTP-facing JSON. Big integers are hex (lossless); SQL views derive decimals.
    pub fn to_json(&self) -> Json {
        match self {
            Value::Address(a) => json!(format!("0x{}", hex::encode(a))),
            Value::U64(n) => json!(n),
            Value::I64(n) => json!(n),
            // Big integers: decimal string when it fits u128 (the common case, and queryable), else
            // hex. i256/negatives beyond u128 fall back to hex (a signed-decimal refinement is later).
            Value::Word16(b) => json!(u128::from_be_bytes(*b).to_string()),
            Value::Word32(b) => {
                if b[..16].iter().all(|&x| x == 0) {
                    json!(u128::from_be_bytes(b[16..].try_into().unwrap()).to_string())
                } else {
                    json!(format!("0x{}", hex::encode(b)))
                }
            }
            Value::Bool(b) => json!(b),
            Value::Bytes(b) => json!(format!("0x{}", hex::encode(b))),
            Value::Str(s) => json!(s),
            Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
            Value::Hash32(b) => json!(format!("0x{}", hex::encode(b))),
        }
    }
}

/// One output column in a table's schema.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub sol_type: String,
    pub kind: StorageKind,
    pub indexed: bool,
}

/// Decodes one event of one contract into rows of one table.
pub struct EventDecoder {
    pub alias: String,
    pub contract: Address,
    pub table: String,
    pub columns: Vec<Column>,
    pub topic0: B256,
    pub signature: String,
    event: Event,
}

impl EventDecoder {
    fn new(alias: &str, contract: Address, event: Event) -> EventDecoder {
        let columns: Vec<Column> = event
            .inputs
            .iter()
            .enumerate()
            .map(|(i, p)| Column {
                name: if p.name.is_empty() {
                    format!("arg{i}")
                } else {
                    p.name.clone()
                },
                sol_type: p.ty.clone(),
                kind: StorageKind::from_sol(&p.ty, p.indexed),
                indexed: p.indexed,
            })
            .collect();
        EventDecoder {
            alias: alias.to_string(),
            contract,
            table: format!("{alias}__{}", snake_case(&event.name)),
            columns,
            topic0: event.selector(),
            signature: event.signature(),
            event,
        }
    }
}

/// One decoded log row.
#[derive(Debug, Clone)]
pub struct DecodedRow {
    pub table: String,
    pub params: Vec<(String, Value)>,
    pub block_number: u64,
    pub log_index: u64,
    pub tx_hash: String,
    pub address: String,
}

impl DecodedRow {
    pub fn to_json(&self) -> Json {
        let mut obj = serde_json::Map::new();
        obj.insert("table".into(), json!(self.table));
        obj.insert("block_number".into(), json!(self.block_number));
        obj.insert("log_index".into(), json!(self.log_index));
        obj.insert("tx_hash".into(), json!(self.tx_hash));
        obj.insert("address".into(), json!(self.address));
        for (name, v) in &self.params {
            obj.insert(name.clone(), v.to_json());
        }
        Json::Object(obj)
    }

    /// True if this row looks like an ERC-20/721 `Transfer(address, address, uint)` — the shape the
    /// hardcoded balance view + transfer sealing understand.
    pub fn is_erc20_transfer(&self) -> bool {
        self.table.ends_with("__transfer")
            && self.params.len() == 3
            && matches!(self.params[0].1, Value::Address(_))
            && matches!(self.params[1].1, Value::Address(_))
    }

    /// (from, to, value-decimal-if-it-fits-u128, value-hex) for a transfer row, else None.
    pub fn erc20_transfer_fields(&self) -> Option<(String, String, Option<String>, String)> {
        if !self.is_erc20_transfer() {
            return None;
        }
        let addr = |v: &Value| match v {
            Value::Address(a) => Some(format!("0x{}", hex::encode(a))),
            _ => None,
        };
        let from = addr(&self.params[0].1)?;
        let to = addr(&self.params[1].1)?;
        let (value, value_hex) = match &self.params[2].1 {
            Value::U64(n) => (Some(n.to_string()), format!("0x{:064x}", n)),
            Value::Word16(b) => {
                let mut full = [0u8; 32];
                full[16..].copy_from_slice(b);
                (
                    Some(u128::from_be_bytes(*b).to_string()),
                    format!("0x{}", hex::encode(full)),
                )
            }
            Value::Word32(b) => {
                let hex = format!("0x{}", hex::encode(b));
                let value = b[..16]
                    .iter()
                    .all(|&x| x == 0)
                    .then(|| u128::from_be_bytes(b[16..].try_into().unwrap()).to_string());
                (value, hex)
            }
            _ => return None,
        };
        Some((from, to, value, value_hex))
    }
}

/// One contract to index: an alias, its address, and its resolved ABI.
pub struct ContractSpec {
    pub alias: String,
    pub address: Address,
    pub abi: JsonAbi,
}

/// The immutable per-nest decode registry.
pub struct DecodeRegistry {
    by_topic0: HashMap<B256, Vec<EventDecoder>>,
    hash: [u8; 32],
    skipped_anonymous: usize,
}

impl DecodeRegistry {
    /// Build from a nest's config: load each contract's vendored ABI and register its events.
    pub fn from_nest(dir: &Path, config: &Config) -> Result<DecodeRegistry> {
        let mut specs = Vec::with_capacity(config.contracts.len());
        for c in &config.contracts {
            let abi_path = dir.join(&c.abi);
            let raw = std::fs::read_to_string(&abi_path)
                .with_context(|| format!("reading ABI {}", abi_path.display()))?;
            let abi: JsonAbi = serde_json::from_str(&raw)
                .with_context(|| format!("parsing ABI {}", abi_path.display()))?;
            specs.push(ContractSpec {
                alias: c.alias.clone(),
                address: parse_address(&c.address)?,
                abi,
            });
        }
        Self::build(specs)
    }

    pub fn build(contracts: Vec<ContractSpec>) -> Result<DecodeRegistry> {
        let mut by_topic0: HashMap<B256, Vec<EventDecoder>> = HashMap::new();
        let mut skipped_anonymous = 0usize;

        for c in &contracts {
            // Detect overloaded event names within this contract for table disambiguation.
            let mut name_counts: HashMap<String, usize> = HashMap::new();
            for ev in c.abi.events() {
                if ev.anonymous {
                    continue;
                }
                *name_counts.entry(snake_case(&ev.name)).or_default() += 1;
            }
            for ev in c.abi.events() {
                if ev.anonymous {
                    skipped_anonymous += 1;
                    continue;
                }
                let mut dec = EventDecoder::new(&c.alias, c.address, ev.clone());
                if name_counts.get(&snake_case(&ev.name)).copied().unwrap_or(0) > 1 {
                    // Overload: append a 4-hex topic0 suffix.
                    let t0 = hex::encode(dec.topic0);
                    dec.table = format!("{}_{}", dec.table, &t0[..4]);
                }
                by_topic0.entry(dec.topic0).or_default().push(dec);
            }
        }

        let hash = registry_hash(&by_topic0);
        Ok(DecodeRegistry {
            by_topic0,
            hash,
            skipped_anonymous,
        })
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }

    pub fn skipped_anonymous(&self) -> usize {
        self.skipped_anonymous
    }

    /// All topic0s to request in a combined `eth_getLogs` filter.
    pub fn topic0s(&self) -> Vec<B256> {
        self.by_topic0.keys().copied().collect()
    }

    /// All contract addresses to request in a combined filter.
    pub fn addresses(&self) -> Vec<Address> {
        let mut set: Vec<Address> = self
            .by_topic0
            .values()
            .flatten()
            .map(|d| d.contract)
            .collect();
        set.sort();
        set.dedup();
        set
    }

    /// Every table this registry produces, with its columns (for schema generation).
    pub fn tables(&self) -> Vec<&EventDecoder> {
        let mut v: Vec<&EventDecoder> = self.by_topic0.values().flatten().collect();
        v.sort_by(|a, b| a.table.cmp(&b.table));
        v
    }

    /// Decode a log. Returns None if no decoder matches (topic0 + emitting address).
    pub fn decode(&self, log: &Log) -> Result<Option<DecodedRow>> {
        let Some(t0_str) = log.topics.first() else {
            return Ok(None);
        };
        let topic0 = parse_b256(t0_str)?;
        let Some(decoders) = self.by_topic0.get(&topic0) else {
            return Ok(None);
        };
        let emitter = parse_address(&log.address)?;
        // Contract-specific decoders first (Allium ordering; a future generic fallback appends).
        let Some(dec) = decoders.iter().find(|d| d.contract == emitter) else {
            return Ok(None);
        };

        let topics: Vec<B256> = log
            .topics
            .iter()
            .map(|t| parse_b256(t))
            .collect::<Result<_>>()?;
        let data = parse_bytes(&log.data)?;
        let decoded = dec
            .event
            .decode_log_parts(topics.iter().copied(), &data)
            .map_err(|e| anyhow!("decode {}: {e}", dec.signature))?;

        let mut indexed = decoded.indexed.iter();
        let mut body = decoded.body.iter();
        let mut params = Vec::with_capacity(dec.columns.len());
        for col in &dec.columns {
            let dv = if col.indexed {
                indexed.next()
            } else {
                body.next()
            }
            .ok_or_else(|| anyhow!("param count mismatch decoding {}", dec.signature))?;
            params.push((col.name.clone(), value_from_dynsol(dv, col)));
        }

        Ok(Some(DecodedRow {
            table: dec.table.clone(),
            params,
            block_number: log.block_number,
            log_index: log.log_index,
            tx_hash: log.tx_hash.clone(),
            address: format!("0x{}", hex::encode(emitter)),
        }))
    }
}

fn value_from_dynsol(dv: &DynSolValue, col: &Column) -> Value {
    // Indexed dynamic types arrive as the 32-byte topic hash.
    if col.kind == StorageKind::Hash32 {
        if let DynSolValue::FixedBytes(w, _) = dv {
            return Value::Hash32(w.0);
        }
    }
    match dv {
        DynSolValue::Address(a) => Value::Address(a.into_array()),
        DynSolValue::Bool(b) => Value::Bool(*b),
        DynSolValue::Uint(u, bits) => {
            if *bits <= 64 {
                Value::U64(u.to::<u64>())
            } else if *bits <= 128 {
                Value::Word16(u.to_be_bytes::<32>()[16..].try_into().unwrap())
            } else {
                Value::Word32(u.to_be_bytes::<32>())
            }
        }
        DynSolValue::Int(i, bits) => {
            if *bits <= 64 {
                Value::I64(i.as_i64())
            } else if *bits <= 128 {
                Value::Word16(i.to_be_bytes::<32>()[16..].try_into().unwrap())
            } else {
                Value::Word32(i.to_be_bytes::<32>())
            }
        }
        DynSolValue::FixedBytes(w, n) => Value::Bytes(w.0[..(*n).min(32)].to_vec()),
        DynSolValue::Bytes(b) => Value::Bytes(b.clone()),
        DynSolValue::String(s) => Value::Str(s.clone()),
        other => Value::Json(dynsol_to_json(other).to_string()),
    }
}

/// JSON rendering of compound / fallback values.
fn dynsol_to_json(dv: &DynSolValue) -> Json {
    match dv {
        DynSolValue::Address(a) => json!(format!("0x{}", hex::encode(a.into_array()))),
        DynSolValue::Bool(b) => json!(b),
        DynSolValue::Uint(u, _) => json!(u.to_string()),
        DynSolValue::Int(i, _) => json!(i.to_string()),
        DynSolValue::FixedBytes(w, n) => json!(format!("0x{}", hex::encode(&w.0[..(*n).min(32)]))),
        DynSolValue::Bytes(b) => json!(format!("0x{}", hex::encode(b))),
        DynSolValue::String(s) => json!(s),
        DynSolValue::Array(items) | DynSolValue::FixedArray(items) => {
            json!(items.iter().map(dynsol_to_json).collect::<Vec<_>>())
        }
        DynSolValue::Tuple(items) => json!(items.iter().map(dynsol_to_json).collect::<Vec<_>>()),
        _ => json!(null),
    }
}

/// sha256 over a canonical serialization of the registry (deterministic, order-independent).
fn registry_hash(by_topic0: &HashMap<B256, Vec<EventDecoder>>) -> [u8; 32] {
    let mut lines: Vec<String> = by_topic0
        .values()
        .flatten()
        .map(|d| {
            let cols: Vec<String> = d
                .columns
                .iter()
                .map(|c| format!("{}:{}:{}", c.name, c.sol_type, c.kind.as_str()))
                .collect();
            format!(
                "{}|0x{}|0x{}|{}|{}",
                d.alias,
                hex::encode(d.contract),
                hex::encode(d.topic0),
                d.signature,
                cols.join(",")
            )
        })
        .collect();
    lines.sort();
    Sha256::digest(lines.join("\n").as_bytes()).into()
}

fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_b256(s: &str) -> Result<B256> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("bad topic hex")?;
    if bytes.len() != 32 {
        return Err(anyhow!("topic is not 32 bytes"));
    }
    Ok(B256::from_slice(&bytes))
}

fn parse_address(s: &str) -> Result<Address> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("bad address hex")?;
    if bytes.len() != 20 {
        return Err(anyhow!("address is not 20 bytes"));
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_bytes(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).context("bad data hex")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abi(json: &str) -> JsonAbi {
        // Accept a bare events array by wrapping it as a contract ABI.
        serde_json::from_str(json).unwrap()
    }

    fn spec(alias: &str, addr: &str, abi_json: &str) -> ContractSpec {
        ContractSpec {
            alias: alias.into(),
            address: parse_address(addr).unwrap(),
            abi: abi(abi_json),
        }
    }

    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn log(addr: &str, topics: &[&str], data: &str, block: u64, li: u64) -> Log {
        Log {
            address: addr.into(),
            topics: topics.iter().map(|s| s.to_string()).collect(),
            data: data.into(),
            block_number: block,
            tx_hash: "0xtx".into(),
            log_index: li,
        }
    }

    const ERC20: &str = r#"[
        {"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false},
        {"type":"event","name":"Approval","inputs":[
            {"name":"owner","type":"address","indexed":true},
            {"name":"spender","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}
    ]"#;

    #[test]
    fn decodes_real_usdc_transfer() {
        let reg = DecodeRegistry::build(vec![spec("usdc", USDC, ERC20)]).unwrap();
        let l = log(
            USDC,
            &[
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                "0x000000000000000000000000943f303a8019652d3a14b29954b2d780dde42ca3",
                "0x000000000000000000000000db5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0",
            ],
            "0x000000000000000000000000000000000000000000000000000000001cd4ad20",
            25529850,
            139,
        );
        let row = reg.decode(&l).unwrap().unwrap();
        assert_eq!(row.table, "usdc__transfer");
        assert_eq!(row.params[0].0, "from");
        assert_eq!(
            row.params[0].1,
            Value::Address(
                hex::decode("943f303a8019652d3a14b29954b2d780dde42ca3")
                    .unwrap()
                    .try_into()
                    .unwrap()
            )
        );
        assert_eq!(row.params[2].0, "value");
        assert_eq!(
            row.params[2].1,
            Value::Word32({
                let mut b = [0u8; 32];
                b[28..].copy_from_slice(&483_700_000u32.to_be_bytes());
                b
            })
        );
        // JSON shape for serving
        let j = row.to_json();
        assert_eq!(j["from"], "0x943f303a8019652d3a14b29954b2d780dde42ca3");
        assert_eq!(j["block_number"], 25529850);
    }

    #[test]
    fn wrong_address_does_not_decode() {
        let reg = DecodeRegistry::build(vec![spec("usdc", USDC, ERC20)]).unwrap();
        let l = log(
            "0x1111111111111111111111111111111111111111",
            &[
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                "0x000000000000000000000000943f303a8019652d3a14b29954b2d780dde42ca3",
                "0x000000000000000000000000db5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0",
            ],
            "0x000000000000000000000000000000000000000000000000000000001cd4ad20",
            1,
            0,
        );
        assert!(reg.decode(&l).unwrap().is_none());
    }

    #[test]
    fn same_signature_two_contracts_land_in_separate_tables() {
        let usdc = spec("usdc", USDC, ERC20);
        let weth = spec("weth", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", ERC20);
        let reg = DecodeRegistry::build(vec![usdc, weth]).unwrap();
        let tables: Vec<&str> = reg.tables().iter().map(|d| d.table.as_str()).collect();
        assert!(tables.contains(&"usdc__transfer"));
        assert!(tables.contains(&"weth__transfer"));
        // one topic0, two decoders keyed by address
        assert_eq!(reg.topic0s().len(), 2); // Transfer + Approval share across both → 2 distinct topic0s
        assert_eq!(reg.addresses().len(), 2);
    }

    #[test]
    fn type_mapping_covers_value_and_dynamic_kinds() {
        assert_eq!(
            StorageKind::from_sol("address", false),
            StorageKind::Address
        );
        assert_eq!(StorageKind::from_sol("uint256", false), StorageKind::Word32);
        assert_eq!(StorageKind::from_sol("uint128", false), StorageKind::Word16);
        assert_eq!(StorageKind::from_sol("uint64", false), StorageKind::U64);
        assert_eq!(StorageKind::from_sol("int24", false), StorageKind::I64);
        assert_eq!(StorageKind::from_sol("int256", false), StorageKind::Word32);
        assert_eq!(StorageKind::from_sol("bool", false), StorageKind::Bool);
        assert_eq!(
            StorageKind::from_sol("bytes32", false),
            StorageKind::FixedBytes
        );
        assert_eq!(StorageKind::from_sol("bytes", false), StorageKind::Bytes);
        assert_eq!(StorageKind::from_sol("string", false), StorageKind::Str);
        assert_eq!(StorageKind::from_sol("uint256[]", false), StorageKind::Json);
        // indexed dynamic → hash
        assert_eq!(StorageKind::from_sol("string", true), StorageKind::Hash32);
        assert_eq!(StorageKind::from_sol("uint256", true), StorageKind::Word32);
    }

    #[test]
    fn registry_hash_is_stable_and_order_independent() {
        let a = DecodeRegistry::build(vec![
            spec("usdc", USDC, ERC20),
            spec("weth", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", ERC20),
        ])
        .unwrap();
        let b = DecodeRegistry::build(vec![
            spec("weth", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", ERC20),
            spec("usdc", USDC, ERC20),
        ])
        .unwrap();
        assert_eq!(
            a.hash(),
            b.hash(),
            "registry hash must not depend on input order"
        );
    }

    #[test]
    fn anonymous_events_are_skipped_and_counted() {
        let anon = r#"[
            {"type":"event","name":"Transfer","inputs":[
                {"name":"from","type":"address","indexed":true},
                {"name":"to","type":"address","indexed":true},
                {"name":"value","type":"uint256","indexed":false}],"anonymous":false},
            {"type":"event","name":"Secret","inputs":[
                {"name":"x","type":"uint256","indexed":false}],"anonymous":true}
        ]"#;
        let reg = DecodeRegistry::build(vec![spec("t", USDC, anon)]).unwrap();
        assert_eq!(reg.skipped_anonymous(), 1);
        assert_eq!(reg.tables().len(), 1); // only Transfer, Secret skipped
    }

    #[test]
    fn snake_case_events() {
        assert_eq!(snake_case("Transfer"), "transfer");
        assert_eq!(snake_case("PoolCreated"), "pool_created");
        assert_eq!(snake_case("OperatorSet"), "operator_set");
    }
}
