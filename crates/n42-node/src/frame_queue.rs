//! Admitted native frames queued for block building, outside the pool.
//!
//! With `N42_FRAME_QUEUE=1` (and `N42_FRAME_REF_BLOCKS=1`, which keeps the
//! frames) an admitted native frame is not inserted into the transaction
//! pool. It waits here, in arrival order, and the builder takes whole frames
//! from the front. Each canonical block then marks its transactions as mined.
//!
//! The pool costs a per-transaction insert on every node and, after each
//! block, a removal pass that holds the pool lock while the next leader is
//! waiting to build. The queue does one lock per frame on the way in and one
//! per block on the way out.
//!
//! Transactions that wait here are not visible to pool RPCs. The builder
//! still executes every one of them and skips what does not fit; a frame
//! that is not used up within the time to live is dropped.

use crate::native_pool::N42PooledTransaction;
use alloy_primitives::{Address, B256, map::B256HashMap, map::B256HashSet};
use n42_tx_types::FrameStore;
use reth_transaction_pool::{
    BestTransactions, ValidPoolTransaction, error::InvalidPoolTransactionError,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{Arc, Condvar, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// Whether native frames bypass the pool (`N42_FRAME_QUEUE=1`). Needs the
/// frame store, which ingest fills only with `N42_FRAME_REF_BLOCKS=1`.
pub fn frame_queue_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_FRAME_QUEUE").is_ok_and(|v| v == "1")
            && n42_tx_types::frame_ref_blocks_enabled()
    })
}

/// A queued transaction in the form the builder consumes.
pub type QueuedTx = Arc<ValidPoolTransaction<N42PooledTransaction>>;

/// Blocks whose transactions are remembered when no held frame named them,
/// so a frame that arrives after its block is not queued again.
const EARLY_BLOCKS: usize = 64;

/// Canonical blocks whose removed transactions are kept so a reorg can put
/// them back.
const RECENT_BLOCKS: usize = 8;

/// Frames handed to a builder per lock.
const WALK_FRAMES: usize = 64;

struct Entry {
    root: B256,
    /// `None` once mined.
    transactions: Vec<Option<QueuedTx>>,
    left: usize,
    admitted: Instant,
}

/// A transaction a canonical block took out of the queue.
struct Taken {
    sequence: u64,
    root: B256,
    position: u32,
    /// Transactions in the frame, to rebuild an entry that was used up.
    frame_len: u32,
    admitted: Instant,
    tx: QueuedTx,
}

/// Identifies a block's transaction list without hashing it: the count and
/// the first and last transaction hash. Header fields are not used because
/// the proposal header is finalized after the block is built.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BlockMark {
    count: usize,
    first: B256,
    last: B256,
}

impl BlockMark {
    fn of(hashes: &[B256]) -> Self {
        Self {
            count: hashes.len(),
            first: hashes.first().copied().unwrap_or_default(),
            last: hashes.last().copied().unwrap_or_default(),
        }
    }
}

#[derive(Default)]
struct Inner {
    /// Frames by arrival sequence.
    entries: BTreeMap<u64, Entry>,
    by_root: B256HashMap<u64>,
    next: u64,
    /// Transactions not yet mined.
    pending: usize,
    /// Highest canonical block applied.
    tip: u64,
    /// Mined transactions that no held frame named, by block.
    early: VecDeque<(u64, Vec<B256>)>,
    early_set: B256HashSet,
    /// What the latest canonical blocks took, by block number.
    recent: VecDeque<(u64, Vec<Taken>)>,
    /// Blocks already applied, by number, with a mark of their content. A
    /// block this node built is applied when it is built; its canonical
    /// notification then finds the same mark and has nothing left to do.
    applied_marks: VecDeque<(u64, BlockMark)>,
}

/// Frames waiting to be built, in arrival order.
pub struct FrameQueue {
    inner: Mutex<Inner>,
    applied: Condvar,
    time_to_live: Duration,
}

impl std::fmt::Debug for FrameQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameQueue").finish_non_exhaustive()
    }
}

