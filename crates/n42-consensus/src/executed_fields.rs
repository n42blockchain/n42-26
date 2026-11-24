//! What a block's execution produced, by block hash, for deferred execution
//! (Profile 1, A10/AM25; `deferred-execution-design.md` section 3.1).
//!
//! Header N carries N−1's execution result in five fields: stateRoot (the
//! QMDB root after N−1), receiptsRoot (gov5's native receipts root),
//! logsBloom, gasUsed (the block's gas, under EIP-8037 `max(regular,
//! state)`, not the receipts' cumulative) and requestsHash (N−1's EIP-7685
//! requests hash). A leader needs result(parent) to assemble a header and a
//! validator needs it to check one; both read it here. Every path that
//! executes a block — the leader's build, the engine's state-root job —
//! files the block's result under its hash.
//!
//! The registry is in memory (the last [`KEEP`] blocks) and, once a sidecar
//! is installed, also in a durable append-only log: one fixed-size record per
//! block, each with its own checksum, fsynced before [`ExecutedFieldsRegistry::insert`]
//! returns. That is before the block's first forkchoice update, so a restart
//! finds the result of every block reth may have persisted. A torn tail
//! record (a crash mid-append) is cut off on open. The log is bound to the
//! chain's genesis hash and compacted to its last [`KEEP_DURABLE`] records.
//!
//! Semantics follow n42-rs `qmdb-reth/src/executed_fields.rs`; the
//! persistence is new. Unlike n42-rs there are no partial entries (both
//! halves are filed together by the state-root job) and no reading of fields
//! from a descendant's header on a voting node.

use alloy_primitives::{B256, Bloom};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A block's execution result, as its child's header carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutedFields {
    /// The state root after the block (the QMDB root on this chain).
    pub state_root: B256,
    /// gov5's native receipts root of the block's receipts.
    pub receipts_root: B256,
    /// The OR of the block's receipt blooms.
    pub logs_bloom: Bloom,
    /// The block's gas: the execution result's `gas_used`
    /// (EIP-8037 `max(regular, state)`).
    pub gas_used: u64,
    /// The block's EIP-7685 requests hash (`sha256("")` without requests).
    pub requests_hash: B256,
}

impl ExecutedFields {
    /// The five carried fields as a header holds them. For genesis these
    /// are its own result (A6: the alloc's QMDB root, the empty receipts
    /// root, a zero bloom, no gas); for any later header they are its
    /// parent's. A header without a requests hash reads as `sha256("")`.
    pub fn carried_by(header: &alloy_consensus::Header) -> Self {
        Self {
            state_root: header.state_root,
            receipts_root: header.receipts_root,
            logs_bloom: header.logs_bloom,
            gas_used: header.gas_used,
            requests_hash: header
                .requests_hash
                .unwrap_or(n42_chainspec::profile1::PROFILE1_REQUESTS_HASH),
        }
    }

    /// Names the first field in which `self` (what a header carries) and
    /// `expected` differ, for logs and refusals.
    pub fn first_difference(&self, expected: &Self) -> Option<&'static str> {
        if self.state_root != expected.state_root {
            Some("stateRoot")
        } else if self.receipts_root != expected.receipts_root {
            Some("receiptsRoot")
        } else if self.logs_bloom != expected.logs_bloom {
            Some("logsBloom")
        } else if self.gas_used != expected.gas_used {
            Some("gasUsed")
        } else if self.requests_hash != expected.requests_hash {
            Some("requestsHash")
        } else {
            None
        }
    }
}

/// Blocks kept in memory: a validator needs the parent, a leader the
/// parent, a restart the persisted head; a few views of forks besides.
pub const KEEP: usize = 256;

/// Records the durable log is compacted to.
pub const KEEP_DURABLE: usize = 1024;

/// The log is rewritten to [`KEEP_DURABLE`] records once it holds this many.
const COMPACT_AT: usize = 4 * KEEP_DURABLE;

