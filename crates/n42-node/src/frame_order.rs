//! Block-building order that follows ingest frames.
//!
//! With `N42_FRAME_ORDER_BUILD=1` the builder takes transactions frame by
//! frame, in the order the frames arrived, instead of sender by sender. A block
//! built this way is a few long runs of whole frames, so its frame-referenced
//! form (`n42_consensus_service::orchestrator::frame_ref`) is a few kilobytes,
//! and the builder never scans the whole pool while frames last.
//!
//! Only the order changes. Every transaction still comes from the pool and is
//! executed by the builder, which skips one whose nonce or balance does not
//! fit; frames of one sender group arrive in nonce order, so that is rare.

use alloy_primitives::{Address, B256, TxHash};
use n42_tx_types::{FrameStore, StoredFrame};
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, ValidPoolTransaction, error::InvalidPoolTransactionError,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

/// Whether the builder walks frames (`N42_FRAME_ORDER_BUILD=1`). It needs the
/// frame store, which ingest fills only with `N42_FRAME_REF_BLOCKS=1`.
pub fn frame_order_build_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_FRAME_ORDER_BUILD").is_ok_and(|v| v == "1")
            && n42_tx_types::frame_ref_blocks_enabled()
    })
}

/// Frames the builder looks at for one block.
const FRAME_WALK: usize = 2_048;

type PoolTx<T> = Arc<ValidPoolTransaction<T>>;
type Rest<T> = Box<dyn BestTransactions<Item = PoolTx<T>>>;

/// Pool transactions in frame arrival order, then everything else.
///
/// `lookup` returns the pool's transactions for a list of hashes (any order,
/// missing ones left out). `rest` builds the ordinary iterator; it is called
/// only once the frames are used up, and what the frames already yielded is
/// filtered out of it.
pub struct FrameOrderedTransactions<T: PoolTransaction, L, R> {
    store: &'static FrameStore,
    lookup: L,
    frames: std::vec::IntoIter<Arc<StoredFrame>>,
    current: VecDeque<PoolTx<T>>,
    /// Frames with no transaction left in the pool: mined or dropped.
    used_up: Vec<B256>,
    yielded: HashSet<TxHash>,
    rest_factory: Option<R>,
    rest: Option<Rest<T>>,
    invalid_senders: HashSet<Address>,
    skip_blobs: bool,
    no_updates: bool,
}

impl<T, L, R> FrameOrderedTransactions<T, L, R>
where
    T: PoolTransaction,
    L: FnMut(Vec<TxHash>) -> Vec<PoolTx<T>>,
    R: FnOnce() -> Rest<T>,
{
    pub fn new(store: &'static FrameStore, lookup: L, rest: R) -> Self {
        Self {
            store,
            lookup,
            frames: store.unbuilt(FRAME_WALK).into_iter(),
            current: VecDeque::new(),
            used_up: Vec::new(),
            yielded: HashSet::new(),
            rest_factory: Some(rest),
            rest: None,
            invalid_senders: HashSet::new(),
            skip_blobs: false,
            no_updates: false,
        }
    }

    /// Loads the next frame that still has transactions in the pool.
    fn next_frame(&mut self) -> bool {
        for frame in self.frames.by_ref() {
            let mut found: HashMap<TxHash, PoolTx<T>> = (self.lookup)(frame.hashes.clone())
                .into_iter()
                .map(|tx| (*tx.hash(), tx))
                .collect();
            if found.is_empty() {
                self.used_up.push(frame.root);
                continue;
            }
            // Frame order, whatever order the pool answered in.
            self.current
                .extend(frame.hashes.iter().filter_map(|hash| found.remove(hash)));
            return true;
        }
        false
    }

    fn next_from_rest(&mut self) -> Option<PoolTx<T>> {
        if self.rest.is_none() {
            let mut rest = (self.rest_factory.take()?)();
            rest.set_skip_blobs(self.skip_blobs);
            if self.no_updates {
                rest.no_updates();
            }
            self.rest = Some(rest);
        }
        let rest = self.rest.as_mut()?;
        rest.find(|tx| !self.yielded.contains(tx.hash()))
    }
}

impl<T, L, R> Iterator for FrameOrderedTransactions<T, L, R>
where
    T: PoolTransaction,
    L: FnMut(Vec<TxHash>) -> Vec<PoolTx<T>>,
    R: FnOnce() -> Rest<T>,
{
    type Item = PoolTx<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while let Some(tx) = self.current.pop_front() {
                if self.invalid_senders.contains(tx.sender_ref()) {
                    continue;
                }
                if self.skip_blobs && tx.is_eip4844() {
                    self.invalid_senders.insert(tx.sender());
                    continue;
                }
                self.yielded.insert(*tx.hash());
                return Some(tx);
            }
            if !self.next_frame() {
                break;
            }
        }
        loop {
            let tx = self.next_from_rest()?;
            if !self.invalid_senders.contains(tx.sender_ref()) {
                return Some(tx);
            }
        }
    }
}