impl FrameQueue {
    /// A queue that drops a frame not used up within `time_to_live`.
    pub fn new(time_to_live: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            applied: Condvar::new(),
            time_to_live,
        }
    }

    /// The process-wide queue; `N42_FRAME_QUEUE_TTL_SECS` (default 120).
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<FrameQueue> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let seconds = std::env::var("N42_FRAME_QUEUE_TTL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(120);
            Self::new(Duration::from_secs(seconds.max(1)))
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a frame the caller has just put into `store`, with its
    /// transactions in frame order. A transaction that `store` files under
    /// another frame, or that a block already mined, is left out. Returns the
    /// number queued.
    pub fn push(&self, store: &FrameStore, root: B256, transactions: Vec<QueuedTx>) -> usize {
        let hashes: Vec<B256> = transactions.iter().map(|tx| *tx.hash()).collect();
        let mut inner = self.lock();
        // Located under the queue lock: `mark_mined` locates under the same
        // lock, so one of the two always sees the other's effect.
        let located = store.locate_all(&hashes);
        if inner.by_root.contains_key(&root) {
            return 0;
        }
        let now = Instant::now();
        while let Some(first) = inner.entries.first_entry() {
            if now.duration_since(first.get().admitted) < self.time_to_live {
                break;
            }
            let expired = first.remove();
            inner.by_root.remove(&expired.root);
            inner.pending -= expired.left;
            metrics::counter!("n42_frame_queue_expired_transactions_total")
                .increment(expired.left as u64);
        }
        let mut left = 0;
        let transactions: Vec<Option<QueuedTx>> = transactions
            .into_iter()
            .zip(hashes.iter().zip(located))
            .enumerate()
            .map(|(position, (tx, (hash, located)))| {
                let here = located == Some((root, position as u32));
                (here && !inner.early_set.contains(hash)).then(|| {
                    left += 1;
                    tx
                })
            })
            .collect();
        if left == 0 {
            return 0;
        }
        let sequence = inner.next;
        inner.next += 1;
        inner.pending += left;
        inner.by_root.insert(root, sequence);
        inner.entries.insert(
            sequence,
            Entry {
                root,
                transactions,
                left,
                admitted: now,
            },
        );
        left
    }

    /// Marks a canonical block's transactions as mined and records the block
    /// as applied. Blocks must be reported in chain order.
    pub fn mark_mined(&self, store: &FrameStore, number: u64, hashes: &[B256]) {
        let mark = BlockMark::of(hashes);
        let mut inner = self.lock();
        match inner
            .applied_marks
            .iter()
            .rev()
            .find(|(applied, _)| *applied == number)
            .map(|(_, applied)| *applied == mark)
        {
            // Applied when it was built.
            Some(true) => return,
            // Another block took this height: give back what the first took.
            Some(false) => {
                Self::revert_locked(&mut inner, number);
                metrics::counter!("n42_frame_queue_replaced_blocks_total").increment(1);
            }
            None => {}
        }
        inner.applied_marks.push_back((number, mark));
        while inner.applied_marks.len() > RECENT_BLOCKS {
            inner.applied_marks.pop_front();
        }
        let located = store.locate_all(hashes);
        let mut early = Vec::new();
        let mut taken = Vec::new();
        for (hash, located) in hashes.iter().zip(located) {
            let Some((root, position)) = located else {
                early.push(*hash);
                continue;
            };
            let Some(sequence) = inner.by_root.get(&root).copied() else {
                // Held by the store but not queued yet: the frame's push is
                // about to run and must not queue this transaction.
                early.push(*hash);
                continue;
            };
            let Some(entry) = inner.entries.get_mut(&sequence) else {
                continue;
            };
            let Some(tx) = entry
                .transactions
                .get_mut(position as usize)
                .and_then(Option::take)
            else {
                continue;
            };
            taken.push(Taken {
                sequence,
                root,
                position,
                frame_len: entry.transactions.len() as u32,
                admitted: entry.admitted,
                tx,
            });
            entry.left -= 1;
            let used_up = entry.left == 0;
            inner.pending -= 1;
            if used_up {
                inner.entries.remove(&sequence);
                inner.by_root.remove(&root);
            }
        }
        inner.early_set.extend(early.iter().copied());
        inner.early.push_back((number, early));
        inner.recent.push_back((number, taken));
        while inner.recent.len() > RECENT_BLOCKS {
            inner.recent.pop_front();
        }
        while inner.early.len() > EARLY_BLOCKS {
            if let Some((_, old)) = inner.early.pop_front() {
                for hash in old {
                    inner.early_set.remove(&hash);
                }
            }
        }
        inner.tip = inner.tip.max(number);
        drop(inner);
        self.applied.notify_all();
    }

    /// Puts back what canonical block `number` took, after a reorg removed
    /// that block from the chain. Only the latest blocks can be put back.
    pub fn revert_mined(&self, number: u64) {
        let mut inner = self.lock();
        Self::revert_locked(&mut inner, number);
    }

    fn revert_locked(inner: &mut Inner, number: u64) {
        inner.applied_marks.retain(|(applied, _)| *applied != number);
        let mut restored = 0usize;
        while let Some(at) = inner.recent.iter().rposition(|(n, _)| *n == number) {
            let Some((_, taken)) = inner.recent.remove(at) else {
                break;
            };
            for taken in taken {
                let entry = inner
                    .entries
                    .entry(taken.sequence)
                    .or_insert_with(|| Entry {
                        root: taken.root,
                        transactions: vec![None; taken.frame_len as usize],
                        left: 0,
                        admitted: taken.admitted,
                    });
                inner.by_root.insert(taken.root, taken.sequence);
                if let Some(slot) = entry.transactions.get_mut(taken.position as usize)
                    && slot.is_none()
                {
                    *slot = Some(taken.tx);
                    entry.left += 1;
                    restored += 1;
                }
            }
        }
        inner.pending += restored;
        while let Some(at) = inner.early.iter().rposition(|(n, _)| *n == number) {
            if let Some((_, hashes)) = inner.early.remove(at) {
                for hash in hashes {
                    inner.early_set.remove(&hash);
                }
            }
        }
        metrics::counter!("n42_frame_queue_restored_transactions_total").increment(restored as u64);
    }

    /// Waits until block `number` has been applied, the queue is empty, or
    /// `timeout` passes. A builder calls this for its parent so it does not
    /// offer transactions the parent already included.
    pub fn wait_applied(&self, number: u64, timeout: Duration) -> bool {
        let inner = self.lock();
        let (inner, _) = self
            .applied
            .wait_timeout_while(inner, timeout, |inner| {
                inner.tip < number && inner.pending > 0
            })
            .unwrap_or_else(PoisonError::into_inner);
        inner.tip >= number || inner.pending == 0
    }

    /// Transactions not yet mined.
    pub fn pending(&self) -> usize {
        self.lock().pending
    }

    /// Unmined transactions of up to `WALK_FRAMES` frames from `cursor` on,
    /// and the cursor for the next call. Empty when no frame is left.
    fn walk(&self, cursor: u64) -> (Vec<QueuedTx>, u64) {
        let inner = self.lock();
        let mut transactions = Vec::new();
        let mut next = cursor;
        for (sequence, entry) in inner.entries.range(cursor..).take(WALK_FRAMES) {
            transactions.extend(entry.transactions.iter().flatten().cloned());
            next = sequence + 1;
        }
        (transactions, next)
    }
}