const MAGIC: &[u8; 8] = b"N42EXF01";
const FILE_HEADER_LEN: usize = MAGIC.len() + 32;
/// hash, state root, receipts root, bloom, gas used (BE), requests hash.
const BODY_LEN: usize = 32 + 32 + 32 + 256 + 8 + 32;
const CHECKSUM_LEN: usize = 8;
/// One record on disk.
pub const RECORD_LEN: usize = BODY_LEN + CHECKSUM_LEN;

/// Why the registry refused an entry or its log.
#[derive(Debug, thiserror::Error)]
pub enum ExecutedFieldsError {
    /// The log could not be read or written.
    #[error("executed-fields log {path}: {source}")]
    Io {
        /// The log.
        path: PathBuf,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
    /// The file is not an executed-fields log.
    #[error("executed-fields log {0}: not an executed-fields log")]
    NotALog(PathBuf),
    /// The log belongs to another chain.
    #[error("executed-fields log {path} is for genesis {found}, this chain's is {expected}")]
    OtherChain {
        /// The log.
        path: PathBuf,
        /// The genesis the log is bound to.
        found: B256,
        /// This chain's genesis.
        expected: B256,
    },
    /// A block already filed with a different result: execution is not
    /// deterministic here, or two paths disagree. The first entry is kept.
    #[error("block {hash}: executed fields already filed differ in {field}")]
    Conflict {
        /// The block.
        hash: B256,
        /// The first differing field.
        field: &'static str,
    },
}

fn encode_record(hash: &B256, fields: &ExecutedFields) -> [u8; RECORD_LEN] {
    let mut record = [0u8; RECORD_LEN];
    let mut at = 0;
    for part in [
        hash.as_slice(),
        fields.state_root.as_slice(),
        fields.receipts_root.as_slice(),
        fields.logs_bloom.as_slice(),
        &fields.gas_used.to_be_bytes(),
        fields.requests_hash.as_slice(),
    ] {
        record[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    let checksum = blake3::hash(&record[..BODY_LEN]);
    record[BODY_LEN..].copy_from_slice(&checksum.as_bytes()[..CHECKSUM_LEN]);
    record
}

fn decode_record(record: &[u8]) -> Option<(B256, ExecutedFields)> {
    if record.len() != RECORD_LEN
        || blake3::hash(&record[..BODY_LEN]).as_bytes()[..CHECKSUM_LEN] != record[BODY_LEN..]
    {
        return None;
    }
    let b256 = |at: usize| B256::from_slice(&record[at..at + 32]);
    let mut gas = [0u8; 8];
    gas.copy_from_slice(&record[352..360]);
    Some((
        b256(0),
        ExecutedFields {
            state_root: b256(32),
            receipts_root: b256(64),
            logs_bloom: Bloom::from_slice(&record[96..352]),
            gas_used: u64::from_be_bytes(gas),
            requests_hash: b256(360),
        },
    ))
}

#[derive(Debug)]
struct Sidecar {
    path: PathBuf,
    file: File,
    genesis_hash: B256,
    records: usize,
}

impl Sidecar {
    fn io(&self, source: std::io::Error) -> ExecutedFieldsError {
        ExecutedFieldsError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn append(&mut self, hash: &B256, fields: &ExecutedFields) -> Result<(), ExecutedFieldsError> {
        let record = encode_record(hash, fields);
        self.file
            .write_all(&record)
            .and_then(|()| self.file.sync_data())
            .map_err(|error| self.io(error))?;
        self.records += 1;
        Ok(())
    }
}

/// Writes a fresh log holding `entries` at `path` (via a temporary file and
/// a rename) and opens it for appending.
fn write_log(
    path: &Path,
    genesis_hash: B256,
    entries: &[(B256, ExecutedFields)],
) -> Result<Sidecar, ExecutedFieldsError> {
    let io = |source| ExecutedFieldsError::Io {
        path: path.to_path_buf(),
        source,
    };
    let temporary = path.with_extension("log.tmp");
    {
        let mut file = File::create(&temporary).map_err(io)?;
        let mut bytes = Vec::with_capacity(FILE_HEADER_LEN + entries.len() * RECORD_LEN);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(genesis_hash.as_slice());
        for (hash, fields) in entries {
            bytes.extend_from_slice(&encode_record(hash, fields));
        }
        file.write_all(&bytes).and_then(|()| file.sync_all()).map_err(io)?;
    }
    std::fs::rename(&temporary, path).map_err(io)?;
    if let Some(dir) = path.parent()
        && let Ok(dir) = File::open(dir)
    {
        let _ = dir.sync_all();
    }
    let file = OpenOptions::new().append(true).open(path).map_err(io)?;
    Ok(Sidecar {
        path: path.to_path_buf(),
        file,
        genesis_hash,
        records: entries.len(),
    })
}

/// The executed-fields registry: the last [`KEEP`] results in memory and,
/// with a sidecar, every result durably.
#[derive(Debug, Default)]
pub struct ExecutedFieldsRegistry {
    by_hash: HashMap<B256, ExecutedFields>,
    order: VecDeque<B256>,
    sidecar: Option<Sidecar>,
}

impl ExecutedFieldsRegistry {
    /// An in-memory registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens (or creates) the durable log at `path` for the chain with
    /// `genesis_hash`, loading its records. A torn or corrupt tail is cut off
    /// at the last whole record; a log of another chain is refused.
    pub fn open(path: &Path, genesis_hash: B256) -> Result<Self, ExecutedFieldsError> {
        let mut registry = Self::new();
        if !path.exists() {
            registry.sidecar = Some(write_log(path, genesis_hash, &[])?);
            return Ok(registry);
        }
        let io = |source| ExecutedFieldsError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(io)?;
        if bytes.len() < FILE_HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
            return Err(ExecutedFieldsError::NotALog(path.to_path_buf()));
        }
        let found = B256::from_slice(&bytes[MAGIC.len()..FILE_HEADER_LEN]);
        if found != genesis_hash {
            return Err(ExecutedFieldsError::OtherChain {
                path: path.to_path_buf(),
                found,
                expected: genesis_hash,
            });
        }
        let mut records = 0;
        let mut good = FILE_HEADER_LEN;
        for record in bytes[FILE_HEADER_LEN..].chunks(RECORD_LEN) {
            let Some((hash, fields)) = decode_record(record) else {
                break;
            };
            registry.insert_in_memory(hash, fields);
            records += 1;
            good += RECORD_LEN;
        }
        let mut file = OpenOptions::new().write(true).open(path).map_err(io)?;
        if good != bytes.len() {
            tracing::warn!(
                target: "n42::executed_fields",
                path = %path.display(),
                kept = records,
                cut_bytes = bytes.len() - good,
                "executed-fields log has a torn or corrupt tail; cut at the last whole record"
            );
            file.set_len(good as u64)
                .and_then(|()| file.sync_all())
                .map_err(io)?;
        }
        file.seek(SeekFrom::End(0)).map_err(io)?;
        registry.sidecar = Some(Sidecar {
            path: path.to_path_buf(),
            file,
            genesis_hash,
            records,
        });
        Ok(registry)
    }

    fn insert_in_memory(&mut self, hash: B256, fields: ExecutedFields) {
        if self.by_hash.insert(hash, fields).is_none() {
            self.order.push_back(hash);
        }
        while self.order.len() > KEEP {
            if let Some(old) = self.order.pop_front() {
                self.by_hash.remove(&old);
            }
        }
    }

    /// Files `fields` as the result of block `hash`; with a sidecar the
    /// record is on disk when this returns. Filing the same result again is
    /// a no-op; a different one is refused and the first is kept.
    pub fn insert(&mut self, hash: B256, fields: ExecutedFields) -> Result<(), ExecutedFieldsError> {
        if let Some(known) = self.by_hash.get(&hash) {
            return match known.first_difference(&fields) {
                None => Ok(()),
                Some(field) => Err(ExecutedFieldsError::Conflict { hash, field }),
            };
        }
        if let Some(sidecar) = &mut self.sidecar {
            sidecar.append(&hash, &fields)?;
        }
        self.insert_in_memory(hash, fields);
        self.compact_if_due()
    }

    fn compact_if_due(&mut self) -> Result<(), ExecutedFieldsError> {
        let Some(sidecar) = &self.sidecar else {
            return Ok(());
        };
        if sidecar.records < COMPACT_AT {
            return Ok(());
        }
        let (path, genesis_hash) = (sidecar.path.clone(), sidecar.genesis_hash);
        // The tail of the log, in order: re-read it, the memory holds less.
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|source| ExecutedFieldsError::Io {
                path: path.clone(),
                source,
            })?;
        let entries: Vec<_> = bytes[FILE_HEADER_LEN..]
            .chunks(RECORD_LEN)
            .filter_map(decode_record)
            .collect();
        let keep = &entries[entries.len().saturating_sub(KEEP_DURABLE)..];
        self.sidecar = Some(write_log(&path, genesis_hash, keep)?);
        Ok(())
    }

    /// The result filed for block `hash`.
    pub fn get(&self, hash: &B256) -> Option<ExecutedFields> {
        self.by_hash.get(hash).copied()
    }

    /// Whether a result is filed for block `hash`.
    pub fn contains(&self, hash: &B256) -> bool {
        self.by_hash.contains_key(hash)
    }

    /// Blocks held in memory.
    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    /// Whether nothing is held in memory.
    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Records in the durable log (`None` without one).
    pub fn durable_records(&self) -> Option<usize> {
        self.sidecar.as_ref().map(|sidecar| sidecar.records)
    }
}

static REGISTRY: Mutex<Option<ExecutedFieldsRegistry>> = Mutex::new(None);
/// Signalled on every filing, for [`wait_for_executed_fields`].
static FILED: Condvar = Condvar::new();

fn with_registry<T>(f: impl FnOnce(&mut ExecutedFieldsRegistry) -> T) -> T {
    let mut guard = REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    f(guard.get_or_insert_with(ExecutedFieldsRegistry::new))
}

/// Installs the process registry over the durable log at `path` and seeds
/// genesis with its own fields (`genesis` is the genesis header, A6).
/// Returns the number of records loaded. Done once at start-up, before any
/// block is executed; the in-memory entries filed so far are kept.
pub fn install_executed_fields_log(
    path: &Path,
    genesis_hash: B256,
    genesis: &alloy_consensus::Header,
) -> Result<usize, ExecutedFieldsError> {
    let opened = ExecutedFieldsRegistry::open(path, genesis_hash)?;
    let loaded = opened.durable_records().unwrap_or_default();
    with_registry(|registry| {
        let earlier: Vec<_> = registry
            .order
            .iter()
            .filter_map(|hash| Some((*hash, registry.by_hash.get(hash).copied()?)))
            .collect();
        *registry = opened;
        for (hash, fields) in earlier {
            registry.insert(hash, fields)?;
        }
        registry.insert(genesis_hash, ExecutedFields::carried_by(genesis))
    })?;
    FILED.notify_all();
    Ok(loaded)
}

/// Files the result of block `hash` in the process registry (durably once
/// a log is installed).
pub fn remember_executed_fields(
    hash: B256,
    fields: ExecutedFields,
) -> Result<(), ExecutedFieldsError> {
    let filed = with_registry(|registry| registry.insert(hash, fields));
    FILED.notify_all();
    filed
}

/// The result filed for block `hash` in the process registry.
pub fn executed_fields(hash: &B256) -> Option<ExecutedFields> {
    with_registry(|registry| registry.get(hash))
}

/// [`executed_fields`], waiting up to `timeout` for it to be filed: a
/// leader's parent may still be importing when its build starts (design
/// section 1.3: wait, never guess).
pub fn wait_for_executed_fields(hash: &B256, timeout: Duration) -> Option<ExecutedFields> {
    let deadline = Instant::now() + timeout;
    let mut guard = REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if let Some(found) = guard.as_ref().and_then(|registry| registry.get(hash)) {
            return Some(found);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        guard = FILED
            .wait_timeout(guard, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(tag: u8) -> ExecutedFields {
        ExecutedFields {
            state_root: B256::repeat_byte(tag),
            receipts_root: B256::repeat_byte(tag.wrapping_add(1)),
            logs_bloom: Bloom::repeat_byte(tag.wrapping_add(2)),
            gas_used: u64::from(tag) * 21_000,
            requests_hash: B256::repeat_byte(tag.wrapping_add(3)),
        }
    }

    fn hash(n: u64) -> B256 {
        B256::left_padding_from(&n.to_be_bytes())
    }

    const GENESIS: B256 = B256::repeat_byte(0x99);

    #[test]
    fn insert_get_and_conflict() {
        let mut registry = ExecutedFieldsRegistry::new();
        registry.insert(hash(1), fields(1)).unwrap();
        assert_eq!(registry.get(&hash(1)), Some(fields(1)));
        registry.insert(hash(1), fields(1)).unwrap();
        let mut other = fields(1);
        other.gas_used += 1;
        assert!(matches!(
            registry.insert(hash(1), other),
            Err(ExecutedFieldsError::Conflict { field: "gasUsed", .. })
        ));
        assert_eq!(registry.get(&hash(1)), Some(fields(1)), "the first result is kept");
        assert_eq!(registry.get(&hash(2)), None);
    }

    #[test]
    fn memory_keeps_the_newest() {
        let mut registry = ExecutedFieldsRegistry::new();
        for n in 0..(KEEP as u64 + 10) {
            registry.insert(hash(n), fields(n as u8)).unwrap();
        }
        assert_eq!(registry.len(), KEEP);
        assert!(!registry.contains(&hash(9)));
        assert!(registry.contains(&hash(10)));
        assert!(registry.contains(&hash(KEEP as u64 + 9)));
    }

    #[test]
    fn every_field_is_named_on_difference() {
        let base = fields(1);
        let mut each = [base; 5];
        each[0].state_root = B256::ZERO;
        each[1].receipts_root = B256::ZERO;
        each[2].logs_bloom = Bloom::ZERO;
        each[3].gas_used = 0;
        each[4].requests_hash = B256::ZERO;
        let names: Vec<_> = each
            .iter()
            .map(|changed| changed.first_difference(&base).unwrap())
            .collect();
        assert_eq!(names, ["stateRoot", "receiptsRoot", "logsBloom", "gasUsed", "requestsHash"]);
        assert_eq!(base.first_difference(&base), None);
    }

    #[test]
    fn the_log_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("executed_fields.log");
        {
            let mut registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
            for n in 1..=5 {
                registry.insert(hash(n), fields(n as u8)).unwrap();
            }
            assert_eq!(registry.durable_records(), Some(5));
        }
        let registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
        assert_eq!(registry.durable_records(), Some(5));
        for n in 1..=5 {
            assert_eq!(registry.get(&hash(n)), Some(fields(n as u8)));
        }
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            FILE_HEADER_LEN + 5 * RECORD_LEN
        );
    }

    #[test]
    fn a_torn_or_corrupt_tail_is_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("executed_fields.log");
        {
            let mut registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
            for n in 1..=3 {
                registry.insert(hash(n), fields(n as u8)).unwrap();
            }
        }
        // A crash mid-append: half a record.
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&encode_record(&hash(4), &fields(4))[..RECORD_LEN / 2]).unwrap();
        drop(file);
        let mut registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
        assert_eq!(registry.durable_records(), Some(3));
        assert_eq!(registry.get(&hash(4)), None);
        // Appends continue on the cut log.
        registry.insert(hash(4), fields(4)).unwrap();
        drop(registry);
        // A flipped byte in the last record: that record and everything
        // after it is cut.
        let mut bytes = std::fs::read(&path).unwrap();
        let last = FILE_HEADER_LEN + 3 * RECORD_LEN;
        bytes[last + 40] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        let registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
        assert_eq!(registry.durable_records(), Some(3));
        assert_eq!(registry.get(&hash(3)), Some(fields(3)));
        assert_eq!(registry.get(&hash(4)), None);
        assert_eq!(std::fs::metadata(&path).unwrap().len() as usize, last);
    }

