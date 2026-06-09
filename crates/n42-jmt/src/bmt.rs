//! SBMT engine + proof types, re-exported from the zero-dependency
//! [`n42_bmt_core`] crate.
//!
//! The implementation lives in `n42-bmt-core` (pure blake3, no reth/mdbx) so
//! that `n42-mobile` / FFI can verify SBMT proofs without the storage stack.
//! This module preserves the `n42_jmt::bmt::*` path for existing callers.

pub use n42_bmt_core::{
    BmtProof, BmtVerifyError, EMPTY_HASH, Hash, SHARD_COUNT, Sbmt, ShardedBmtProof, hash_value,
    shard_tree_path, shard_tree_root, shard_tree_root_from_path,
};
