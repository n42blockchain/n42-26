use alloy_primitives::B256;
use n42_mobile::{AggregatedAttestation, VerifierRegistry};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Cumulative reward points for a single verifier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifierRewardPoints {
    /// Total number of blocks this verifier has attested to.
    pub blocks_attested: u64,
    /// Cumulative reward points (may use weighted scoring in the future).
    pub points: u64,
    /// Block number of the last attestation.
    pub last_attested_block: u64,
}

/// Persisted attestation data for the node.
///
/// Stores:
/// 1. Recent aggregated attestations (ring buffer, bounded)
/// 2. Cumulative reward points per verifier
/// 3. The verifier registry (pubkey ↔ index mapping)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttestationSnapshot {
    /// Version for forward compatibility.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Recent aggregated attestation proofs (newest last).
    pub attestations: VecDeque<AggregatedAttestation>,
    /// Cumulative reward points keyed by hex-encoded pubkey.
    pub reward_points: HashMap<String, VerifierRewardPoints>,
    /// The verifier registry mapping pubkeys to bitfield indices.
    pub registry: VerifierRegistry,
    /// Total number of attestations ever produced.
    pub total_attestations: u64,
}

const SNAPSHOT_VERSION: u32 = 1;
const MAX_STORED_ATTESTATIONS: usize = 1000;

fn default_version() -> u32 {
    SNAPSHOT_VERSION
}

impl Default for AttestationSnapshot {
    fn default() -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            attestations: VecDeque::new(),
            reward_points: HashMap::new(),
            registry: VerifierRegistry::new(),
            total_attestations: 0,
        }
    }
}

/// Manages attestation persistence and reward tracking.
///
/// Wraps an in-memory [`AttestationSnapshot`] with methods to record new
/// attestations, update reward points, and persist/load from disk.
pub struct AttestationStore {
    snapshot: AttestationSnapshot,
    path: PathBuf,
    /// Dirty flag: true when in-memory state has changed since last save.
    dirty: bool,
}

impl AttestationStore {
    /// Creates a new store, loading from disk if the file exists.
    pub fn new(path: PathBuf) -> io::Result<Self> {
        let snapshot = load_attestation_snapshot(&path)?.unwrap_or_default();
        info!(
            attestations = snapshot.attestations.len(),
            verifiers = snapshot.reward_points.len(),
            total = snapshot.total_attestations,
            "attestation store loaded"
        );
        Ok(Self { snapshot, path, dirty: false })
    }

    /// Records a completed aggregated attestation.
    ///
    /// Updates reward points for each participant and appends the attestation
    /// to the ring buffer. Call [`save`] to persist to disk.
    pub fn record_attestation(&mut self, attestation: &AggregatedAttestation) {
        // Update reward points for each participant.
        for (byte_idx, &byte) in attestation.participant_bitfield.iter().enumerate() {
            for bit in 0..8u32 {
                if byte & (1 << bit) != 0 {
                    let index = byte_idx as u32 * 8 + bit;
                    if let Some(pubkey) = self.snapshot.registry.pubkey_at(index) {
                        let key = hex::encode(pubkey);
                        let entry = self
                            .snapshot
                            .reward_points
                            .entry(key)
                            .or_default();
                        entry.blocks_attested += 1;
                        entry.points += 1;
                        entry.last_attested_block = attestation.block_number;
                    }
                }
            }
        }

        // Append to ring buffer.
        if self.snapshot.attestations.len() >= MAX_STORED_ATTESTATIONS {
            self.snapshot.attestations.pop_front();
        }
        self.snapshot.attestations.push_back(attestation.clone());
        self.snapshot.total_attestations += 1;
        self.dirty = true;
    }

    /// Returns a reference to the verifier registry.
    pub fn registry(&self) -> &VerifierRegistry {
        &self.snapshot.registry
    }

    /// Returns a mutable reference to the verifier registry.
    pub fn registry_mut(&mut self) -> &mut VerifierRegistry {
        self.dirty = true;
        &mut self.snapshot.registry
    }

