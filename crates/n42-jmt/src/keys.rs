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

/// One key-derivation job for [`derive_keys_batch`].
pub enum KeyJob {
    /// `account_key(address)` — a 32-byte single-block blake3 input.
    Account(Address),
    /// `storage_key(address, slot)` — exactly one 64-byte block.
    Storage(Address, U256),
}

/// Derive many account/storage keys at once via the SIMD single-block kernel
/// (16-way AVX-512 / 8-way AVX2 / scalar). Byte-identical to calling
/// [`account_key`]/[`storage_key`] per job — both build the same
/// `domain || address [|| slot]` blake3 input (locked by tests).
pub fn derive_keys_batch(jobs: &[KeyJob]) -> Vec<[u8; 32]> {
    let msgs: Vec<([u8; 64], u32)> = jobs
        .iter()
        .map(|j| {
            let mut buf = [0u8; 64];
            match j {
                KeyJob::Account(addr) => {
                    buf[..12].copy_from_slice(n42_bmt_core::ACCOUNT_DOMAIN);
                    buf[12..32].copy_from_slice(addr.as_slice());
                    (buf, 32)
                }
                KeyJob::Storage(addr, slot) => {
                    buf[..12].copy_from_slice(n42_bmt_core::STORAGE_DOMAIN);
                    buf[12..32].copy_from_slice(addr.as_slice());
                    buf[32..64].copy_from_slice(&slot.to_be_bytes::<32>());
                    (buf, 64)
                }
            }
        })
        .collect();
    let mut out = vec![[0u8; 32]; jobs.len()];
    n42_twig_core::hash_singleblock_batch(&msgs, &mut out);
    out
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

    /// Batched derivation must be byte-identical to per-key derivation —
    /// mixed account/storage jobs across full batches and tails.
    #[test]
    fn batch_derivation_matches_scalar() {
        let mut jobs = Vec::new();
        let mut want = Vec::new();
        for i in 0..53u64 {
            let mut b = [0u8; 20];
            b[..8].copy_from_slice(&i.to_le_bytes());
            let addr = Address::from(b);
            if i % 3 == 0 {
                let slot = U256::from(i * 7 + 1);
                jobs.push(KeyJob::Storage(addr, slot));
                want.push(storage_key(&addr, &slot).0);
            } else {
                jobs.push(KeyJob::Account(addr));
                want.push(account_key(&addr).0);
            }
        }
        assert_eq!(derive_keys_batch(&jobs), want);
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
