// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use alloy_primitives::map::FbBuildHasher;
use std::sync::OnceLock;

/// A cache of Ed25519 verifying keys by their 32 bytes, decompressed once.
///
/// `VerifyingKey::from_bytes` decompresses the point (a square root, ~250
/// field squarings), and the batch verifier was paying it for every
/// signature: on the loop136 profile `pow2k` alone was 19% of an execution
/// layer's CPU with the same few thousand senders signing every block. The
/// key is the sender's, and senders repeat; `N42_ED25519_KEY_CACHE=0` turns
/// the cache off. Bounded by sharding: each of 256 shards keeps at most
/// `KEY_SHARD_CAP` keys and is cleared when full.
const KEY_SHARDS: usize = 256;
const KEY_SHARD_CAP: usize = 1024; // 256 shards x 1024 keys x ~200 B: ~50 MB at most

type KeyShard = std::sync::RwLock<
    std::collections::HashMap<[u8; 32], ed25519_dalek::VerifyingKey, FbBuildHasher<32>>,
>;

fn key_shards() -> Option<&'static [KeyShard]> {
    static SHARDS: OnceLock<Option<Box<[KeyShard]>>> = OnceLock::new();
    SHARDS
        .get_or_init(|| {
            if std::env::var("N42_ED25519_KEY_CACHE").is_ok_and(|v| v == "0") {
                return None;
            }
            Some(
                (0..KEY_SHARDS)
                    .map(|_| std::sync::RwLock::new(Default::default()))
                    .collect(),
            )
        })
        .as_deref()
}

/// The verifying key for `bytes`, from the cache or decompressed and cached;
/// `None` for bytes that are not a point (the caller reports the error).
/// A weak (small-order) key is never cached, so the caller's rejection of it
/// stays on the caller's path.
pub fn verifying_key(bytes: &[u8; 32]) -> Option<ed25519_dalek::VerifyingKey> {
    let Some(shards) = key_shards() else {
        return ed25519_dalek::VerifyingKey::from_bytes(bytes).ok();
    };
    let shard = &shards[bytes[0] as usize];
    if let Some(found) = shard.read().unwrap_or_else(|p| p.into_inner()).get(bytes) {
        return Some(*found);
    }
    let key = ed25519_dalek::VerifyingKey::from_bytes(bytes).ok()?;
    if key.is_weak() {
        return Some(key);
    }
    let mut shard = shard.write().unwrap_or_else(|p| p.into_inner());
    if shard.len() >= KEY_SHARD_CAP {
        shard.clear();
    }
    shard.insert(*bytes, key);
    Some(key)
}