    #[test]
    fn another_chains_log_or_a_foreign_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("executed_fields.log");
        drop(ExecutedFieldsRegistry::open(&path, GENESIS).unwrap());
        assert!(matches!(
            ExecutedFieldsRegistry::open(&path, B256::repeat_byte(1)),
            Err(ExecutedFieldsError::OtherChain { .. })
        ));
        let foreign = dir.path().join("foreign.log");
        std::fs::write(&foreign, b"not a log at all, but long enough to have a header").unwrap();
        assert!(matches!(
            ExecutedFieldsRegistry::open(&foreign, GENESIS),
            Err(ExecutedFieldsError::NotALog(_))
        ));
    }

    #[test]
    fn the_log_is_compacted_to_its_newest_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("executed_fields.log");
        let mut registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
        for n in 0..COMPACT_AT as u64 {
            registry.insert(hash(n), fields(n as u8)).unwrap();
        }
        assert_eq!(registry.durable_records(), Some(KEEP_DURABLE));
        registry.insert(hash(COMPACT_AT as u64), fields(7)).unwrap();
        drop(registry);
        let registry = ExecutedFieldsRegistry::open(&path, GENESIS).unwrap();
        assert_eq!(registry.durable_records(), Some(KEEP_DURABLE + 1));
        assert_eq!(registry.get(&hash(COMPACT_AT as u64)), Some(fields(7)));
        assert_eq!(registry.get(&hash(COMPACT_AT as u64 - 1)), Some(fields((COMPACT_AT - 1) as u8)));
        assert!(!dir.path().join("executed_fields.log.tmp").exists());
    }

    /// Genesis's own fields from the V1 genesis (A6): its QMDB alloc root,
    /// the empty receipts root, a zero bloom, no gas, `sha256("")`.
    #[test]
    fn the_genesis_entry_is_the_v1_genesis_header() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let (spec, _, genesis) = n42_chainspec::profile1::profile1_chain_spec(&json).unwrap();
        let carried = ExecutedFields::carried_by(&genesis.header);
        assert_eq!(carried.state_root, genesis.header.state_root);
        assert_eq!(carried.receipts_root, alloy_consensus::EMPTY_ROOT_HASH);
        assert_eq!(carried.logs_bloom, Bloom::ZERO);
        assert_eq!(carried.gas_used, 0);
        assert_eq!(carried.requests_hash, n42_chainspec::profile1::PROFILE1_REQUESTS_HASH);

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("executed_fields.log");
        install_executed_fields_log(&log, spec.genesis_hash(), &genesis.header).unwrap();
        assert_eq!(executed_fields(&spec.genesis_hash()), Some(carried));
        // Waiting for a block filed by another thread.
        let later = hash(0xfeed);
        let filer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            remember_executed_fields(later, fields(5)).unwrap();
        });
        assert_eq!(wait_for_executed_fields(&later, Duration::from_secs(5)), Some(fields(5)));
        filer.join().unwrap();
        assert_eq!(wait_for_executed_fields(&hash(0xdead), Duration::from_millis(10)), None);
        drop(dir);
    }
}
