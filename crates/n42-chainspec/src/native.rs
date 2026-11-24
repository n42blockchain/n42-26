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

/// Genesis `config` key gov5 reads the consensus engine from
/// (`params.ChainConfig.Consensus`).
pub const CONSENSUS_KEY: &str = "consensus";

/// Whether the genesis describes an N42-native chain: gov5's `IsN42Native`
/// (`params/config.go:473`) — consensus `hotstuff` or `apos`, or a `hotstuff`
/// / `apos` section in the config — and every Profile-1 genesis. The same rule
/// as n42-rs `crates/chainspec/src/qmdb.rs` `is_n42_native`.
pub fn is_n42_native(genesis: &Genesis) -> bool {
    if crate::profile1::genesis_declares_profile(genesis) {
        return true;
    }
    let fields = &genesis.config.extra_fields;
    let consensus: Option<String> = fields
        .get_deserialized(CONSENSUS_KEY)
        .and_then(Result::ok);
    matches!(consensus.as_deref(), Some("hotstuff" | "apos"))
        || fields.contains_key("hotstuff")
        || fields.contains_key("apos")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n42_native_follows_gov5() {
        let native = |config| is_n42_native(&genesis(config));
        assert!(native(serde_json::json!({ "chainId": 94, "consensus": "hotstuff" })));
        assert!(native(serde_json::json!({ "chainId": 94, "consensus": "apos" })));
        assert!(native(serde_json::json!({ "chainId": 4242, "hotstuff": { "leaderTenure": 8 } })));
        assert!(native(serde_json::json!({ "chainId": 4242, "apos": {} })));
        assert!(native(serde_json::json!({ "chainId": 4242, "n42Profile": 1 })));
        assert!(!native(serde_json::json!({ "chainId": 1 })));
        assert!(!native(serde_json::json!({ "chainId": 1, "consensus": "clique" })));
    }

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
