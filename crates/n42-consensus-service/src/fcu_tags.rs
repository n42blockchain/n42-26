//! Fork-choice tags (E14) for a deferred-execution chain.
//!
//! On a Profile-1 chain the header of block N carries N−1's execution result,
//! so N's own result is only confirmed by its child. The tags a node gives the
//! execution layer are therefore: latest = the committed head N, safe = its
//! parent N−1, finalized = the highest block at or below safe that the
//! execution layer reports persisted. Off Profile 1 every forkchoice update
//! names the committed block three times, as before.
//!
//! A tag that cannot be established from this node's own knowledge (the
//! head's parent is not known here, nothing is persisted yet, or the
//! persisted block is not on the head's chain) is sent as zero, which the
//! Engine API reads as "no new assertion": the execution layer keeps its
//! previous safe/finalized block. Tags never guess.

use std::sync::Arc;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;

/// `(number, parent hash)` of a block, as this node knows it.
pub type BlockLink = (u64, B256);

/// How far a finalized candidate is walked back along parent links. The
/// execution layer persists within a few blocks of its head; a gap larger
/// than this holds finalized rather than walking an unbounded chain.
pub const MAX_FINALIZED_WALK: u64 = 1024;

type LinkSource = Arc<dyn Fn(&B256) -> Option<BlockLink> + Send + Sync>;
type PersistedSource = Arc<dyn Fn() -> Option<(u64, B256)> + Send + Sync>;

/// The sources E14 tags are derived from.
#[derive(Clone)]
pub struct ForkchoiceTags {
    link: LinkSource,
    persisted: PersistedSource,
}

impl std::fmt::Debug for ForkchoiceTags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForkchoiceTags").finish_non_exhaustive()
    }
}

impl ForkchoiceTags {
    /// `link` returns a block's number and parent; `persisted` the highest
    /// block the execution layer has written to disk.
    pub fn new(
        link: impl Fn(&B256) -> Option<BlockLink> + Send + Sync + 'static,
        persisted: impl Fn() -> Option<(u64, B256)> + Send + Sync + 'static,
    ) -> Self {
        Self {
            link: Arc::new(link),
            persisted: Arc::new(persisted),
        }
    }

    /// `(safe, finalized)` for the committed block `committed`.
    pub fn safe_and_finalized(&self, committed: B256) -> (B256, B256) {
        deferred_tags(committed, |hash| (self.link)(hash), (self.persisted)())
    }
}

/// The forkchoice state that makes `head` the execution layer's head, with
/// the safe and finalized tags of the committed block `committed` (usually
/// `head` itself; a pipelined finalize may advance the head past it).
/// Without `tags` (any chain but Profile 1) both tags name `committed`.
pub fn forkchoice_state(
    tags: Option<&ForkchoiceTags>,
    head: B256,
    committed: B256,
) -> ForkchoiceState {
    let (safe_block_hash, finalized_block_hash) = match tags {
        Some(tags) => tags.safe_and_finalized(committed),
        None => (committed, committed),
    };
    ForkchoiceState {
        head_block_hash: head,
        safe_block_hash,
        finalized_block_hash,
    }
}

/// E14 `(safe, finalized)` for the committed head `head`: safe is its parent;
/// finalized is the persisted block if it is at or below safe and on safe's
/// chain, safe itself if the persisted block is above safe and descends from
/// it, else zero. Genesis is its own safe and finalized block.
pub fn deferred_tags(
    head: B256,
    link: impl Fn(&B256) -> Option<BlockLink>,
    persisted: Option<(u64, B256)>,
) -> (B256, B256) {
    let Some((number, parent)) = link(&head) else {
        return (B256::ZERO, B256::ZERO);
    };
    if number == 0 {
        return (head, head);
    }
    let safe = parent;
    let safe_number = number - 1;
    let Some((persisted_number, persisted_hash)) = persisted else {
        return (safe, B256::ZERO);
    };
    let finalized = if persisted_number <= safe_number {
        // The persisted block is final only if it lies on safe's chain.
        (ancestor_at(safe, safe_number, persisted_number, &link) == Some(persisted_hash))
            .then_some(persisted_hash)
    } else {
        // Safe is persisted only if the persisted block descends from it.
        (ancestor_at(persisted_hash, persisted_number, safe_number, &link) == Some(safe))
            .then_some(safe)
    };
    (safe, finalized.unwrap_or(B256::ZERO))
}

