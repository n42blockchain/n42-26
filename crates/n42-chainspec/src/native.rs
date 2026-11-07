//! Genesis switches for the native transaction path.
//!
//! Both are read from the genesis `config` object under the same keys n42-rs
//! uses (n42-rs `crates/chainspec/src/qmdb.rs` at tag
//! `fleet7-x22-5.2M-20261024`), so one genesis file describes the chain to
//! both clients. Absent, or anything but `true`, means disabled.

use alloy_genesis::Genesis;

/// Genesis `config` key that enables the 0x50 alternative-signature
/// (Ed25519) transaction.
pub const ALT_SIG_TX_KEY: &str = "altSigTx";

/// Genesis `config` key that makes a frame-aligned block's transactions root
/// the binary Merkle root over its frames' roots instead of the ordered MPT
/// root. A body that is not frame-aligned keeps the MPT root.
pub const FRAME_BLOCKS_KEY: &str = "frameBlocks";

fn flag(genesis: &Genesis, key: &str) -> bool {
    genesis
        .config
        .extra_fields
        .get_deserialized::<bool>(key)
        .and_then(Result::ok)
        .unwrap_or(false)
}

/// Whether the genesis enables the 0x50 transaction. When disabled the type
/// is rejected at admission and in block validation.
pub fn alt_sig_tx_enabled(genesis: &Genesis) -> bool {
    flag(genesis, ALT_SIG_TX_KEY)
}

/// Whether the genesis enables frame blocks.
pub fn frame_blocks_enabled(genesis: &Genesis) -> bool {
    flag(genesis, FRAME_BLOCKS_KEY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn genesis(config: serde_json::Value) -> Genesis {
        serde_json::from_value(serde_json::json!({ "config": config, "alloc": {} })).unwrap()
    }

    #[test]
    fn flags_default_off_and_need_true() {
        let g = genesis(serde_json::json!({ "chainId": 1 }));
        assert!(!alt_sig_tx_enabled(&g));
        assert!(!frame_blocks_enabled(&g));

        let g = genesis(serde_json::json!({ "chainId": 1, "altSigTx": true, "frameBlocks": true }));
        assert!(alt_sig_tx_enabled(&g));
        assert!(frame_blocks_enabled(&g));

        // A non-boolean value is not an opt-in.
        let g = genesis(serde_json::json!({ "chainId": 1, "altSigTx": "true", "frameBlocks": 1 }));
        assert!(!alt_sig_tx_enabled(&g));
        assert!(!frame_blocks_enabled(&g));
    }
}
