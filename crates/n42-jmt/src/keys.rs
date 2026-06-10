use alloy_primitives::{Address, U256};
use jmt::KeyHash;

/// Compute JMT key hash for an account leaf.
///
/// Encoding: `blake3(b"n42:account:" || address_20_bytes)` → KeyHash.
/// Delegates to [`n42_bmt_core::account_key`] so the builder side and the
/// mobile/FFI proof verifier derive byte-identical keys (the soundness premise
/// of [`n42_bmt_core::ShardedBmtProof::verify_for_key`]).
#[inline]
pub fn account_key(address: &Address) -> KeyHash {
    KeyHash(n42_bmt_core::account_key(&address.into_array()))
}

/// Compute JMT key hash for a storage slot leaf.
///
/// Encoding: `blake3(b"n42:storage:" || address_20_bytes || slot_32_bytes)` → KeyHash.
/// Address is included to scope storage keys per contract. Delegates to
/// [`n42_bmt_core::storage_key`] for a single source of truth.
#[inline]
pub fn storage_key(address: &Address, slot: &U256) -> KeyHash {
    KeyHash(n42_bmt_core::storage_key(
        &address.into_array(),
        &slot.to_be_bytes::<32>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_key_deterministic() {
        let addr = Address::repeat_byte(0x01);
        assert_eq!(account_key(&addr), account_key(&addr));
    }

    #[test]
    fn different_addresses_different_keys() {
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        assert_ne!(account_key(&a), account_key(&b));
    }

    #[test]
    fn storage_key_includes_address() {
        let slot = U256::from(42);
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        assert_ne!(storage_key(&a, &slot), storage_key(&b, &slot));
    }

    #[test]
    fn storage_key_includes_slot() {
        let addr = Address::repeat_byte(0x01);
        let s1 = U256::from(1);
        let s2 = U256::from(2);
        assert_ne!(storage_key(&addr, &s1), storage_key(&addr, &s2));
    }

    /// Soundness invariant: the builder-side key (n42-jmt) and the
    /// verifier-side key (n42-bmt-core, used by mobile/FFI) MUST be byte
    /// identical, or `ShardedBmtProof::verify_for_key` would reject honest
    /// proofs (or, worse, key binding would not actually bind).
    #[test]
    fn jmt_and_bmt_core_keys_agree() {
        let addr = Address::repeat_byte(0x7C);
        assert_eq!(
            account_key(&addr).0,
            n42_bmt_core::account_key(&addr.into_array()),
        );

        let slot = U256::from(0xDEAD_BEEFu64);
        assert_eq!(
            storage_key(&addr, &slot).0,
            n42_bmt_core::storage_key(&addr.into_array(), &slot.to_be_bytes::<32>()),
        );
    }
}