type Rest = Box<dyn BestTransactions<Item = QueuedTx>>;

/// Queued frames in arrival order, then the pool's own order.
///
/// `rest` builds the pool iterator; it is called only once the queue is
/// used up.
pub struct QueuedFrameTransactions<R> {
    queue: &'static FrameQueue,
    cursor: u64,
    current: VecDeque<QueuedTx>,
    rest_factory: Option<R>,
    rest: Option<Rest>,
    invalid_senders: HashSet<Address>,
    skip_blobs: bool,
    no_updates: bool,
}

impl<R: FnOnce() -> Rest> QueuedFrameTransactions<R> {
    pub fn new(queue: &'static FrameQueue, rest: R) -> Self {
        Self {
            queue,
            cursor: 0,
            current: VecDeque::new(),
            rest_factory: Some(rest),
            rest: None,
            invalid_senders: HashSet::new(),
            skip_blobs: false,
            no_updates: false,
        }
    }

    fn next_from_rest(&mut self) -> Option<QueuedTx> {
        if self.rest.is_none() {
            let mut rest = (self.rest_factory.take()?)();
            rest.set_skip_blobs(self.skip_blobs);
            if self.no_updates {
                rest.no_updates();
            }
            self.rest = Some(rest);
        }
        self.rest.as_mut()?.next()
    }
}

