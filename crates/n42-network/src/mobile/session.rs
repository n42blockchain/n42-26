use alloy_primitives::B256;
use std::collections::HashSet;
use std::time::Instant;

/// Represents an active QUIC session with a mobile verifier.
///
/// Each connected phone maintains a session on the IDC node, tracking
/// the phone's identity, connection state, and cached bytecode inventory.
#[derive(Debug)]
pub struct MobileSession {
    /// Unique session identifier (derived from QUIC connection ID).
    pub session_id: u64,
    /// Ed25519 public key of the mobile verifier.
    pub verifier_pubkey: [u8; 32],
    /// When the phone connected.
    pub connected_at: Instant,
    /// Last activity timestamp (updated on message send/receive).
    pub last_active: Instant,
    /// Set of code hashes the phone has cached.
    /// Used to determine which bytecodes to include in verification packets.
    pub cached_code_hashes: HashSet<B256>,
    /// Number of verification packets sent to this phone.
    pub packets_sent: u64,
    /// Number of verification receipts received from this phone.
    pub receipts_received: u64,
}

impl MobileSession {
    /// Creates a new mobile session.
    pub fn new(session_id: u64, verifier_pubkey: [u8; 32]) -> Self {
        let now = Instant::now();
        Self {
            session_id,
            verifier_pubkey,
            connected_at: now,
            last_active: now,
            cached_code_hashes: HashSet::new(),
            packets_sent: 0,
            receipts_received: 0,
        }
    }

    /// Marks the session as recently active.
    pub fn touch(&mut self) {
        self.last_active = Instant::now();
    }

    /// Returns the session duration.
    pub fn duration(&self) -> std::time::Duration {
        self.connected_at.elapsed()
    }

    /// Returns time since last activity.
    pub fn idle_duration(&self) -> std::time::Duration {
        self.last_active.elapsed()
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
