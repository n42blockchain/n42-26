// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Frames a node admitted at ingest, kept so a block can be sent as references
//! to frames instead of transaction bytes.
//!
//! A frame-referenced block is a transport form only: the block hash and the
//! transactions root still commit to the full ordered transaction list. The
//! leader names, for each run of transactions, the frame they came from and
//! their positions in it; a follower that holds those frames puts the same
//! bytes back. A follower that misses a frame takes the block's full copy.

use alloy_primitives::{Address, B256, Bytes, map::B256HashMap};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

/// One admitted frame: its root and its transactions in frame order.
#[derive(Debug)]
pub struct StoredFrame {
    /// `frame_root` over `hashes`.
    pub root: B256,
    /// EIP-2718 bytes of each transaction, exactly as they arrived.
    pub transactions: Vec<Bytes>,
    /// Hash of each transaction.
    pub hashes: Vec<B256>,
    /// Authenticated sender of each transaction; empty when the frame was
    /// kept without them.
    pub senders: Vec<Address>,
}

#[derive(Default)]
struct Inner {
    frames: B256HashMap<Arc<StoredFrame>>,
    order: VecDeque<B256>,
    /// Frames a block builder has not yet found used up, in arrival order.
    unbuilt: VecDeque<B256>,
    /// Transaction hash to (frame root, position in the frame).
    by_transaction: B256HashMap<(B256, u32)>,
}

/// A bounded store of admitted frames; the oldest frame leaves first.
pub struct FrameStore {
    inner: Mutex<Inner>,
    capacity: usize,
}

impl std::fmt::Debug for FrameStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStore")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

/// Whether blocks travel as frame references (`N42_FRAME_REF_BLOCKS=1`). Off by
/// default. With it on, ingest keeps admitted frames and a leader sends
/// references for the transactions it finds in them. Every validator must run
/// a build that reads the form, and frames must reach every validator.
pub fn frame_ref_blocks_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("N42_FRAME_REF_BLOCKS").is_ok_and(|v| v == "1"))
}

