//! Verifier context and statistics types.

use n42_mobile::code_cache::CodeCache;
use n42_primitives::BlsSecretKey;
use reth_chainspec::ChainSpec;
use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::warn;

/// Maximum number of pending packets in the receive queue.
pub(crate) const MAX_PENDING_PACKETS: usize = 64;

/// Default code cache capacity (number of contract bytecodes).
pub(crate) const DEFAULT_CODE_CACHE_CAPACITY: usize = 1000;

/// Recovers from a poisoned mutex instead of panicking.
///
/// If a thread panics while holding the mutex, subsequent `.lock()` calls
/// return `Err(PoisonError)`. This helper logs a warning and recovers the
/// inner data, preventing cascading panics across FFI boundaries.
pub(crate) fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        warn!("mutex poisoned, recovering");
        poisoned.into_inner()
    })
}

/// Safely converts `usize` to `c_int`, returning -1 on overflow.
pub(crate) fn safe_cint(val: usize) -> c_int {
    c_int::try_from(val).unwrap_or(-1)
}

/// Statistics tracked across verification sessions.
#[derive(Debug, Default)]
pub(crate) struct VerifyStats {
    pub blocks_verified: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub total_verify_time_ms: u64,
}

/// Information about the last verification (exposed to UI via JSON).
#[derive(Debug, serde::Serialize)]
pub(crate) struct LastVerifyInfo {
    pub block_number: u64,
    pub block_hash: String,
    pub receipts_root_match: bool,
    pub computed_receipts_root: String,
    pub expected_receipts_root: String,
    pub tx_count: usize,
    pub witness_accounts: usize,
    pub uncached_bytecodes: usize,
    pub packet_size_bytes: usize,
    pub verify_time_ms: u64,
    pub signature: String,
}

/// QUIC connection to a StarHub node.
pub(crate) struct QuicConnection {
    pub connection: quinn::Connection,
    pub pending_packets: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Counter for packets dropped due to a full queue.
    pub dropped_count: Arc<AtomicU64>,
    /// Handle to the background receiver task.
    pub _recv_task: tokio::task::JoinHandle<()>,
}

/// The main verifier context, opaque to C callers.
pub struct VerifierContext {
    #[allow(dead_code)]
    pub(crate) chain_id: u64,
    pub(crate) chain_spec: Arc<ChainSpec>,
    pub(crate) signing_key: BlsSecretKey,
    pub(crate) pubkey_bytes: [u8; 48],
    pub(crate) code_cache: Arc<Mutex<CodeCache>>,
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) connection: Mutex<Option<QuicConnection>>,
    pub(crate) stats: Mutex<VerifyStats>,
    pub(crate) last_info: Mutex<Option<LastVerifyInfo>>,
}
