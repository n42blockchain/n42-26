use alloy_primitives::B256;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Represents an active QUIC session with a mobile verifier.
///
/// Each connected phone maintains a session on the IDC node, tracking
/// the phone's identity, connection state, and cached bytecode inventory.
///
/// P2 optimization: `packets_sent` and `receipts_received` use `AtomicU64`
/// so they can be updated with only a read lock on the sessions HashMap,
/// reducing write lock contention under high concurrency (2500+ phones).
pub struct MobileSession {
    /// Unique session identifier (derived from QUIC connection ID).
    pub session_id: u64,
    /// BLS12-381 public key of the mobile verifier.
    pub verifier_pubkey: [u8; 48],
    /// When the phone connected.
    pub connected_at: Instant,
    /// Last activity timestamp as millis since `connected_at`.
    /// Uses AtomicU64 to allow updates with only a read lock.
    last_active_offset_ms: AtomicU64,
    /// Set of code hashes the phone has cached.
    /// Used to determine which bytecodes to include in verification packets.
    pub cached_code_hashes: HashSet<B256>,
    /// Number of verification packets sent to this phone.
    pub packets_sent: AtomicU64,
    /// Number of verification receipts received from this phone.
    pub receipts_received: AtomicU64,
}

impl std::fmt::Debug for MobileSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MobileSession")
            .field("session_id", &self.session_id)
            .field("connected_at", &self.connected_at)
            .field("cached_code_hashes", &self.cached_code_hashes.len())
            .field("packets_sent", &self.packets_sent.load(Ordering::Relaxed))
            .field("receipts_received", &self.receipts_received.load(Ordering::Relaxed))
            .finish()
    }
}

impl MobileSession {
    /// Creates a new mobile session.
    pub fn new(session_id: u64, verifier_pubkey: [u8; 48]) -> Self {
        Self {
            session_id,
            verifier_pubkey,
            connected_at: Instant::now(),
            last_active_offset_ms: AtomicU64::new(0),
            cached_code_hashes: HashSet::new(),
            packets_sent: AtomicU64::new(0),
            receipts_received: AtomicU64::new(0),
        }
    }

    /// Marks the session as recently active (lock-free).
    pub fn touch(&self) {
        let offset = self.connected_at.elapsed().as_millis() as u64;
        self.last_active_offset_ms.store(offset, Ordering::Relaxed);
    }

    /// Increments the receipt counter and touches the session (lock-free).
    pub fn record_receipt(&self) {
        self.receipts_received.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Increments the packet counter and touches the session (lock-free).
    pub fn record_packet_sent(&self) {
        self.packets_sent.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    /// Returns the session duration.
    pub fn duration(&self) -> std::time::Duration {
        self.connected_at.elapsed()
    }

    /// Returns time since last activity.
    pub fn idle_duration(&self) -> std::time::Duration {
        let offset_ms = self.last_active_offset_ms.load(Ordering::Relaxed);
        let total_ms = self.connected_at.elapsed().as_millis() as u64;
        std::time::Duration::from_millis(total_ms.saturating_sub(offset_ms))
    }

    /// Updates the phone's cached code hash set.
    pub fn update_cache_inventory(&mut self, code_hashes: HashSet<B256>) {
        self.cached_code_hashes = code_hashes;
    }

    /// Checks if the phone has a specific bytecode cached.
    pub fn has_cached(&self, code_hash: &B256) -> bool {
        self.cached_code_hashes.contains(code_hash)
    }
}
