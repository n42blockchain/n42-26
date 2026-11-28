//! Sizes of this crate's long-lived in-memory structures, read by the
//! node's periodic `N42_MEM` line. Each structure stores its own figures
//! when it changes; reading them takes no lock.

use std::sync::atomic::AtomicU64;

/// Blocks in the Gov5 block-by-hash serve cache.
pub static SERVED_GOV5_BLOCKS: AtomicU64 = AtomicU64::new(0);
/// Body bytes the Gov5 block-by-hash serve cache holds.
pub static SERVED_GOV5_BLOCK_BYTES: AtomicU64 = AtomicU64::new(0);