/// The ancestor of `hash` (at `number`) at height `target`, by parent links.
fn ancestor_at(
    mut hash: B256,
    mut number: u64,
    target: u64,
    link: &impl Fn(&B256) -> Option<BlockLink>,
) -> Option<B256> {
    if number.checked_sub(target)? > MAX_FINALIZED_WALK {
        return None;
    }
    while number > target {
        let (at, parent) = link(&hash)?;
        if at != number {
            return None;
        }
        hash = parent;
        number -= 1;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A chain 0..=len with hashes `h(i)` and a fork at `fork_at` whose
    /// blocks are `f(i)`.
    fn h(i: u64) -> B256 {
        B256::with_last_byte(i as u8 + 1)
    }
    fn f(i: u64) -> B256 {
        B256::repeat_byte(0xf0) ^ B256::with_last_byte(i as u8)
    }
    fn chain(len: u64, fork_at: u64, fork_len: u64) -> HashMap<B256, BlockLink> {
        let mut links = HashMap::new();
        links.insert(h(0), (0, B256::ZERO));
        for i in 1..=len {
            links.insert(h(i), (i, h(i - 1)));
        }
        for i in fork_at + 1..=fork_at + fork_len {
            let parent = if i == fork_at + 1 { h(fork_at) } else { f(i - 1) };
            links.insert(f(i), (i, parent));
        }
        links
    }

    #[test]
    fn head_safe_parent_finalized_persisted() {
        let links = chain(10, 0, 0);
        let link = |hash: &B256| links.get(hash).copied();
        // Persisted below safe: finalized is the persisted block.
        assert_eq!(deferred_tags(h(10), link, Some((7, h(7)))), (h(9), h(7)));
        // Persisted at safe.
        assert_eq!(deferred_tags(h(10), link, Some((9, h(9)))), (h(9), h(9)));
        // Persisted at the head (persistence threshold 0): finalized is
        // capped at safe, never the head.
        assert_eq!(deferred_tags(h(10), link, Some((10, h(10)))), (h(9), h(9)));
        // Nothing persisted yet: finalized is held.
        assert_eq!(deferred_tags(h(10), link, None), (h(9), B256::ZERO));
    }

    #[test]
    fn block_one_and_genesis() {
        let links = chain(3, 0, 0);
        let link = |hash: &B256| links.get(hash).copied();
        assert_eq!(deferred_tags(h(1), link, Some((0, h(0)))), (h(0), h(0)));
        assert_eq!(deferred_tags(h(1), link, Some((1, h(1)))), (h(0), h(0)));
        assert_eq!(deferred_tags(h(0), link, None), (h(0), h(0)));
    }

    #[test]
    fn a_persisted_block_off_the_head_chain_is_never_finalized() {
        // reth persisted f(6) on a fork from 4; the committed head is h(10).
        let links = chain(10, 4, 4);
        let link = |hash: &B256| links.get(hash).copied();
        assert_eq!(deferred_tags(h(10), link, Some((6, f(6)))), (h(9), B256::ZERO));
        // A fork block above safe does not make safe final either.
        let links = chain(5, 2, 8);
        let link = |hash: &B256| links.get(hash).copied();
        assert_eq!(deferred_tags(h(5), link, Some((9, f(9)))), (h(4), B256::ZERO));
    }

    #[test]
    fn unknown_links_hold_the_tags() {
        let links = chain(10, 0, 0);
        let link = |hash: &B256| links.get(hash).copied();
        assert_eq!(deferred_tags(B256::repeat_byte(9), link, Some((3, h(3)))), (B256::ZERO, B256::ZERO));
        // A gap in the links between safe and the persisted block holds
        // finalized but still names safe.
        let mut gapped = chain(10, 0, 0);
        gapped.remove(&h(6));
        let link = |hash: &B256| gapped.get(hash).copied();
        assert_eq!(deferred_tags(h(10), link, Some((3, h(3)))), (h(9), B256::ZERO));
    }

    #[test]
    fn off_profile1_every_tag_is_the_committed_block() {
        let head = h(5);
        let state = forkchoice_state(None, head, head);
        assert_eq!((state.head_block_hash, state.safe_block_hash, state.finalized_block_hash), (head, head, head));
        let links = chain(6, 0, 0);
        let tags = ForkchoiceTags::new(move |hash| links.get(hash).copied(), || Some((4, h(4))));
        // A pipelined finalize advances the head past the committed block.
        let state = forkchoice_state(Some(&tags), h(6), h(5));
        assert_eq!((state.head_block_hash, state.safe_block_hash, state.finalized_block_hash), (h(6), h(4), h(4)));
    }
}