    /// Returns the reward points for a verifier by hex-encoded pubkey.
    pub fn get_reward_points(&self, pubkey_hex: &str) -> Option<&VerifierRewardPoints> {
        self.snapshot.reward_points.get(pubkey_hex)
    }

    /// Returns all reward points.
    pub fn all_reward_points(&self) -> &HashMap<String, VerifierRewardPoints> {
        &self.snapshot.reward_points
    }

    /// Returns the most recent attestation, if any.
    pub fn latest_attestation(&self) -> Option<&AggregatedAttestation> {
        self.snapshot.attestations.back()
    }

    /// Returns the attestation for a specific block hash, if stored.
    pub fn get_attestation(&self, block_hash: &B256) -> Option<&AggregatedAttestation> {
        self.snapshot
            .attestations
            .iter()
            .find(|a| a.block_hash == *block_hash)
    }

    /// Returns the total number of attestations ever produced.
    pub fn total_attestations(&self) -> u64 {
        self.snapshot.total_attestations
    }

    /// Returns summary statistics.
    pub fn stats(&self) -> AttestationStats {
        AttestationStats {
            stored_attestations: self.snapshot.attestations.len(),
            total_attestations: self.snapshot.total_attestations,
            registered_verifiers: self.snapshot.registry.len(),
            verifiers_with_points: self.snapshot.reward_points.len(),
        }
    }

    /// Persists the current state to disk if dirty.
    pub fn save(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        save_attestation_snapshot(&self.path, &self.snapshot)?;
        self.dirty = false;
        Ok(())
    }

    /// Forces a save regardless of dirty flag.
    pub fn force_save(&mut self) -> io::Result<()> {
        self.dirty = true;
        self.save()
    }
}

/// Summary statistics for the attestation store.
#[derive(Debug, Clone)]
pub struct AttestationStats {
    pub stored_attestations: usize,
    pub total_attestations: u64,
    pub registered_verifiers: u32,
    pub verifiers_with_points: usize,
}

/// Atomically saves the attestation snapshot to a JSON file.
fn save_attestation_snapshot(path: &Path, snapshot: &AttestationSnapshot) -> io::Result<()> {
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)
}

