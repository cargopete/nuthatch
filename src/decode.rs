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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Log;

    /// A real mainnet USDC Transfer (block 25529850, log 139) captured during slice-1 bring-up.
    /// Golden fixture: fixed input → exact decoded output. Deterministic, no network.
    fn usdc_log() -> Log {
        Log {
            address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            topics: vec![
                TRANSFER_TOPIC0.into(),
                "0x000000000000000000000000943f303a8019652d3a14b29954b2d780dde42ca3".into(),
                "0x000000000000000000000000db5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0".into(),
            ],
            data: "0x000000000000000000000000000000000000000000000000000000001cd4ad20".into(),
            block_number: 25529850,
            tx_hash: "0x2b9c7352f1866915b642847f4f2b82dbcb109edf78a7b88128ceec198ab85aac".into(),
            log_index: 139,
        }
    }

    #[test]
    fn decodes_known_usdc_transfer() {
        let t = transfer(&usdc_log()).expect("should decode");
        assert_eq!(t.from, "0x943f303a8019652d3a14b29954b2d780dde42ca3");
        assert_eq!(t.to, "0xdb5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0");
        assert_eq!(t.value.as_deref(), Some("483700000")); // 483.70 USDC (6 decimals)
        assert_eq!(t.value_hex, "0x000000000000000000000000000000000000000000000000000000001cd4ad20");
        assert_eq!(t.block_number, 25529850);
        assert_eq!(t.log_index, 139);
    }

    #[test]
    fn rejects_log_with_too_few_topics() {
        let mut log = usdc_log();
        log.topics.truncate(1); // only topic0, no indexed from/to
        assert!(transfer(&log).is_none());
    }

    #[test]
    fn large_value_keeps_hex_but_no_decimal() {
        // A value that overflows u128 (high bytes set): decimal is None, hex still present.
        let mut log = usdc_log();
        log.data = format!("0x{}", "f".repeat(64));
        let t = transfer(&log).expect("should decode");
        assert_eq!(t.value, None);
        assert_eq!(t.value_hex, format!("0x{}", "f".repeat(64)));
    }

    #[test]
    fn zero_value_decodes_to_zero() {
        let mut log = usdc_log();
        log.data = format!("0x{}", "0".repeat(64));
        let t = transfer(&log).expect("should decode");
        assert_eq!(t.value.as_deref(), Some("0"));
    }

    #[test]
    fn addresses_are_lowercased_and_right_aligned() {
        let mut log = usdc_log();
        log.topics[1] =
            "0x000000000000000000000000AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let t = transfer(&log).expect("should decode");
        assert_eq!(t.from, "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    }
}
