//! Immutable QMDB values at an exact authenticated block, for execution reads.
//!
//! This index shares unchanged nodes and values between versions. It does not
//! compute another commitment: only the root store may construct/publish it,
//! from an authenticated tree or from operations whose binary root it checked.
//! A pinned view needs neither the root-store mutex nor a mutable-tip lookup.

use alloy_primitives::{Address, B256, U256};
use n42_twig_core::{
    qmdb_compat::{GOV5_EMPTY_CODE_HASH, QmdbOperation, gov5_account_key, gov5_storage_key},
    qmdb_leaf_tree::QmdbLeafTree,
};
use rayon::prelude::*;
use reth_primitives_traits::Account;
use std::sync::{Arc, OnceLock};

// The first byte is already uniformly distributed by the QMDB key hash.
// Shards only partition a derived lookup index, never the binary commitment.
const READ_SHARDS: usize = 64;
const PARALLEL_UPDATES: usize = 8_192;
#[derive(Clone)]
struct ReadShard {
    values: imbl::HashMap<[u8; 32], ReadValue>,
    logical_bytes: usize,
}

// Ordinary funded EOA values and storage words fit in 32 bytes. Keeping them
// inside the persistent map avoids a separate allocation and pointer chase;
// longer contract accounts/values retain shared, immutable byte storage.
#[derive(Clone)]
enum ReadValue {
    Inline { len: u8, bytes: [u8; 32] },
    Shared(Arc<[u8]>),
}

impl ReadValue {
    fn new(value: &[u8]) -> Self {
        if value.len() <= 32 {
            let mut bytes = [0; 32];
            bytes[..value.len()].copy_from_slice(value);
            Self::Inline {
                len: value.len() as u8,
                bytes,
            }
        } else {
            Self::Shared(Arc::from(value))
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..usize::from(*len)],
            Self::Shared(value) => value,
        }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }
}

fn read_shard(key: &[u8; 32]) -> usize {
    usize::from(key[0]) % READ_SHARDS
}

// View construction runs under the forest lock. Do not schedule it on the
// global EVM pool: another task waiting for that lock could exhaust its workers.
// A bounded dedicated pool only handles independent index shards. If worker
// creation fails, the identical sequential path remains available.
fn read_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        if threads == 1 {
            return None;
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|n| format!("qmdb-read-{n}"))
            .build()
            .ok()
    })
    .as_ref()
}

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
    values: Vec<ReadShard>,
}

impl std::fmt::Debug for QmdbReadView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbReadView")
            .field("block_hash", &self.block_hash)
            .field("root", &self.root)
            .field(
                "keys",
                &self
                    .values
                    .iter()
                    .map(|shard| shard.values.len())
                    .sum::<usize>(),
            )
            .field("logical_bytes", &self.logical_bytes())
            .finish()
    }
}

impl QmdbReadView {
    pub(crate) fn from_tree(block_hash: B256, root: B256, tree: &QmdbLeafTree) -> Self {
        let mut groups: Vec<Vec<_>> = (0..READ_SHARDS).map(|_| Vec::new()).collect();
        for (key, value) in tree.live_values() {
            groups[read_shard(key)].push((*key, value));
        }
        let build = |group: Vec<([u8; 32], &[u8])>| {
            let logical_bytes = group.iter().map(|(_, value)| 32 + value.len()).sum();
            let values = group
                .into_iter()
                .map(|(key, value)| (key, ReadValue::new(value)))
                .collect();
            ReadShard {
                values,
                logical_bytes,
            }
        };
        let count = groups.iter().map(Vec::len).sum::<usize>();
        let values = if count >= PARALLEL_UPDATES
            && let Some(pool) = read_pool()
        {
            pool.install(|| groups.into_par_iter().map(build).collect())
        } else {
            groups.into_iter().map(build).collect()
        };
        Self {
            block_hash,
            root,
            values,
        }
    }

    pub(crate) fn with_operations(
        &self,
        block_hash: B256,
        root: B256,
        operations: &[QmdbOperation],
    ) -> Self {
        let pool = if operations.len() >= PARALLEL_UPDATES {
            read_pool()
        } else {
            None
        };
        self.with_operations_in_pool(block_hash, root, operations, pool)
    }

