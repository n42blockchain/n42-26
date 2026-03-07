pub mod attestation;
pub mod code_cache;
pub mod packet;
pub mod quic_client;
pub mod receipt;
pub(crate) mod serde_helpers;
pub mod verification;
pub mod verifier;
pub mod wire;

pub use attestation::{
    AggregatedAttestation, AttestationBuilder, AttestationError, VerifierRegistry,
};
pub use code_cache::{CacheSyncMessage, CodeCache, HotContractTracker};
pub use packet::{PacketError, StreamPacket, VerificationPacket, WitnessAccount};
pub use receipt::{ReceiptError, VerificationReceipt, sign_receipt};
pub use verification::{BlockVerificationStatus, ReceiptAggregator};
pub use verifier::{
    StreamReplayDB, VerificationResult, VerifyError,
    update_cache_after_stream_verify, update_cache_after_verify,
    verify_block, verify_block_stream,
};

/// Derives an ETH address via `keccak256(pubkey_bytes)[12..]`.
pub fn bls_pubkey_to_address(pubkey: &[u8; 48]) -> alloy_primitives::Address {
    let hash = alloy_primitives::keccak256(pubkey);
    alloy_primitives::Address::from_slice(&hash[12..])
}
