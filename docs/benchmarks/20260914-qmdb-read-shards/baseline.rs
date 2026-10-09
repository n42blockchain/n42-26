//! Immutable QMDB values at an exact authenticated block, for execution reads.
//!
//! This index shares unchanged nodes and values between versions. It does not
//! compute another commitment: only the root store may construct/publish it,
//! from an authenticated tree or from operations whose binary root it checked.
//! A pinned view needs neither the root-store mutex nor a mutable-tip lookup.

use alloy_primitives::{Address, B256, U256};
use n42_twig_core::{
    qmdb_compat::{QmdbOperation, encode_gov5_account_value, gov5_account_key, gov5_storage_key},
    qmdb_leaf_tree::QmdbLeafTree,
};
use reth_primitives_traits::Account;
use std::sync::Arc;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QmdbReadError {
    #[error("invalid QMDB account encoding")]
    InvalidAccount,
    #[error("invalid QMDB storage encoding: expected 32 bytes, got {0}")]
    InvalidStorage(usize),
}

/// Values from one exact block. Missing keys are answers; an unavailable block
/// is represented separately by the store returning no view.
#[derive(Clone)]
pub struct QmdbReadView {
    block_hash: B256,
    root: B256,
    values: imbl::HashMap<[u8; 32], Arc<[u8]>>,
}

impl std::fmt::Debug for QmdbReadView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbReadView")
            .field("block_hash", &self.block_hash)
            .field("root", &self.root)
            .field("keys", &self.values.len())
            .finish()
    }
}

impl QmdbReadView {
    pub(crate) fn from_tree(block_hash: B256, root: B256, tree: &QmdbLeafTree) -> Self {
        Self {
            block_hash,
            root,
            values: tree
                .live_values()
                .map(|(key, value)| (*key, Arc::from(value)))
                .collect(),
        }
    }

    pub(crate) fn with_operations(
        &self,
        block_hash: B256,
        root: B256,
        operations: &[QmdbOperation],
    ) -> Self {
        let mut values = self.values.clone();
        for operation in operations {
            if let Some(value) = &operation.value {
                values.insert(operation.key, Arc::from(value.as_slice()));
            } else {
                values.remove(&operation.key);
            }
        }
        Self {
            block_hash,
            root,
            values,
        }
    }

    pub const fn block_hash(&self) -> B256 {
        self.block_hash
    }
    pub const fn root(&self) -> B256 {
        self.root
    }

    pub fn value(&self, key: &[u8; 32]) -> Option<&[u8]> {
        self.values.get(key).map(AsRef::as_ref)
    }

    /// Native Gov5's post-EIP-161 account semantics: its tree can retain an
    /// empty system account that the execution database treats as absent.
    pub fn account(&self, address: &Address) -> Result<Option<Account>, QmdbReadError> {
        self.value(&gov5_account_key(address.as_ref()))
            .map(decode_account)
            .transpose()
            .map(Option::flatten)
    }

    pub fn storage(&self, address: &Address, slot: &B256) -> Result<Option<U256>, QmdbReadError> {
        let Some(value) = self.value(&gov5_storage_key(address.as_ref(), slot.as_ref())) else {
            return Ok(None);
        };
        if value.len() != 32 {
            return Err(QmdbReadError::InvalidStorage(value.len()));
        }
        let value = U256::from_be_slice(value);
        Ok((!value.is_zero()).then_some(value))
    }
}

