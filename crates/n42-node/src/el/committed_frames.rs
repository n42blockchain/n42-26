//! Takes the transactions of committed blocks out of the frame queue on a
//! node that does not execute them.
//!
//! An executing node marks a block's transactions mined when its engine
//! makes the block canonical. A key that defers execution to the owner of
//! its machine has no canonical chain of its own: without this its queue
//! only grows, the ingest gate closes at its high-water mark, and frames
//! sent to it are only held for reference from then on.
//!
//! The transactions of each block the key imports are kept by block hash.
//! When a forkchoice update names a committed head, that head and the kept
//! ancestors not yet applied are marked mined, in chain order; kept blocks
//! at or below the head's height are dropped then.

use alloy_primitives::{B256, keccak256};
use alloy_rpc_types_engine::ExecutionData;
use n42_tx_types::FrameStore;
use std::collections::VecDeque;
use std::sync::Mutex;

use crate::frame_queue::FrameQueue;

/// Imported blocks kept until a committed head covers them.
const KEPT: usize = 64;

/// An imported block and its transaction hashes.
#[derive(Debug, Clone)]
pub(super) struct ImportedBlock {
    hash: B256,
    parent: B256,
    number: u64,
    transactions: Vec<B256>,
}

impl ImportedBlock {
    /// The block a payload carries. A transaction's hash is the keccak of
    /// its EIP-2718 encoding, which is how the payload carries it.
    pub(super) fn of(payload: &ExecutionData) -> Self {
        use rayon::prelude::*;
        let transactions = payload.payload.transactions();
        let transactions = if transactions.len() >= 4096 {
            transactions
                .par_iter()
                .with_min_len(1024)
                .map(keccak256)
                .collect()
        } else {
            transactions.iter().map(keccak256).collect()
        };
        Self {
            hash: payload.block_hash(),
            parent: payload.parent_hash(),
            number: payload.block_number(),
            transactions,
        }
    }
}

/// Imported blocks whose transactions are still queued.
#[derive(Debug, Default)]
pub(super) struct CommittedFrames {
    imported: Mutex<VecDeque<ImportedBlock>>,
}

impl CommittedFrames {
    /// Keeps `block` until a committed head covers it.
    pub(super) fn imported(&self, block: ImportedBlock) {
        let mut imported = self.imported.lock().unwrap_or_else(|e| e.into_inner());
        imported.retain(|kept| kept.hash != block.hash);
        imported.push_back(block);
        while imported.len() > KEPT {
            imported.pop_front();
        }
    }

    /// Marks committed `head` and its kept ancestors mined in `queue`, oldest
    /// first. Returns the number of blocks marked.
    pub(super) fn committed(&self, queue: &FrameQueue, store: &FrameStore, head: B256) -> usize {
        // Held while marking, so heads committed one after another are
        // marked in chain order even when reported concurrently.
        let mut imported = self.imported.lock().unwrap_or_else(|e| e.into_inner());
        let Some(number) = imported.iter().find(|kept| kept.hash == head).map(|b| b.number) else {
            return 0;
        };
        let mut chain = Vec::new();
        let mut next = head;
        while let Some(at) = imported.iter().position(|kept| kept.hash == next) {
            let block = imported.remove(at).expect("position is in range");
            next = block.parent;
            chain.push(block);
        }
        // Siblings of the committed chain at or below its head are never
        // committed now.
        imported.retain(|kept| kept.number > number);
        let blocks = chain.len();
        for block in chain.into_iter().rev() {
            queue.mark_mined(store, block.number, &block.transactions);
        }
        metrics::counter!("n42_frame_queue_committed_blocks_total").increment(blocks as u64);
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;

    fn block(hash: u8, parent: u8, number: u64, transactions: &[B256]) -> ImportedBlock {
        ImportedBlock {
            hash: B256::repeat_byte(hash),
            parent: B256::repeat_byte(parent),
            number,
            transactions: transactions.to_vec(),
        }
    }

    #[test]
    fn a_committed_head_drains_its_chain_and_drops_its_siblings() {
        let store = FrameStore::new(16);
        let queue = FrameQueue::new(std::time::Duration::from_secs(60));
        let queued: Vec<_> = (0..4).map(|nonce| crate::frame_queue::tests::tx(1, nonce)).collect();
        let hashes: Vec<B256> = queued.iter().map(|tx| *tx.hash()).collect();
        let root = B256::repeat_byte(0x77);
        let bytes = hashes.iter().map(|h| Bytes::copy_from_slice(&h[..])).collect();
        assert!(store.insert(root, bytes, hashes.clone()));
        assert_eq!(queue.push(&store, root, queued), 4);

        let frames = CommittedFrames::default();
        frames.imported(block(1, 0, 1, &hashes[..2]));
        frames.imported(block(2, 1, 2, &hashes[2..3]));
        // A sibling of block 2 that is not committed.
        frames.imported(block(3, 1, 2, &hashes[3..]));
        // An unknown head marks nothing.
        assert_eq!(frames.committed(&queue, &store, B256::repeat_byte(9)), 0);
        assert_eq!(queue.pending(), 4);

        assert_eq!(frames.committed(&queue, &store, B256::repeat_byte(2)), 2);
        assert_eq!(queue.pending(), 1, "the sibling's transaction stays queued");
        assert!(queue.wait_applied(2, std::time::Duration::ZERO));
        // Nothing is marked twice, and the sibling was dropped.
        assert_eq!(frames.committed(&queue, &store, B256::repeat_byte(3)), 0);
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn a_payload_names_its_transactions_by_the_hash_of_their_encoding() {
        let tx = Bytes::from(vec![0x50, 1, 2, 3]);
        let payload = ExecutionData::new(
            alloy_rpc_types_engine::ExecutionPayload::V1(
                alloy_rpc_types_engine::ExecutionPayloadV1 {
                    parent_hash: B256::repeat_byte(1),
                    fee_recipient: Default::default(),
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number: 7,
                    gas_limit: 1,
                    gas_used: 0,
                    timestamp: 1,
                    extra_data: Bytes::new(),
                    base_fee_per_gas: Default::default(),
                    block_hash: B256::repeat_byte(2),
                    transactions: vec![tx.clone()],
                },
            ),
            alloy_rpc_types_engine::ExecutionPayloadSidecar::none(),
        );
        let block = ImportedBlock::of(&payload);
        assert_eq!(block.transactions, vec![keccak256(&tx)]);
        assert_eq!((block.hash, block.parent, block.number), (B256::repeat_byte(2), B256::repeat_byte(1), 7));
    }
}
