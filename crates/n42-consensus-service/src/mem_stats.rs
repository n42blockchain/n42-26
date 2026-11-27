//! Sizes of this crate's long-lived in-memory structures, read by the
//! node's periodic `N42_MEM` line. Each structure stores its own figures
//! when it changes; reading them takes no lock.

use std::sync::atomic::AtomicU64;

/// Entries in the orchestrator's committed-block ring (sync serving).
pub static COMMITTED_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Block-data bytes the committed-block ring keeps alive.
pub static COMMITTED_BLOCK_BYTES: AtomicU64 = AtomicU64::new(0);