impl FrameStore {
    /// A store of at most `capacity` frames (at least one).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            capacity: capacity.max(1),
        }
    }

    /// The process-wide store, sized by `N42_FRAME_STORE_FRAMES` (default
    /// 4,096 frames: about 600 MB of 1,000-transfer frames when full).
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<FrameStore> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let capacity = std::env::var("N42_FRAME_STORE_FRAMES")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(4_096);
            Self::new(capacity)
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a frame. `transactions` and `hashes` must be the same length and
    /// order; a frame already held, or an inconsistent one, is ignored. A
    /// transaction keeps the first frame that named it.
    pub fn insert(&self, root: B256, transactions: Vec<Bytes>, hashes: Vec<B256>) -> bool {
        self.insert_with_senders(root, transactions, hashes, Vec::new())
    }

    /// [`Self::insert`], also keeping the sender admission authenticated for
    /// each transaction (`senders` empty, or one per transaction).
    pub fn insert_with_senders(
        &self,
        root: B256,
        transactions: Vec<Bytes>,
        hashes: Vec<B256>,
        senders: Vec<Address>,
    ) -> bool {
        if transactions.len() != hashes.len() || transactions.is_empty() {
            return false;
        }
        if !senders.is_empty() && senders.len() != hashes.len() {
            return false;
        }
        let mut inner = self.lock();
        if inner.frames.contains_key(&root) {
            return false;
        }
        while inner.order.len() >= self.capacity {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.unbuilt.retain(|root| *root != oldest);
            if let Some(frame) = inner.frames.remove(&oldest) {
                for hash in &frame.hashes {
                    if inner.by_transaction.get(hash).is_some_and(|(r, _)| *r == oldest) {
                        inner.by_transaction.remove(hash);
                    }
                }
            }
        }
        for (position, hash) in hashes.iter().enumerate() {
            inner
                .by_transaction
                .entry(*hash)
                .or_insert((root, position as u32));
        }
        inner.order.push_back(root);
        inner.unbuilt.push_back(root);
        inner.frames.insert(
            root,
            Arc::new(StoredFrame {
                root,
                transactions,
                hashes,
                senders,
            }),
        );
        true
    }

    /// The sender admission authenticated for this transaction, if a held
    /// frame carries it.
    pub fn sender_of(&self, hash: &B256) -> Option<Address> {
        let inner = self.lock();
        let (root, position) = inner.by_transaction.get(hash)?;
        inner
            .frames
            .get(root)?
            .senders
            .get(*position as usize)
            .copied()
    }

    /// The frame with this root, if held.
    pub fn get(&self, root: &B256) -> Option<Arc<StoredFrame>> {
        self.lock().frames.get(root).cloned()
    }

    /// For each transaction hash, the frame holding it and its position.
    /// One lock for the whole list.
    pub fn locate_all(&self, hashes: &[B256]) -> Vec<Option<(B256, u32)>> {
        let inner = self.lock();
        hashes
            .iter()
            .map(|hash| inner.by_transaction.get(hash).copied())
            .collect()
    }

    /// The frames with these roots, `None` for each one not held.
    pub fn get_all(&self, roots: &[B256]) -> Vec<Option<Arc<StoredFrame>>> {
        let inner = self.lock();
        roots.iter().map(|root| inner.frames.get(root).cloned()).collect()
    }

    /// The first `limit` frames, in arrival order, that no builder has
    /// retired yet.
    pub fn unbuilt(&self, limit: usize) -> Vec<Arc<StoredFrame>> {
        let inner = self.lock();
        inner
            .unbuilt
            .iter()
            .take(limit)
            .filter_map(|root| inner.frames.get(root).cloned())
            .collect()
    }

    /// Takes frames out of the builders' walk; they stay available by root
    /// and by transaction until they age out. A builder retires a frame once
    /// none of its transactions is left to include.
    pub fn retire(&self, roots: &[B256]) {
        if roots.is_empty() {
            return;
        }
        let mut inner = self.lock();
        inner.unbuilt.retain(|root| !roots.contains(root));
    }

    /// Number of frames held.
    pub fn len(&self) -> usize {
        self.lock().order.len()
    }

    /// Whether no frame is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seed: u8, count: u8) -> (B256, Vec<Bytes>, Vec<B256>) {
        let transactions: Vec<Bytes> = (0..count)
            .map(|i| Bytes::from(vec![seed, i, 0x50]))
            .collect();
        let hashes: Vec<B256> = transactions
            .iter()
            .map(alloy_primitives::keccak256)
            .collect();
        (crate::frame_root(&hashes), transactions, hashes)
    }

    #[test]
    fn a_frame_kept_with_senders_answers_for_each_transaction() {
        let store = FrameStore::new(4);
        let hashes = vec![B256::repeat_byte(0xa1), B256::repeat_byte(0xa2)];
        let senders = vec![Address::repeat_byte(1), Address::repeat_byte(2)];
        let bytes = vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")];
        assert!(!store.insert_with_senders(
            B256::repeat_byte(0xf0),
            bytes.clone(),
            hashes.clone(),
            senders[..1].to_vec(),
        ));
        assert!(store.insert_with_senders(B256::repeat_byte(0xf1), bytes, hashes.clone(), senders));
        assert_eq!(store.sender_of(&hashes[1]), Some(Address::repeat_byte(2)));
        assert_eq!(store.sender_of(&B256::repeat_byte(0xa3)), None);

        // A frame kept without senders has no answer.
        let plain = B256::repeat_byte(0xb1);
        store.insert(B256::repeat_byte(0xf2), vec![Bytes::from_static(b"c")], vec![plain]);
        assert_eq!(store.sender_of(&plain), None);
    }

    #[test]
    fn frames_are_found_by_root_and_by_transaction_and_leave_oldest_first() {
        let store = FrameStore::new(2);
        let (root_a, txs_a, hashes_a) = frame(1, 3);
        let (root_b, txs_b, hashes_b) = frame(2, 2);
        let (root_c, txs_c, hashes_c) = frame(3, 1);
        assert!(store.insert(root_a, txs_a.clone(), hashes_a.clone()));
        assert!(!store.insert(root_a, txs_a.clone(), hashes_a.clone()));
        assert!(!store.insert(B256::ZERO, txs_a.clone(), hashes_b.clone()));
        assert!(store.insert(root_b, txs_b, hashes_b.clone()));
        assert_eq!(
            store.locate_all(&[hashes_a[2], hashes_b[0], B256::ZERO]),
            vec![Some((root_a, 2)), Some((root_b, 0)), None]
        );
        assert_eq!(store.get(&root_a).unwrap().transactions, txs_a);

        // The third frame pushes the first out, with its transaction index.
        assert!(store.insert(root_c, txs_c, hashes_c));
        assert_eq!(store.len(), 2);
        assert!(store.get(&root_a).is_none());
        assert_eq!(store.locate_all(&[hashes_a[0], hashes_b[1]]), vec![None, Some((root_b, 1))]);
        assert!(store.get_all(&[root_a, root_c])[1].is_some());

        // Builders walk frames in arrival order until they retire them.
        let walk = |store: &FrameStore| -> Vec<B256> {
            store.unbuilt(8).iter().map(|frame| frame.root).collect()
        };
        assert_eq!(walk(&store), vec![root_b, root_c]);
        assert_eq!(store.unbuilt(1).len(), 1);
        store.retire(&[root_b]);
        assert_eq!(walk(&store), vec![root_c]);
        assert!(store.get(&root_b).is_some());
    }
}