    fn with_operations_in_pool(
        &self,
        block_hash: B256,
        root: B256,
        operations: &[QmdbOperation],
        pool: Option<&rayon::ThreadPool>,
    ) -> Self {
        let mut values = self.values.clone();
        let apply = |shard: &mut ReadShard, operation: &QmdbOperation| {
            if let Some(value) = &operation.value {
                let previous = shard.values.insert(operation.key, ReadValue::new(value));
                shard.logical_bytes += 32 + value.len();
                if let Some(previous) = previous {
                    shard.logical_bytes -= 32 + previous.len();
                }
            } else if let Some(previous) = shard.values.remove(&operation.key) {
                shard.logical_bytes -= 32 + previous.len();
            }
        };
        if let Some(pool) = pool {
            let mut groups: Vec<Vec<_>> = (0..READ_SHARDS).map(|_| Vec::new()).collect();
            for operation in operations {
                groups[read_shard(&operation.key)].push(operation);
            }
            pool.install(|| {
                values
                    .par_iter_mut()
                    .zip(groups)
                    .for_each(|(shard, group)| {
                        for operation in group {
                            apply(shard, operation);
                        }
                    })
            });
        } else {
            for operation in operations {
                apply(&mut values[read_shard(&operation.key)], operation);
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

    /// Sum of live key/value lengths. Cache admission charges each view in
    /// full, even when its nodes are shared. This is not an allocator/RSS size.
    pub fn logical_bytes(&self) -> usize {
        self.values.iter().map(|shard| shard.logical_bytes).sum()
    }

    pub fn value(&self, key: &[u8; 32]) -> Option<&[u8]> {
        self.values[read_shard(key)]
            .values
            .get(key)
            .map(ReadValue::as_slice)
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
                // A present zero nonce or a zero final group is nonminimal.
                if byte == 0 {
                    return Err(invalid());
                }
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
        let bytes = value.get(at..at + len).ok_or_else(invalid)?;
        if bytes[0] == 0 {
            return Err(invalid());
        }
        balance = U256::from_be_slice(bytes);
        at += len;
    }
    let code_hash = if bitmap & 8 != 0 {
        let hash = B256::from_slice(value.get(at..at + 32).ok_or_else(invalid)?);
        if hash.is_zero() || hash.0 == GOV5_EMPTY_CODE_HASH {
            return Err(invalid());
        }
        at += 32;
        hash
    } else {
        B256::ZERO
    };
    // Canonical fields were checked in place, without re-encoding or allocating.
    if at != value.len() {
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
    use n42_twig_core::qmdb_compat::encode_gov5_account_value;

    #[test]
    fn sharded_versions_match_tree_and_sequential_updates() {
        let mut tree = QmdbLeafTree::new();
        let mut view = QmdbReadView::from_tree(B256::ZERO, B256::from(tree.root()), &tree);
        let key = |n: u64| {
            let mut address = [0u8; 20];
            address[12..].copy_from_slice(&n.to_be_bytes());
            gov5_account_key(&address)
        };
        let mut expected = std::collections::BTreeMap::new();
        let mut history = Vec::new();
        // Cross the parallel threshold, update/insert/delete across every
        // shard, then exercise small and empty blocks after the large ones.
        for (round, count) in [8191, 8192, 16_000, 1, 0].into_iter().enumerate() {
            let operations: Vec<_> = (0..count)
                .map(|n| QmdbOperation {
                    key: key(n),
                    value: (!(n + round as u64).is_multiple_of(5))
                        .then(|| vec![round as u8; ((n + round as u64 * 17) % 80) as usize]),
                })
                .collect();
            for op in &operations {
                if let Some(value) = &op.value {
                    expected.insert(op.key, value.clone());
                } else {
                    expected.remove(&op.key);
                }
            }
            let root = B256::from(tree.apply_sorted_ops(operations.clone()).unwrap());
            let hash = B256::repeat_byte(round as u8 + 1);
            let next = view.with_operations(hash, root, &operations);
            let parallel = view.with_operations_in_pool(hash, root, &operations, read_pool());
            // Independent sequential batching of the same mutations.
            let mut sequential = view.clone();
            for chunk in operations.chunks(1000) {
                sequential = sequential.with_operations(hash, root, chunk);
            }
            let rebuilt = QmdbReadView::from_tree(hash, root, &tree);
            for n in 0..16_001 {
                let key = key(n);
                let want = expected.get(&key).map(Vec::as_slice);
                assert_eq!(next.value(&key), want);
                assert_eq!(parallel.value(&key), want);
                assert_eq!(sequential.value(&key), want);
                assert_eq!(rebuilt.value(&key), want);
            }
            let bytes = expected
                .values()
                .map(|value| 32 + value.len())
                .sum::<usize>();
            assert_eq!(next.logical_bytes(), bytes);
            assert_eq!(parallel.logical_bytes(), bytes);
            assert_eq!(sequential.logical_bytes(), bytes);
            assert_eq!(rebuilt.logical_bytes(), bytes);
            history.push((next.clone(), expected.clone()));
            view = next;
        }
        // Later mutations must leave every older version intact.
        for (old, expected) in history {
            for n in 0..16_001 {
                let key = key(n);
                assert_eq!(old.value(&key), expected.get(&key).map(Vec::as_slice));
            }
        }
    }

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
    fn account_decode_agrees_with_canonical_encoder_on_generated_inputs() {
        let mut seed = 0x42cafe12345678u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for index in 0..4096 {
            let nonce = random() >> (index % 64);
            let mut balance = [0u8; 32];
            for chunk in balance.chunks_mut(8) {
                chunk.copy_from_slice(&random().to_be_bytes());
            }
            balance[..index % 33].fill(0);
            let mut code = [0u8; 32];
            if index.is_multiple_of(3) {
                code[..8].copy_from_slice(&random().to_be_bytes());
            }
            let encoded = encode_gov5_account_value(nonce, &balance, &code);
            let account = decode_account(&encoded).unwrap().unwrap_or_default();
            assert_eq!(account.nonce, nonce);
            assert_eq!(account.balance, U256::from_be_bytes(balance));
            assert_eq!(
                account.bytecode_hash,
                (code != [0; 32]).then_some(B256::from(code))
            );
            // Mutations may produce another valid value. Every accepted value
            // must still reproduce the exact canonical native encoding.
            for at in 0..encoded.len() {
                let mut mutated = encoded.clone();
                mutated[at] ^= (random() as u8) | 1;
                if let Ok(account) = decode_account(&mutated) {
                    let account = account.unwrap_or_default();
                    assert_eq!(
                        encode_gov5_account_value(
                            account.nonce,
                            &account.balance.to_be_bytes(),
                            &account.bytecode_hash.unwrap_or_default().0,
                        ),
                        mutated
                    );
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
            vec![2, 2, 0, 1],
            vec![2, 33],
            vec![8],
            [vec![8], vec![0; 32]].concat(),
            [vec![8], GOV5_EMPTY_CODE_HASH.to_vec()].concat(),
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
