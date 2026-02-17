mod keys;
mod aggregate;
mod verify;

/// BLS12-381 Domain Separation Tag for signatures.
///
/// Single definition used across signing, verification, and aggregation.
pub(crate) const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

pub use keys::{BlsError, BlsPublicKey, BlsSecretKey, BlsSignature};
pub use aggregate::AggregateSignature;
pub use verify::{batch_verify, batch_verify_with_fallback};
