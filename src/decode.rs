//! Deterministic decode. Skeleton scope: ERC-20 `Transfer(address,address,uint256)` only.
//! This is the seam that later generalises to full topic0-keyed, ABI-driven decoding — but it
//! stays deterministic Rust forever. No LLM ever sits here.

use serde::Serialize;

/// keccak256("Transfer(address,address,uint256)") — the ERC-20/721 Transfer topic0.
/// Hardcoded (not computed) to keep the skeleton dependency-free; a real decoder derives this.
pub const TRANSFER_TOPIC0: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

#[derive(Debug, Serialize)]
pub struct Transfer {
    pub from: String,
    pub to: String,
    /// Decimal value when it fits in u128, else null (with `value_hex` always present).
    pub value: Option<String>,
    pub value_hex: String,
    pub block_number: u64,
    pub tx_hash: String,
    pub log_index: u64,
}

/// Decode an ERC-20 Transfer log. Returns None if the log doesn't match the expected shape.
pub fn transfer(log: &crate::rpc::Log) -> Option<Transfer> {
    // topics: [topic0, from(indexed), to(indexed)]; data: value (32 bytes).
    if log.topics.len() < 3 {
        return None;
    }
    let from = address_from_topic(&log.topics[1])?;
    let to = address_from_topic(&log.topics[2])?;
    let value_hex = normalise_word(&log.data)?;
    let value = u128_from_word(&value_hex);
    Some(Transfer {
        from,
        to,
        value,
        value_hex: format!("0x{value_hex}"),
        block_number: log.block_number,
        tx_hash: log.tx_hash.clone(),
        log_index: log.log_index,
    })
}

/// An indexed address is right-aligned in a 32-byte topic: take the last 40 hex chars.
fn address_from_topic(topic: &str) -> Option<String> {
    let h = topic.strip_prefix("0x").unwrap_or(topic);
    if h.len() != 64 {
        return None;
    }
    Some(format!("0x{}", &h[24..].to_ascii_lowercase()))
}

/// Normalise a 32-byte data word to 64 lowercase hex chars (no 0x).
fn normalise_word(data: &str) -> Option<String> {
    let h = data.strip_prefix("0x").unwrap_or(data);
    if h.len() < 64 {
        return None;
    }
    Some(h[..64].to_ascii_lowercase())
}

/// Decimalise a 32-byte word iff the high 16 bytes are zero (fits u128). Covers ~all real tokens.
fn u128_from_word(word: &str) -> Option<String> {
    let (high, low) = word.split_at(32);
    if high.bytes().all(|b| b == b'0') {
        u128::from_str_radix(low, 16).ok().map(|v| v.to_string())
    } else {
        None
    }
}