impl<T, L, R> BestTransactions for FrameOrderedTransactions<T, L, R>
where
    T: PoolTransaction,
    L: FnMut(Vec<TxHash>) -> Vec<PoolTx<T>> + Send,
    R: FnOnce() -> Rest<T> + Send,
{
    fn mark_invalid(&mut self, tx: &Self::Item, kind: InvalidPoolTransactionError) {
        self.invalid_senders.insert(tx.sender());
        if let Some(rest) = self.rest.as_mut() {
            rest.mark_invalid(tx, kind);
        }
    }

    fn no_updates(&mut self) {
        self.no_updates = true;
        if let Some(rest) = self.rest.as_mut() {
            rest.no_updates();
        }
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.skip_blobs = skip_blobs;
        if let Some(rest) = self.rest.as_mut() {
            rest.set_skip_blobs(skip_blobs);
        }
    }
}

impl<T: PoolTransaction, L, R> Drop for FrameOrderedTransactions<T, L, R> {
    fn drop(&mut self) {
        // Later builds start past the frames this one found used up.
        self.store.retire(&self.used_up);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use reth_transaction_pool::test_utils::{MockTransaction, MockTransactionFactory};

    type Tx = PoolTx<MockTransaction>;

    fn tx(factory: &mut MockTransactionFactory, sender: u8, nonce: u64, hash: u8) -> Tx {
        factory.validated_arc(
            MockTransaction::legacy()
                .with_sender(Address::with_last_byte(sender))
                .with_nonce(nonce)
                .with_hash(B256::with_last_byte(hash)),
        )
    }

    /// The ordinary iterator of the tests: a fixed list.
    struct Listed(VecDeque<Tx>);

    impl Iterator for Listed {
        type Item = Tx;
        fn next(&mut self) -> Option<Tx> {
            self.0.pop_front()
        }
    }

    impl BestTransactions for Listed {
        fn mark_invalid(&mut self, _: &Tx, _: InvalidPoolTransactionError) {}
        fn no_updates(&mut self) {}
        fn set_skip_blobs(&mut self, _: bool) {}
    }

    fn store_frame(store: &FrameStore, seed: u8, txs: &[&Tx]) -> B256 {
        let hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();
        let root = B256::repeat_byte(seed);
        assert!(store.insert(
            root,
            hashes.iter().map(|hash| Bytes::copy_from_slice(hash.as_slice())).collect(),
            hashes,
        ));
        root
    }

    #[test]
    fn frames_come_first_in_arrival_order_then_the_rest_without_repeats() {
        let mut factory = MockTransactionFactory::default();
        // Two senders, two nonces each; a frame holds one nonce of both.
        let a0 = tx(&mut factory, 1, 0, 1);
        let b0 = tx(&mut factory, 2, 0, 2);
        let a1 = tx(&mut factory, 1, 1, 3);
        let b1 = tx(&mut factory, 2, 1, 4);
        let loose = tx(&mut factory, 3, 0, 5);
        let mined = tx(&mut factory, 4, 0, 6);

        let store: &'static FrameStore = Box::leak(Box::new(FrameStore::new(8)));
        let mined_root = store_frame(store, 0xa0, &[&mined]);
        store_frame(store, 0xa1, &[&a0, &b0]);
        store_frame(store, 0xa2, &[&a1, &b1]);

        // The pool holds everything but the mined frame, and answers in
        // reverse order to show the frame order is restored.
        let pool = vec![a0.clone(), b0.clone(), a1.clone(), b1.clone(), loose.clone()];
        let lookup = {
            let pool = pool.clone();
            move |hashes: Vec<TxHash>| -> Vec<Tx> {
                pool.iter().rev().filter(|tx| hashes.contains(tx.hash())).cloned().collect()
            }
        };
        let rest = {
            let pool = pool.clone();
            move || Box::new(Listed(pool.into_iter().collect())) as Rest<MockTransaction>
        };
        let order: Vec<B256> = FrameOrderedTransactions::new(store, lookup.clone(), rest.clone())
            .map(|tx| *tx.hash())
            .collect();
        assert_eq!(order, [&a0, &b0, &a1, &b1, &loose].map(|tx| *tx.hash()).to_vec());

        // The used-up frame left the walk but is still held.
        let walk: Vec<B256> = store.unbuilt(8).iter().map(|frame| frame.root).collect();
        assert_eq!(walk, vec![B256::repeat_byte(0xa1), B256::repeat_byte(0xa2)]);
        assert!(store.get(&mined_root).is_some());

        // A sender marked invalid loses its later transactions, in frames and after.
        let mut best = FrameOrderedTransactions::new(store, lookup, rest);
        let first = best.next().unwrap();
        assert_eq!(first.hash(), a0.hash());
        best.mark_invalid(
            &first,
            InvalidPoolTransactionError::Underpriced,
        );
        let order: Vec<B256> = best.map(|tx| *tx.hash()).collect();
        assert_eq!(order, [&b0, &b1, &loose].map(|tx| *tx.hash()).to_vec());
    }
}
