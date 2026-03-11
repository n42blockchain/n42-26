use alloy_primitives::{Address, U256};
use jmt::KeyHash;

/// Domain separators to prevent cross-type hash collisions.
const ACCOUNT_DOMAIN: &[u8] = b"n42:account:";
const STORAGE_DOMAIN: &[u8] = b"n42:storage:";

/// Compute JMT key hash for an account leaf.
///
/// Encoding: `blake3(b"n42:account:" || address_20_bytes)` → KeyHash
/// Uses incremental hashing to avoid heap allocation on the hot path.
#[inline]
pub fn account_key(address: &Address) -> KeyHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ACCOUNT_DOMAIN);
    hasher.update(address.as_slice());
    KeyHash(*hasher.finalize().as_bytes())
}

/// Compute JMT key hash for a storage slot leaf.
///
/// Encoding: `blake3(b"n42:storage:" || address_20_bytes || slot_32_bytes)` → KeyHash
/// Address is included to scope storage keys per contract.
#[inline]
pub fn storage_key(address: &Address, slot: &U256) -> KeyHash {
    let slot_be = slot.to_be_bytes::<32>();
    let mut hasher = blake3::Hasher::new();
    hasher.update(STORAGE_DOMAIN);
    hasher.update(address.as_slice());
    hasher.update(&slot_be);
    KeyHash(*hasher.finalize().as_bytes())
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
}