/// Loads an attestation snapshot from a JSON file.
fn load_attestation_snapshot(path: &Path) -> io::Result<Option<AttestationSnapshot>> {
    match std::fs::read_to_string(path) {
        Ok(json) => {
            let snapshot: AttestationSnapshot = serde_json::from_str(&json)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            if snapshot.version > SNAPSHOT_VERSION {
                warn!(
                    snapshot_version = snapshot.version,
                    supported_version = SNAPSHOT_VERSION,
                    "attestation snapshot has newer version; loading anyway"
                );
            }

            Ok(Some(snapshot))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("n42-test-attestation-{name}"))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn make_mock_attestation(block_number: u64) -> AggregatedAttestation {
        use n42_primitives::BlsSecretKey;

        let sk = BlsSecretKey::key_gen(&[block_number as u8; 32]).unwrap();
        let sig = sk.sign(b"mock");

        AggregatedAttestation {
            block_hash: B256::from([block_number as u8; 32]),
            block_number,
            receipts_root: B256::from([0xAA; 32]),
            aggregate_signature: sig,
            participant_bitfield: vec![0b00000111], // indices 0, 1, 2
            participant_count: 3,
            created_at_ms: 1_000_000,
        }
    }

    #[test]
    fn test_new_store_empty() {
        let path = temp_path("new-empty.json");
        cleanup(&path);

        let store = AttestationStore::new(path.clone()).unwrap();
        assert_eq!(store.total_attestations(), 0);
        assert!(store.latest_attestation().is_none());

        cleanup(&path);
    }

    #[test]
    fn test_record_and_save() {
        let path = temp_path("record-save.json");
        cleanup(&path);

        {
            let mut store = AttestationStore::new(path.clone()).unwrap();

            // Register verifiers.
            store.registry_mut().register([0x01; 48]);
            store.registry_mut().register([0x02; 48]);
            store.registry_mut().register([0x03; 48]);

            let att = make_mock_attestation(100);
            store.record_attestation(&att);
            store.save().unwrap();

            assert_eq!(store.total_attestations(), 1);
            assert!(store.latest_attestation().is_some());
            assert_eq!(store.latest_attestation().unwrap().block_number, 100);
        }

        // Reload from disk.
        {
            let store = AttestationStore::new(path.clone()).unwrap();
            assert_eq!(store.total_attestations(), 1);
            assert_eq!(store.latest_attestation().unwrap().block_number, 100);

            // Check reward points.
            let pk1_hex = hex::encode([0x01u8; 48]);
            let points = store.get_reward_points(&pk1_hex).unwrap();
            assert_eq!(points.blocks_attested, 1);
            assert_eq!(points.points, 1);
        }

        cleanup(&path);
    }

    #[test]
    fn test_reward_points_accumulate() {
        let path = temp_path("reward-accumulate.json");
        cleanup(&path);

        let mut store = AttestationStore::new(path.clone()).unwrap();
        store.registry_mut().register([0x01; 48]);
        store.registry_mut().register([0x02; 48]);
        store.registry_mut().register([0x03; 48]);

        for i in 1..=5u64 {
            store.record_attestation(&make_mock_attestation(i));
        }

        let pk1_hex = hex::encode([0x01u8; 48]);
        let points = store.get_reward_points(&pk1_hex).unwrap();
        assert_eq!(points.blocks_attested, 5);
        assert_eq!(points.points, 5);
        assert_eq!(points.last_attested_block, 5);

        cleanup(&path);
    }

    #[test]
    fn test_attestation_ring_buffer() {
        let path = temp_path("ring-buffer.json");
        cleanup(&path);

        let mut store = AttestationStore::new(path.clone()).unwrap();
        store.registry_mut().register([0x01; 48]);

        // Record more than MAX_STORED_ATTESTATIONS.
        for i in 0..MAX_STORED_ATTESTATIONS + 10 {
            store.record_attestation(&make_mock_attestation(i as u64));
        }

        assert_eq!(
            store.snapshot.attestations.len(),
            MAX_STORED_ATTESTATIONS
        );
        assert_eq!(store.total_attestations(), (MAX_STORED_ATTESTATIONS + 10) as u64);

        // Oldest should be evicted.
        assert_eq!(
            store.snapshot.attestations.front().unwrap().block_number,
            10
        );

        cleanup(&path);
    }

    #[test]
    fn test_get_attestation_by_hash() {
        let path = temp_path("get-by-hash.json");
        cleanup(&path);

        let mut store = AttestationStore::new(path.clone()).unwrap();
        store.registry_mut().register([0x01; 48]);

        let att = make_mock_attestation(42);
        let hash = att.block_hash;
        store.record_attestation(&att);

        assert!(store.get_attestation(&hash).is_some());
        assert!(store.get_attestation(&B256::ZERO).is_none());

        cleanup(&path);
    }

    #[test]
    fn test_stats() {
        let path = temp_path("stats.json");
        cleanup(&path);

        let mut store = AttestationStore::new(path.clone()).unwrap();
        store.registry_mut().register([0x01; 48]);
        store.registry_mut().register([0x02; 48]);
        store.registry_mut().register([0x03; 48]);
        store.record_attestation(&make_mock_attestation(1));

        let stats = store.stats();
        assert_eq!(stats.stored_attestations, 1);
        assert_eq!(stats.total_attestations, 1);
        assert_eq!(stats.registered_verifiers, 3);
        assert_eq!(stats.verifiers_with_points, 3);

        cleanup(&path);
    }

    #[test]
    fn test_save_not_dirty_skips() {
        let path = temp_path("not-dirty.json");
        cleanup(&path);

        let mut store = AttestationStore::new(path.clone()).unwrap();
        // No changes made — save should skip.
        store.save().unwrap();
        assert!(!path.exists(), "no file should be created when nothing is dirty");

        cleanup(&path);
    }
}