fn decode_account(value: &[u8]) -> Result<Option<Account>, QmdbReadError> {
    let invalid = || QmdbReadError::InvalidAccount;
    let bitmap = *value.first().ok_or_else(invalid)?;
    if bitmap & !0x0b != 0 {
        return Err(invalid());
    }
    let mut at = 1;
    let mut nonce = 0u64;
    if bitmap & 1 != 0 {
        for index in 0..10 {
            let byte = *value.get(at).ok_or_else(invalid)?;
            at += 1;
            if index == 9 && byte > 1 {
                return Err(invalid());
            }
            nonce |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                break;
            }
        }
    }
    let mut balance = U256::ZERO;
    if bitmap & 2 != 0 {
        let len = usize::from(*value.get(at).ok_or_else(invalid)?);
        at += 1;
        if len == 0 || len > 32 {
            return Err(invalid());
        }
        balance = U256::from_be_slice(value.get(at..at + len).ok_or_else(invalid)?);
        at += len;
    }
    let code_hash = if bitmap & 8 != 0 {
        let hash = B256::from_slice(value.get(at..at + 32).ok_or_else(invalid)?);
        at += 32;
        hash
    } else {
        B256::ZERO
    };
    // Enforce canonical varints, minimal integers, flags and complete input.
    if at != value.len()
        || encode_gov5_account_value(nonce, &balance.to_be_bytes(), &code_hash.0) != value
    {
        return Err(invalid());
    }
    if nonce == 0 && balance.is_zero() && code_hash.is_zero() {
        return Ok(None);
    }
    Ok(Some(Account {
        nonce,
        balance,
        bytecode_hash: (!code_hash.is_zero()).then_some(code_hash),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_codec_covers_nonce_and_balance_boundaries() {
        for nonce in [0, 1, 127, 128, u64::MAX] {
            for balance in [U256::ZERO, U256::from(1), U256::MAX] {
                for code in [B256::ZERO, B256::repeat_byte(0xab)] {
                    let encoded = encode_gov5_account_value(nonce, &balance.to_be_bytes(), &code.0);
                    let account = decode_account(&encoded).unwrap();
                    if nonce == 0 && balance.is_zero() && code.is_zero() {
                        assert_eq!(account, None);
                    } else {
                        let account = account.unwrap();
                        assert_eq!(account.nonce, nonce);
                        assert_eq!(account.balance, balance);
                        assert_eq!(account.bytecode_hash, (!code.is_zero()).then_some(code));
                    }
                }
            }
        }
    }

    #[test]
    fn corrupt_accounts_are_errors_not_absence() {
        for value in [
            vec![],
            vec![0, 1],
            vec![4],
            vec![1, 0],
            vec![1, 0x81, 0],
            vec![2, 0],
            vec![2, 1, 0],
            vec![2, 33],
            vec![8],
            [vec![1], vec![0xff; 10], vec![1]].concat(),
        ] {
            assert_eq!(
                decode_account(&value),
                Err(QmdbReadError::InvalidAccount),
                "{value:?}"
            );
        }
    }

    /// Read-index microbenchmark: excludes consensus, storage I/O and EVM.
    #[test]
    #[ignore]
    fn bench_immutable_read_views() {
        use std::{hint::black_box, time::Instant};
        let keys = std::env::var("N42_READ_BENCH_KEYS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(200_000);
        assert!(keys > 0);
        let storage = match std::env::var("N42_READ_BENCH_KIND").as_deref() {
            Err(_) | Ok("account") => false,
            Ok("storage") => true,
            Ok(other) => panic!("unknown benchmark kind {other}"),
        };
        let address = |n: u64| {
            let mut bytes = [0u8; 20];
            bytes[12..].copy_from_slice(&n.to_be_bytes());
            Address::from(bytes)
        };
        let key = |n| {
            if storage {
                gov5_storage_key(address(n).as_ref(), &B256::ZERO.0)
            } else {
                gov5_account_key(address(n).as_ref())
            }
        };
        let value = |generation: u64| {
            if storage {
                U256::from(generation).to_be_bytes::<32>().to_vec()
            } else {
                encode_gov5_account_value(
                    generation,
                    &U256::from(generation * 1_000_000_000).to_be_bytes(),
                    &B256::ZERO.0,
                )
            }
        };
        let operations: Vec<_> = (0..keys)
            .map(|n| QmdbOperation {
                key: key(n),
                value: Some(value(1)),
            })
            .collect();
        let mut tree = QmdbLeafTree::new();
        let root = B256::from(tree.apply_sorted_ops(operations).unwrap());
        let start = Instant::now();
        let view = QmdbReadView::from_tree(B256::ZERO, root, &tree);
        eprintln!(
            "read_view keys={keys} initialize_ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
        let operations: Vec<_> = (0..keys.min(147_000))
            .map(|n| QmdbOperation {
                key: key(n),
                value: Some(value(2)),
            })
            .collect();
        let root = B256::from(tree.apply_sorted_ops(operations.clone()).unwrap());
        let start = Instant::now();
        let next = view.with_operations(B256::repeat_byte(1), root, &operations);
        eprintln!(
            "read_view updates={} derive_ms={:.3}",
            operations.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
        for threads in [1, 16] {
            let count = 1_000_000 / threads;
            let start = Instant::now();
            std::thread::scope(|scope| {
                for worker in 0..threads {
                    let next = &next;
                    scope.spawn(move || {
                        let mut sum = 0u64;
                        for i in 0..count {
                            let n = ((i * 7919 + worker) as u64) % keys;
                            sum += if storage {
                                next.storage(&address(n), &B256::ZERO)
                                    .unwrap()
                                    .unwrap()
                                    .to::<u64>()
                            } else {
                                next.account(&address(n)).unwrap().unwrap().nonce
                            };
                        }
                        black_box(sum);
                    });
                }
            });
            let seconds = start.elapsed().as_secs_f64();
            eprintln!(
                "read_view threads={threads} reads={} elapsed_ms={:.3} reads_per_second={:.0}",
                count * threads,
                seconds * 1000.0,
                (count * threads) as f64 / seconds
            );
        }
        assert_eq!(view.value(&key(0)), Some(value(1).as_slice()));
        assert_eq!(next.value(&key(0)), Some(value(2).as_slice()));
    }
}