impl<R: FnOnce() -> Rest> Iterator for QueuedFrameTransactions<R> {
    type Item = QueuedTx;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while let Some(tx) = self.current.pop_front() {
                if !self.invalid_senders.contains(tx.sender_ref()) {
                    return Some(tx);
                }
            }
            let (transactions, cursor) = self.queue.walk(self.cursor);
            if cursor == self.cursor {
                break;
            }
            self.cursor = cursor;
            self.current.extend(transactions);
        }
        loop {
            let tx = self.next_from_rest()?;
            if !self.invalid_senders.contains(tx.sender_ref()) {
                return Some(tx);
            }
        }
    }
}

impl<R: FnOnce() -> Rest + Send> BestTransactions for QueuedFrameTransactions<R> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_consensus::{TxLegacy, transaction::Recovered};
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use n42_tx_types::N42TxEnvelope;
    use reth_transaction_pool::{
        PoolTransaction, TransactionOrigin,
        identifier::{SenderId, TransactionId},
    };

    pub(crate) fn tx(sender: u8, nonce: u64) -> QueuedTx {
        let signed = alloy_consensus::SignableTransaction::into_signed(
            TxLegacy {
                chain_id: Some(1),
                nonce,
                gas_price: 1,
                gas_limit: 21_000,
                to: TxKind::Call(Address::repeat_byte(0xee)),
                value: U256::from(1),
                input: Bytes::new(),
            },
            Signature::new(U256::from(sender), U256::from(nonce + 1), false),
        );
        let from = Address::repeat_byte(sender);
        Arc::new(ValidPoolTransaction {
            transaction: N42PooledTransaction::try_from_consensus(Recovered::new_unchecked(
                N42TxEnvelope::from(alloy_consensus::EthereumTxEnvelope::<
                    alloy_consensus::TxEip4844,
                >::Legacy(signed)),
                from,
            ))
            .unwrap(),
            transaction_id: TransactionId::new(SenderId::from(sender as u64), nonce),
            propagate: false,
            timestamp: Instant::now(),
            origin: TransactionOrigin::External,
            authority_ids: None,
        })
    }

    fn admit(store: &FrameStore, queue: &FrameQueue, seed: u8, txs: &[QueuedTx]) -> B256 {
        let root = B256::repeat_byte(seed);
        let hashes: Vec<B256> = txs.iter().map(|tx| *tx.hash()).collect();
        let bytes = hashes
            .iter()
            .map(|h| Bytes::copy_from_slice(&h[..]))
            .collect();
        store.insert(root, bytes, hashes);
        queue.push(store, root, txs.to_vec());
        root
    }

    fn leak(queue: FrameQueue) -> &'static FrameQueue {
        Box::leak(Box::new(queue))
    }

    fn drain(queue: &'static FrameQueue) -> Vec<B256> {
        QueuedFrameTransactions::new(queue, || {
            reth_transaction_pool::noop::NoopTransactionPool::<N42PooledTransaction>::new()
                .best_transactions()
        })
        .map(|tx| *tx.hash())
        .collect()
    }

    use reth_transaction_pool::TransactionPool;

    #[test]
    fn frames_are_offered_in_arrival_order_until_mined() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let first = [tx(1, 0), tx(1, 1)];
        let second = [tx(2, 0)];
        admit(&store, queue, 1, &first);
        admit(&store, queue, 2, &second);
        assert_eq!(queue.pending(), 3);
        assert_eq!(
            drain(queue),
            vec![*first[0].hash(), *first[1].hash(), *second[0].hash()]
        );

        queue.mark_mined(&store, 1, &[*first[0].hash(), *second[0].hash()]);
        assert_eq!(queue.pending(), 1);
        assert_eq!(drain(queue), vec![*first[1].hash()]);

        // Marking twice changes nothing.
        queue.mark_mined(&store, 2, &[*first[0].hash()]);
        assert_eq!(queue.pending(), 1);
        queue.mark_mined(&store, 3, &[*first[1].hash()]);
        assert_eq!(queue.pending(), 0);
        assert!(drain(queue).is_empty());
    }

    #[test]
    fn a_reorged_block_gives_its_transactions_back_in_their_place() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let first = [tx(1, 0), tx(1, 1)];
        let second = [tx(2, 0)];
        admit(&store, queue, 1, &first);
        admit(&store, queue, 2, &second);

        // Block 5 uses up the first frame; block 6 takes the second.
        queue.mark_mined(&store, 5, &[*first[0].hash(), *first[1].hash()]);
        queue.mark_mined(&store, 6, &[*second[0].hash()]);
        assert_eq!(queue.pending(), 0);

        queue.revert_mined(6);
        queue.revert_mined(5);
        assert_eq!(queue.pending(), 3);
        assert_eq!(
            drain(queue),
            vec![*first[0].hash(), *first[1].hash(), *second[0].hash()]
        );

        // Reverting again, or a block that took nothing, changes nothing.
        queue.revert_mined(5);
        queue.revert_mined(9);
        assert_eq!(queue.pending(), 3);

        // The block that replaces it takes them as usual.
        queue.mark_mined(&store, 5, &[*first[0].hash()]);
        assert_eq!(queue.pending(), 2);
    }

    #[test]
    fn a_block_applied_when_built_is_not_applied_again_and_can_be_replaced() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let first = [tx(1, 0), tx(1, 1)];
        let second = [tx(2, 0)];
        admit(&store, queue, 1, &first);
        admit(&store, queue, 2, &second);

        // The builder applies block 5 itself; the next build need not wait.
        let built = [*first[0].hash(), *first[1].hash()];
        queue.mark_mined(&store, 5, &built);
        assert!(queue.wait_applied(5, Duration::ZERO));
        assert_eq!(queue.pending(), 1);

        // Its canonical notification changes nothing.
        queue.mark_mined(&store, 5, &built);
        assert_eq!(queue.pending(), 1);

        // Another block at that height gives back what the first took.
        queue.mark_mined(&store, 5, &[*second[0].hash()]);
        assert_eq!(queue.pending(), 2);
        assert_eq!(drain(queue), vec![*first[0].hash(), *first[1].hash()]);
    }

    #[test]
    fn a_frame_that_arrives_after_its_block_is_not_queued() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let late = [tx(3, 0), tx(3, 1)];
        queue.mark_mined(&store, 1, &[*late[0].hash()]);
        admit(&store, queue, 3, &late);
        assert_eq!(drain(queue), vec![*late[1].hash()]);
    }

    #[test]
    fn a_transaction_filed_under_an_earlier_frame_is_queued_once() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let shared = tx(4, 0);
        admit(&store, queue, 4, std::slice::from_ref(&shared));
        admit(&store, queue, 5, &[shared.clone(), tx(4, 1)]);
        assert_eq!(queue.pending(), 2);
        assert_eq!(drain(queue).len(), 2);
    }

    #[test]
    fn frames_past_their_time_to_live_are_dropped_on_the_next_push() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::ZERO));
        admit(&store, queue, 6, &[tx(6, 0)]);
        let kept = [tx(7, 0)];
        admit(&store, queue, 7, &kept);
        assert_eq!(queue.pending(), 1);
        assert_eq!(drain(queue), vec![*kept[0].hash()]);
    }

    #[test]
    fn a_builder_waits_for_its_parent_only_while_transactions_are_queued() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        // Nothing queued: no reason to wait.
        assert!(queue.wait_applied(9, Duration::from_millis(1)));
        admit(&store, queue, 8, &[tx(8, 0)]);
        assert!(!queue.wait_applied(9, Duration::from_millis(1)));
        queue.mark_mined(&store, 9, &[]);
        assert!(queue.wait_applied(9, Duration::from_millis(1)));
    }

    #[test]
    fn a_sender_marked_invalid_is_skipped_for_the_rest_of_the_build() {
        let store = FrameStore::new(16);
        let queue = leak(FrameQueue::new(Duration::from_secs(60)));
        let txs = [tx(9, 0), tx(9, 1), tx(10, 0)];
        admit(&store, queue, 9, &txs);
        let mut best = QueuedFrameTransactions::new(queue, || {
            reth_transaction_pool::noop::NoopTransactionPool::<N42PooledTransaction>::new()
                .best_transactions()
        });
        let first = best.next().unwrap();
        best.mark_invalid(
            &first,
            InvalidPoolTransactionError::Consensus(
                reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
            ),
        );
        assert_eq!(best.next().map(|tx| *tx.hash()), Some(*txs[2].hash()));
        assert!(best.next().is_none());
    }
}
