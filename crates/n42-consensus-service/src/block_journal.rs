//! AM35 block journal: a block is durable on this node before its R2 vote.
//!
//! The R2 (commit) vote is released once this node has executed the block
//! (`new_payload` Valid), but reth persists a block only after the
//! forkchoice update that follows its commit. A whole-set power loss between
//! the R2 votes and the first member's reth save leaves a durable lock
//! (`locked_qc.bin`) on a block whose body no member holds, and the chain
//! halts for good (`docs-drafts/whole-set-power-loss-20261127.md`).
//!
//! This journal closes that window (AM35 "option A"). Before any R2
//! signature the vote log calls [`BlockJournal::ensure_durable`], which
//! appends the raw block data of the voted block and of every ancestor above
//! the execution layer's persisted head that is not yet here, then
//! `fdatasync`s the file. The lock write follows immediately. On restart the
//! orchestrator re-imports the journalled blocks the execution layer lacks
//! before it serves, builds or votes, so a leader can build on the locked
//! block and peers can be served it.
//!
//! File format (`blocks.journal` beside `vote_log.bin`): a sequence of frames
//!
//! ```text
//! magic "N42J" | version u8 | 3 reserved bytes | payload length u32 LE
//!     | blake3(payload) 32 bytes | payload (bincode JournalRecord)
//! ```
//!
//! Opening the journal scans the frames and truncates the file at the first
//! frame that is short, oversized or fails its checksum: a torn tail from a
//! crash during an append is dropped, and the append it belonged to never
//! returned, so no vote rests on it. Entries leave the index once the
//! execution layer has persisted their block (matched by hash), or once they
//! are a dead fork below the finalized height; [`BlockJournal::maybe_compact`]
//! reclaims the space off the vote path.

use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionData;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// File name of the journal inside the consensus data directory.
pub const BLOCK_JOURNAL_FILE: &str = "blocks.journal";

const FRAME_MAGIC: [u8; 4] = *b"N42J";
const FRAME_VERSION: u8 = 1;
const FRAME_HEADER_BYTES: usize = 4 + 1 + 3 + 4 + 32;
/// Largest record accepted. A full native block is tens of MiB.
const MAX_RECORD_BYTES: u32 = 1 << 30;
/// Block data staged (held in memory, not yet durable) at most.
const MAX_STAGED_BLOCKS: usize = 256;
/// Longest unpersisted ancestry one append may write.
const MAX_ANCESTOR_WALK: usize = 4096;
/// Dead space below which compaction is not worth a rewrite.
const COMPACT_MIN_DEAD_BYTES: u64 = 64 << 20;
/// Live journal size above which every append warns: the execution layer's
/// persistence is far behind the chain.
const WARN_LIVE_BYTES: u64 = 4 << 30;

/// The execution layer's persisted chain, as far as the journal needs it.
pub trait PersistedChain: Send + Sync {
    /// Highest block number durably committed by the execution layer (a
    /// synced commit: reth `save_blocks` with static files, RocksDB and MDBX
    /// in `Durable` mode). `None` when unknown.
    fn persisted_number(&self) -> Option<u64>;

    /// Hash of the persisted canonical block at `number`, for `number` at or
    /// below [`Self::persisted_number`].
    fn persisted_hash(&self, number: u64) -> Option<B256>;
}

/// A [`PersistedChain`] built from two closures over the execution layer's
/// provider, so the node binary need not name the provider type here.
pub struct FnPersistedChain<N, H> {
    number: N,
    hash: H,
}

impl<N, H> FnPersistedChain<N, H>
where
    N: Fn() -> Option<u64> + Send + Sync,
    H: Fn(u64) -> Option<B256> + Send + Sync,
{
    pub const fn new(number: N, hash: H) -> Self {
        Self { number, hash }
    }
}

impl<N, H> PersistedChain for FnPersistedChain<N, H>
where
    N: Fn() -> Option<u64> + Send + Sync,
    H: Fn(u64) -> Option<B256> + Send + Sync,
{
    fn persisted_number(&self) -> Option<u64> {
        (self.number)()
    }

    fn persisted_hash(&self, number: u64) -> Option<B256> {
        (self.hash)(number)
    }
}

/// One journalled block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub block_hash: B256,
    pub parent_hash: B256,
    pub number: u64,
    pub view: u64,
    /// The block's native (Gov5 / Profile-1) header RLP when the node knew
    /// it at append time; a restart needs it to admit the block again.
    pub native_header: Option<Vec<u8>>,
    /// bincode `BlockDataBroadcast`: the block exactly as the block-data
    /// path carries it (payload possibly zstd-compressed).
    pub block_data: Vec<u8>,
}

/// Result of [`BlockJournal::ensure_durable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// The block and its unpersisted ancestors are durable.
    Durable(AppendStats),
    /// The data of `missing` is not staged here yet.
    Unavailable { missing: B256 },
}

/// What one durability call wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AppendStats {
    /// Blocks appended (0 when already durable or persisted).
    pub blocks: usize,
    /// Bytes appended, frame headers included.
    pub bytes: u64,
    /// Time to build and write the frames, microseconds.
    pub write_us: u64,
    /// Time in `fdatasync`, microseconds.
    pub sync_us: u64,
}

/// What [`BlockJournal::open`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenReport {
    pub records: usize,
    pub truncated_bytes: u64,
    pub truncation_reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct DurableMeta {
    number: u64,
    view: u64,
    offset: u64,
    len: u64,
}

enum StagedSource {
    Data(Arc<Vec<u8>>),
    Execution {
        data: Box<ExecutionData>,
        timestamp: u64,
    },
}

struct Staged {
    view: u64,
    source: StagedSource,
}

struct Inner {
    file: File,
    len: u64,
    live_bytes: u64,
    durable: HashMap<B256, DurableMeta>,
    staged: HashMap<B256, Staged>,
    staged_order: VecDeque<B256>,
    lineage: Lineage,
    /// Height of the highest block consensus finalized (committed and made
    /// canonical). A journal entry at or below it whose height the
    /// execution layer persisted under another hash is a dead fork.
    finalized_number: Option<u64>,
}

/// The block journal. Shared between the vote log (appends before R2) and
/// the orchestrator (stages block data, replays on restart, compacts).
pub struct BlockJournal {
    path: PathBuf,
    inner: Mutex<Inner>,
    persisted: RwLock<Option<Arc<dyn PersistedChain>>>,
}

impl std::fmt::Debug for BlockJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockJournal")
            .field("path", &self.path)
            .finish()
    }
}

/// `hash -> (parent, number)` known without decoding a payload, with
/// insertion order for the bound.
#[derive(Default)]
struct Lineage {
    map: HashMap<B256, (B256, u64)>,
    order: VecDeque<B256>,
}

/// Lineage entries remembered at most.
const LINEAGE_REMEMBERED: usize = 2048;

impl Lineage {
    fn note(&mut self, hash: B256, parent_hash: B256, number: u64) {
        if self.map.insert(hash, (parent_hash, number)).is_none() {
            self.order.push_back(hash);
            if self.order.len() > LINEAGE_REMEMBERED
                && let Some(old) = self.order.pop_front()
            {
                self.map.remove(&old);
            }
        }
    }
}

fn encode_frame(record: &JournalRecord) -> io::Result<Vec<u8>> {
    let payload = bincode::serialize(record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|len| *len <= MAX_RECORD_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal record too large"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&FRAME_MAGIC);
    frame.push(FRAME_VERSION);
    frame.extend_from_slice(&[0u8; 3]);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(blake3::hash(&payload).as_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Reads the frame at the reader's position. `Ok(None)` at a clean end of
/// file; `Err` with a reason for a torn or damaged frame.
fn read_frame(
    reader: &mut impl Read,
    remaining: u64,
) -> Result<Option<(JournalRecord, u64)>, String> {
    if remaining == 0 {
        return Ok(None);
    }
    if remaining < FRAME_HEADER_BYTES as u64 {
        return Err(format!("torn frame header ({remaining} bytes)"));
    }
    let mut header = [0u8; FRAME_HEADER_BYTES];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("read frame header: {e}"))?;
    if header[..4] != FRAME_MAGIC {
        return Err("bad frame magic".to_string());
    }
    if header[4] != FRAME_VERSION {
        return Err(format!("unknown frame version {}", header[4]));
    }
    let len = u32::from_le_bytes(header[8..12].try_into().expect("four length bytes"));
    if len > MAX_RECORD_BYTES {
        return Err(format!("frame length {len} over the limit"));
    }
    let frame_len = FRAME_HEADER_BYTES as u64 + u64::from(len);
    if frame_len > remaining {
        return Err(format!(
            "torn frame payload ({} of {len} bytes)",
            remaining - FRAME_HEADER_BYTES as u64
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader
        .read_exact(&mut payload)
        .map_err(|e| format!("read frame payload: {e}"))?;
    if blake3::hash(&payload).as_bytes() != &header[12..44] {
        return Err("frame checksum mismatch".to_string());
    }
    let record: JournalRecord =
        bincode::deserialize(&payload).map_err(|e| format!("frame record: {e}"))?;
    Ok(Some((record, frame_len)))
}

impl BlockJournal {
    /// Opens (creating if needed) the journal at `path`, indexes its frames
    /// and truncates a torn or damaged tail.
    pub fn open(path: PathBuf) -> io::Result<(Self, OpenReport)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let created = !path.exists();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let file_len = file.metadata()?.len();
        let mut report = OpenReport::default();
        let mut durable = HashMap::new();
        let mut offset = 0u64;
        {
            file.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::with_capacity(1 << 20, &mut file);
            loop {
                match read_frame(&mut reader, file_len - offset) {
                    Ok(Some((record, frame_len))) => {
                        durable.insert(
                            record.block_hash,
                            DurableMeta {
                                number: record.number,
                                view: record.view,
                                offset,
                                len: frame_len,
                            },
                        );
                        offset += frame_len;
                        report.records += 1;
                    }
                    Ok(None) => break,
                    Err(reason) => {
                        report.truncated_bytes = file_len - offset;
                        report.truncation_reason = Some(reason);
                        break;
                    }
                }
            }
        }
        if report.truncated_bytes > 0 {
            file.set_len(offset)?;
            file.sync_all()?;
        }
        if created {
            n42_jmt::snapshot::sync_parent_directory(&path)?;
        }
        let live_bytes = durable.values().map(|meta| meta.len).sum();
        Ok((
            Self {
                path,
                inner: Mutex::new(Inner {
                    file,
                    len: offset,
                    live_bytes,
                    durable,
                    staged: HashMap::new(),
                    staged_order: VecDeque::new(),
                    lineage: Lineage::default(),
                    finalized_number: None,
                }),
                persisted: RwLock::new(None),
            },
            report,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Installs the execution layer's persisted chain. Until then nothing
    /// counts as persisted, and every ancestor must be staged or durable.
    pub fn set_persisted_chain(&self, chain: Arc<dyn PersistedChain>) {
        *self
            .persisted
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(chain);
    }

    fn persisted_chain(&self) -> Option<Arc<dyn PersistedChain>> {
        self.persisted
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records the parent and number of `hash` so an append need not decode
    /// its payload. Fed by the leader's build, imports and catch-up; bounded.
    pub fn note_lineage(&self, hash: B256, parent_hash: B256, number: u64) {
        self.lock().lineage.note(hash, parent_hash, number);
    }

    /// Records that consensus finalized `hash` (committed, forkchoice moved
    /// to it). Its height bounds the dead-fork pruning; the height comes
    /// from what the journal knows about `hash`, so an unknown hash is a
    /// no-op.
    pub fn note_finalized(&self, hash: B256) {
        let mut inner = self.lock();
        let number = inner
            .lineage
            .map
            .get(&hash)
            .map(|(_, number)| *number)
            .or_else(|| inner.durable.get(&hash).map(|meta| meta.number));
        if let Some(number) = number {
            inner.finalized_number = Some(inner.finalized_number.unwrap_or(0).max(number));
        }
    }

    /// Whether the execution layer persisted `hash` as its canonical block
    /// at `number` (hash compared, not height alone).
    pub fn is_persisted(&self, hash: B256, number: u64) -> bool {
        let Some(chain) = self.persisted_chain() else {
            return false;
        };
        chain.persisted_number().is_some_and(|tip| number <= tip)
            && chain.persisted_hash(number) == Some(hash)
    }

    /// Holds the block data of `hash` (a bincode `BlockDataBroadcast`) until
    /// an R2 vote needs it durable. The first copy staged under a hash wins;
    /// [`Self::unstage`] drops a copy found to be forged.
    pub fn stage_data(&self, hash: B256, view: u64, data: Arc<Vec<u8>>) {
        self.stage(
            hash,
            Staged {
                view,
                source: StagedSource::Data(data),
            },
        );
    }

    /// Holds a block known only as an execution payload (catch-up imports).
    /// It is encoded only if an R2 vote needs it durable.
    pub fn stage_execution(&self, hash: B256, view: u64, timestamp: u64, data: &ExecutionData) {
        if self.lock().durable.contains_key(&hash) {
            return;
        }
        self.stage(
            hash,
            Staged {
                view,
                source: StagedSource::Execution {
                    data: Box::new(data.clone()),
                    timestamp,
                },
            },
        );
    }

    fn stage(&self, hash: B256, staged: Staged) {
        let mut inner = self.lock();
        if inner.durable.contains_key(&hash) || inner.staged.contains_key(&hash) {
            return;
        }
        if inner.staged_order.len() >= MAX_STAGED_BLOCKS
            && let Some(oldest) = inner.staged_order.pop_front()
        {
            inner.staged.remove(&oldest);
        }
        inner.staged.insert(hash, staged);
        inner.staged_order.push_back(hash);
    }

    /// Forgets the staged data of `hash` (its import rejected the bytes).
    pub fn unstage(&self, hash: &B256) {
        let mut inner = self.lock();
        if inner.staged.remove(hash).is_some() {
            inner.staged_order.retain(|candidate| candidate != hash);
        }
    }

    /// Whether `hash` is durable in the journal.
    pub fn is_durable(&self, hash: &B256) -> bool {
        self.lock().durable.contains_key(hash)
    }

    /// Whether data for `hash` is staged or durable here.
    pub fn holds(&self, hash: &B256) -> bool {
        let inner = self.lock();
        inner.durable.contains_key(hash) || inner.staged.contains_key(hash)
    }

    /// Number of live (unpersisted) journal entries.
    pub fn live_entries(&self) -> usize {
        self.lock().durable.len()
    }

    /// AM35: makes `hash` and every ancestor above the execution layer's
    /// persisted head durable here, with one append and one `fdatasync`.
    /// Ancestors are matched by hash: an ancestor counts as persisted only if
    /// the persisted canonical block at its height has its hash.
    pub fn ensure_durable(&self, hash: B256) -> io::Result<Durability> {
        let started = Instant::now();
        let persisted = self.persisted_chain();
        let persisted_number = persisted
            .as_ref()
            .and_then(|chain| chain.persisted_number());
        let is_persisted = |hash: B256, number: u64| {
            persisted_number.is_some_and(|tip| number <= tip)
                && persisted
                    .as_ref()
                    .and_then(|chain| chain.persisted_hash(number))
                    == Some(hash)
        };

        let mut inner = self.lock();
        Self::prune(&mut inner, persisted.as_deref());

        // Walk from the voted block towards the persisted head, collecting
        // what is neither durable here nor persisted by the EL. A durable
        // entry's own ancestors were made durable when it was appended.
        let mut chain: Vec<(B256, u64, B256, u64)> = Vec::new();
        let mut current = hash;
        let mut expected_number: Option<u64> = None;
        loop {
            if inner.durable.contains_key(&current) {
                break;
            }
            if let Some(number) = expected_number
                && is_persisted(current, number)
            {
                break;
            }
            let Some(staged) = inner.staged.get(&current) else {
                metrics::counter!("n42_block_journal_unavailable_total").increment(1);
                return Ok(Durability::Unavailable { missing: current });
            };
            let view = staged.view;
            let Some((parent, number)) = Self::staged_lineage(&inner.lineage, current, staged)
            else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("staged block data of {current} does not decode to its parent"),
                ));
            };
            if expected_number.is_some_and(|expected| expected != number) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("block {current} is at height {number}, not its child's height - 1"),
                ));
            }
            if is_persisted(current, number) {
                metrics::counter!("n42_block_journal_skipped_persisted_total").increment(1);
                break;
            }
            // A block at a persisted height under another hash is a fork the
            // execution layer does not hold durably: it is journalled like
            // any unpersisted ancestor, and the walk goes on to the common
            // ancestor. A QC on such a fork cannot conflict with a committed
            // block, so this is normally a fork above the finalized height
            // whose sibling reth persisted early.
            chain.push((current, number, parent, view));
            if chain.len() > MAX_ANCESTOR_WALK {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("more than {MAX_ANCESTOR_WALK} unpersisted ancestors below {hash}"),
                ));
            }
            current = parent;
            expected_number = number.checked_sub(1);
        }
        if chain.is_empty() {
            return Ok(Durability::Durable(AppendStats::default()));
        }

        // Oldest first: a torn append can only lose a suffix of descendants.
        let lineage: Vec<(B256, B256, u64)> = chain
            .iter()
            .map(|(block_hash, number, parent, _)| (*block_hash, *parent, *number))
            .collect();
        for (block_hash, parent, number) in lineage {
            inner.lineage.note(block_hash, parent, number);
        }
        let mut buffer = Vec::new();
        let mut frames = Vec::with_capacity(chain.len());
        for (block_hash, number, parent_hash, view) in chain.iter().rev().copied() {
            let staged = inner
                .staged
                .get(&block_hash)
                .expect("walked from staged data");
            let record = JournalRecord {
                block_hash,
                parent_hash,
                number,
                view,
                native_header: n42_consensus::gov5_native_header_rlp(&block_hash)
                    .map(|raw| raw.to_vec()),
                block_data: Self::staged_block_data(block_hash, staged)?,
            };
            let frame = encode_frame(&record)?;
            frames.push((
                block_hash,
                number,
                view,
                buffer.len() as u64,
                frame.len() as u64,
            ));
            buffer.extend_from_slice(&frame);
        }

        let base = inner.len;
        let written = inner
            .file
            .seek(SeekFrom::Start(base))
            .and_then(|_| inner.file.write_all(&buffer));
        let write_us = started.elapsed().as_micros() as u64;
        let sync_started = Instant::now();
        let synced = written.and_then(|_| inner.file.sync_data());
        let sync_us = sync_started.elapsed().as_micros() as u64;
        if let Err(error) = synced {
            // Roll the partial append back so the next open does not see it;
            // no vote is signed on a failed append.
            let _ = inner.file.set_len(base);
            return Err(error);
        }
        inner.len = base + buffer.len() as u64;
        inner.live_bytes += buffer.len() as u64;
        for (block_hash, number, view, offset, len) in frames {
            inner.durable.insert(
                block_hash,
                DurableMeta {
                    number,
                    view,
                    offset: base + offset,
                    len,
                },
            );
            inner.staged.remove(&block_hash);
        }
        let Inner {
            staged,
            staged_order,
            ..
        } = &mut *inner;
        staged_order.retain(|candidate| staged.contains_key(candidate));

        let stats = AppendStats {
            blocks: chain.len(),
            bytes: buffer.len() as u64,
            write_us,
            sync_us,
        };
        metrics::histogram!("n42_block_journal_append_ms")
            .record(started.elapsed().as_secs_f64() * 1000.0);
        metrics::histogram!("n42_block_journal_fsync_ms").record(sync_us as f64 / 1000.0);
        metrics::counter!("n42_block_journal_appended_blocks_total").increment(stats.blocks as u64);
        metrics::counter!("n42_block_journal_appended_bytes_total").increment(stats.bytes);
        metrics::gauge!("n42_block_journal_live_bytes").set(inner.live_bytes as f64);
        metrics::gauge!("n42_block_journal_live_blocks").set(inner.durable.len() as f64);
        if inner.live_bytes > WARN_LIVE_BYTES {
            tracing::warn!(
                target: "n42::cl::block_journal",
                live_bytes = inner.live_bytes,
                live_blocks = inner.durable.len(),
                "block journal is large: execution-layer persistence is far behind the chain"
            );
        }
        tracing::debug!(
            target: "n42::cl::block_journal",
            %hash,
            blocks = stats.blocks,
            bytes = stats.bytes,
            write_us,
            sync_us,
            "block journal append synced"
        );
        Ok(Durability::Durable(stats))
    }

    fn staged_lineage(lineage: &Lineage, hash: B256, staged: &Staged) -> Option<(B256, u64)> {
        if let Some(found) = lineage.map.get(&hash) {
            return Some(*found);
        }
        match &staged.source {
            StagedSource::Data(data) => crate::orchestrator::block_data_lineage(hash, data),
            StagedSource::Execution { data, .. } => {
                (data.block_hash() == hash).then(|| (data.parent_hash(), data.block_number()))
            }
        }
    }

    fn staged_block_data(hash: B256, staged: &Staged) -> io::Result<Vec<u8>> {
        match &staged.source {
            StagedSource::Data(data) => Ok(data.as_ref().clone()),
            StagedSource::Execution { data, timestamp } => {
                crate::orchestrator::block_data_from_execution(hash, staged.view, *timestamp, data)
            }
        }
    }

    /// Drops the entries that are no longer needed, matched by hash:
    /// - the execution layer persisted the entry's block (the persisted
    ///   canonical block at its height has its hash);
    /// - the entry is a dead fork: its height is at or below both the
    ///   persisted tip and the finalized height, and the persisted block
    ///   there has another hash. A finalized block is final, so no QC can
    ///   ever extend a sibling below it.
    ///
    /// An entry at a persisted height under another hash but above the
    /// finalized height stays: reth may persist an early sibling, and the
    /// entry may still be the block consensus commits.
    fn prune(inner: &mut Inner, persisted: Option<&dyn PersistedChain>) -> usize {
        let Some(chain) = persisted else {
            return 0;
        };
        let Some(tip) = chain.persisted_number() else {
            return 0;
        };
        let finalized = inner.finalized_number;
        let before = inner.durable.len();
        let mut freed = 0u64;
        inner.durable.retain(|hash, meta| {
            if meta.number > tip {
                return true;
            }
            let keep = match chain.persisted_hash(meta.number) {
                Some(persisted) if persisted == *hash => false,
                Some(_) => finalized.is_none_or(|finalized| meta.number > finalized),
                None => true,
            };
            if !keep {
                freed += meta.len;
            }
            keep
        });
        inner.live_bytes -= freed;
        before - inner.durable.len()
    }

    /// Drops entries the execution layer persisted (by hash) and dead forks
    /// below the finalized height, and reclaims file space when worthwhile.
    /// Runs off the vote path.
    pub fn maybe_compact(&self) -> io::Result<usize> {
        let persisted = self.persisted_chain();
        let mut inner = self.lock();
        let pruned = Self::prune(&mut inner, persisted.as_deref());
        let dead = inner.len - inner.live_bytes;
        if inner.durable.is_empty() && inner.len > 0 {
            inner.file.set_len(0)?;
            inner.file.sync_data()?;
            inner.len = 0;
        } else if dead > COMPACT_MIN_DEAD_BYTES.max(inner.live_bytes) {
            self.rewrite_live(&mut inner)?;
        }
        metrics::gauge!("n42_block_journal_live_bytes").set(inner.live_bytes as f64);
        metrics::gauge!("n42_block_journal_live_blocks").set(inner.durable.len() as f64);
        Ok(pruned)
    }

    fn rewrite_live(&self, inner: &mut Inner) -> io::Result<()> {
        let tmp_path = self.path.with_extension("journal.tmp");
        let mut tmp = File::create(&tmp_path)?;
        let mut live: Vec<(B256, DurableMeta)> = inner
            .durable
            .iter()
            .map(|(hash, meta)| (*hash, *meta))
            .collect();
        live.sort_by_key(|(_, meta)| meta.offset);
        let mut offset = 0u64;
        let mut moved = HashMap::with_capacity(live.len());
        for (hash, meta) in live {
            let mut frame = vec![0u8; meta.len as usize];
            inner.file.seek(SeekFrom::Start(meta.offset))?;
            inner.file.read_exact(&mut frame)?;
            tmp.write_all(&frame)?;
            moved.insert(hash, DurableMeta { offset, ..meta });
            offset += meta.len;
        }
        tmp.sync_all()?;
        drop(tmp);
        std::fs::rename(&tmp_path, &self.path)?;
        n42_jmt::snapshot::sync_parent_directory(&self.path)?;
        inner.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        inner.durable = moved;
        inner.len = offset;
        inner.live_bytes = offset;
        Ok(())
    }

    fn read_at(inner: &mut Inner, meta: DurableMeta) -> io::Result<JournalRecord> {
        inner.file.seek(SeekFrom::Start(meta.offset))?;
        match read_frame(&mut inner.file, meta.len) {
            Ok(Some((record, _))) => Ok(record),
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty journal frame",
            )),
            Err(reason) => Err(io::Error::new(io::ErrorKind::InvalidData, reason)),
        }
    }

    /// The durable record of `hash`, for serving peers.
    pub fn get(&self, hash: &B256) -> io::Result<Option<JournalRecord>> {
        let mut inner = self.lock();
        let Some(meta) = inner.durable.get(hash).copied() else {
            return Ok(None);
        };
        Self::read_at(&mut inner, meta).map(Some)
    }

    /// Every live record in height order (then view), for restart replay.
    pub fn records(&self) -> io::Result<Vec<JournalRecord>> {
        let mut inner = self.lock();
        let mut metas: Vec<DurableMeta> = inner.durable.values().copied().collect();
        metas.sort_by_key(|meta| (meta.number, meta.view, meta.offset));
        metas
            .into_iter()
            .map(|meta| Self::read_at(&mut inner, meta))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::BlockDataBroadcast;
    use alloy_primitives::{Address, Bloom, Bytes, U256};
    use alloy_rpc_types_engine::{ExecutionPayload, ExecutionPayloadSidecar, ExecutionPayloadV1};

    /// A persisted chain fixed by the test.
    #[derive(Default)]
    pub(crate) struct FixedPersisted(pub Mutex<Vec<B256>>);

    impl PersistedChain for FixedPersisted {
        fn persisted_number(&self) -> Option<u64> {
            let chain = self.0.lock().unwrap();
            (!chain.is_empty()).then(|| chain.len() as u64 - 1)
        }

        fn persisted_hash(&self, number: u64) -> Option<B256> {
            self.0.lock().unwrap().get(number as usize).copied()
        }
    }

    fn execution(parent: B256, hash: B256, number: u64, txs: usize) -> ExecutionData {
        ExecutionData::new(
            ExecutionPayload::V1(ExecutionPayloadV1 {
                parent_hash: parent,
                fee_recipient: Address::ZERO,
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Bloom::ZERO,
                prev_randao: B256::ZERO,
                block_number: number,
                gas_limit: 30_000_000,
                gas_used: 0,
                timestamp: number,
                extra_data: Bytes::new(),
                base_fee_per_gas: U256::from(1),
                block_hash: hash,
                transactions: (0..txs)
                    .map(|i| Bytes::from(vec![(i % 251) as u8; 110]))
                    .collect(),
            }),
            ExecutionPayloadSidecar::none(),
        )
    }

    fn block_data(parent: B256, hash: B256, number: u64, view: u64) -> Arc<Vec<u8>> {
        let payload_json = serde_json::to_vec(&execution(parent, hash, number, 2)).unwrap();
        Arc::new(
            bincode::serialize(&BlockDataBroadcast {
                block_hash: hash,
                view,
                payload_json,
                timestamp: number,
                execution_output: None,
                leader_ready_unix_ms: 0,
                native_header_rlp: None,
            })
            .unwrap(),
        )
    }

    fn h(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn open(dir: &Path) -> (BlockJournal, OpenReport) {
        BlockJournal::open(dir.join(BLOCK_JOURNAL_FILE)).unwrap()
    }

    fn persisted(chain: &[B256]) -> Arc<FixedPersisted> {
        Arc::new(FixedPersisted(Mutex::new(chain.to_vec())))
    }

    #[test]
    fn append_then_read_back_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, report) = open(dir.path());
        assert_eq!(report, OpenReport::default());
        journal.set_persisted_chain(persisted(&[h(0)]));
        let data = block_data(h(0), h(1), 1, 7);
        journal.stage_data(h(1), 7, Arc::clone(&data));

        let Durability::Durable(stats) = journal.ensure_durable(h(1)).unwrap() else {
            panic!("staged block must become durable");
        };
        assert_eq!(stats.blocks, 1);
        assert!(journal.is_durable(&h(1)));
        // A second call is free: the block is already durable.
        assert_eq!(
            journal.ensure_durable(h(1)).unwrap(),
            Durability::Durable(AppendStats::default())
        );
        drop(journal);

        let (journal, report) = open(dir.path());
        assert_eq!(report.records, 1);
        let record = journal.get(&h(1)).unwrap().expect("record survives reopen");
        assert_eq!(
            (
                record.block_hash,
                record.parent_hash,
                record.number,
                record.view
            ),
            (h(1), h(0), 1, 7)
        );
        assert_eq!(record.block_data, *data);
        assert_eq!(journal.records().unwrap(), vec![record]);
    }

    #[test]
    fn unstaged_block_or_ancestor_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, _) = open(dir.path());
        journal.set_persisted_chain(persisted(&[h(0)]));
        assert_eq!(
            journal.ensure_durable(h(2)).unwrap(),
            Durability::Unavailable { missing: h(2) }
        );
        // B2's parent B1 is above the persisted head and not staged.
        journal.stage_data(h(2), 2, block_data(h(1), h(2), 2, 2));
        assert_eq!(
            journal.ensure_durable(h(2)).unwrap(),
            Durability::Unavailable { missing: h(1) }
        );
        assert!(
            !journal.is_durable(&h(2)),
            "nothing is written without the whole ancestry"
        );

        journal.stage_execution(h(1), 1, 1, &execution(h(0), h(1), 1, 3));
        let Durability::Durable(stats) = journal.ensure_durable(h(2)).unwrap() else {
            panic!("the ancestry is complete");
        };
        assert_eq!(stats.blocks, 2, "ancestor and block in one append");
        let records = journal.records().unwrap();
        assert_eq!(
            records.iter().map(|r| r.block_hash).collect::<Vec<_>>(),
            vec![h(1), h(2)]
        );
        // The catch-up block was encoded into block data that replays.
        let broadcast: BlockDataBroadcast = bincode::deserialize(&records[0].block_data).unwrap();
        assert_eq!(broadcast.block_hash, h(1));
        assert_eq!(
            crate::orchestrator::block_data_lineage(h(1), &records[0].block_data),
            Some((h(0), 1))
        );
    }

    #[test]
    fn persisted_block_is_not_journalled_and_ancestors_match_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, _) = open(dir.path());
        journal.set_persisted_chain(persisted(&[h(0), h(1)]));
        journal.stage_data(h(1), 1, block_data(h(0), h(1), 1, 1));
        assert_eq!(
            journal.ensure_durable(h(1)).unwrap(),
            Durability::Durable(AppendStats::default()),
            "already persisted by the EL: no journal write"
        );
        assert_eq!(journal.live_entries(), 0);

        // A block on a fork whose parent sits at a persisted height under
        // another hash: the parent is not persisted, so it must be staged.
        journal.stage_data(h(0x22), 2, block_data(h(0x11), h(0x22), 2, 2));
        assert_eq!(
            journal.ensure_durable(h(0x22)).unwrap(),
            Durability::Unavailable { missing: h(0x11) }
        );
        journal.stage_data(h(0x11), 1, block_data(h(0), h(0x11), 1, 1));
        // The fork parent is journalled with its child (not refused): the
        // EL holds a sibling at its height, not the parent itself.
        assert!(matches!(
            journal.ensure_durable(h(0x22)).unwrap(),
            Durability::Durable(stats) if stats.blocks == 2
        ));
        assert!(journal.is_durable(&h(0x11)) && journal.is_durable(&h(0x22)));

        // Pruning compares hashes: the fork entry at the persisted height 1
        // survives the persisted sibling while nothing is finalized there.
        assert_eq!(journal.maybe_compact().unwrap(), 0);
        assert!(journal.is_durable(&h(0x11)));
        // Once consensus finalizes the persisted sibling, the fork is dead.
        journal.note_lineage(h(1), h(0), 1);
        journal.note_finalized(h(1));
        assert_eq!(journal.maybe_compact().unwrap(), 1);
        assert!(!journal.is_durable(&h(0x11)));
        assert!(
            journal.is_durable(&h(0x22)),
            "height 2 is above the persisted tip"
        );
    }

    #[test]
    fn torn_tail_is_truncated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOCK_JOURNAL_FILE);
        {
            let (journal, _) = open(dir.path());
            journal.set_persisted_chain(persisted(&[h(0)]));
            journal.stage_data(h(1), 1, block_data(h(0), h(1), 1, 1));
            journal.ensure_durable(h(1)).unwrap();
            journal.stage_data(h(2), 2, block_data(h(1), h(2), 2, 2));
            journal.ensure_durable(h(2)).unwrap();
        }
        let full = std::fs::read(&path).unwrap();
        // Cut the second frame in half, as a crash during its append would.
        let first_len = {
            let (journal, _) = open(dir.path());
            let inner = journal.lock();
            inner.durable[&h(1)].len
        };
        let cut = first_len + (full.len() as u64 - first_len) / 2;
        std::fs::write(&path, &full[..cut as usize]).unwrap();

        let (journal, report) = open(dir.path());
        assert_eq!(report.records, 1);
        assert_eq!(report.truncated_bytes, cut - first_len);
        assert!(report.truncation_reason.unwrap().contains("torn"));
        assert!(journal.is_durable(&h(1)));
        assert!(!journal.is_durable(&h(2)));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), first_len);

        // A torn header (a few bytes) is dropped too, and appends continue.
        drop(journal);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"N42");
        std::fs::write(&path, &bytes).unwrap();
        let (journal, report) = open(dir.path());
        assert_eq!((report.records, report.truncated_bytes), (1, 3));
        journal.set_persisted_chain(persisted(&[h(0)]));
        journal.stage_data(h(2), 2, block_data(h(1), h(2), 2, 2));
        assert!(matches!(
            journal.ensure_durable(h(2)).unwrap(),
            Durability::Durable(_)
        ));
        drop(journal);
        assert_eq!(open(dir.path()).1.records, 2);
    }

    #[test]
    fn checksum_mismatch_stops_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOCK_JOURNAL_FILE);
        {
            let (journal, _) = open(dir.path());
            journal.set_persisted_chain(persisted(&[h(0)]));
            journal.stage_data(h(1), 1, block_data(h(0), h(1), 1, 1));
            journal.ensure_durable(h(1)).unwrap();
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let (journal, report) = open(dir.path());
        assert_eq!(report.records, 0);
        assert_eq!(
            report.truncation_reason.as_deref(),
            Some("frame checksum mismatch")
        );
        assert_eq!(report.truncated_bytes, bytes.len() as u64);
        assert!(!journal.is_durable(&h(1)));
    }

    #[test]
    fn pruning_follows_the_persisted_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOCK_JOURNAL_FILE);
        let (journal, _) = open(dir.path());
        let chain = persisted(&[h(0)]);
        journal.set_persisted_chain(chain.clone());
        for n in 1..=3u8 {
            journal.stage_data(
                h(n),
                u64::from(n),
                block_data(h(n - 1), h(n), u64::from(n), u64::from(n)),
            );
        }
        assert!(
            matches!(journal.ensure_durable(h(3)).unwrap(), Durability::Durable(s) if s.blocks == 3)
        );
        assert_eq!(journal.live_entries(), 3);

        // reth persists B1 and B2: they leave the index; B3 stays.
        chain.0.lock().unwrap().extend([h(1), h(2)]);
        assert_eq!(journal.maybe_compact().unwrap(), 2);
        assert_eq!(journal.live_entries(), 1);
        assert!(journal.is_durable(&h(3)));
        assert_eq!(
            journal
                .records()
                .unwrap()
                .iter()
                .map(|r| r.block_hash)
                .collect::<Vec<_>>(),
            vec![h(3)]
        );

        // Once B3 is persisted too the file is emptied.
        chain.0.lock().unwrap().push(h(3));
        assert_eq!(journal.maybe_compact().unwrap(), 1);
        assert_eq!(journal.live_entries(), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        drop(journal);
        assert_eq!(open(dir.path()).1.records, 0);
    }

    #[test]
    fn compaction_rewrites_live_frames() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, _) = open(dir.path());
        let chain = persisted(&[h(0)]);
        journal.set_persisted_chain(chain.clone());
        journal.stage_data(h(1), 1, block_data(h(0), h(1), 1, 1));
        journal.stage_data(h(2), 2, block_data(h(1), h(2), 2, 2));
        journal.ensure_durable(h(2)).unwrap();
        chain.0.lock().unwrap().push(h(1));
        {
            let mut inner = journal.lock();
            let chain: Arc<dyn PersistedChain> = chain.clone();
            BlockJournal::prune(&mut inner, Some(chain.as_ref()));
            journal.rewrite_live(&mut inner).unwrap();
        }
        assert_eq!(journal.get(&h(2)).unwrap().unwrap().number, 2);
        assert!(journal.get(&h(1)).unwrap().is_none());
        drop(journal);
        let (journal, report) = open(dir.path());
        assert_eq!(report.records, 1);
        assert!(journal.is_durable(&h(2)));
    }

    #[test]
    fn unstage_drops_forged_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (journal, _) = open(dir.path());
        journal.set_persisted_chain(persisted(&[h(0)]));
        journal.stage_data(h(1), 1, Arc::new(vec![1, 2, 3]));
        assert!(
            journal.ensure_durable(h(1)).is_err(),
            "undecodable bytes are not journalled"
        );
        journal.unstage(&h(1));
        journal.stage_data(h(1), 1, block_data(h(0), h(1), 1, 1));
        assert!(matches!(
            journal.ensure_durable(h(1)).unwrap(),
            Durability::Durable(_)
        ));
    }

    /// Cost of AM35 on the R2 path: one append + `fdatasync` of a ~1200-tx
    /// block. Run on the target disk with
    /// `N42_JOURNAL_BENCH_DIR=/data/... cargo test --release -p
    /// n42-consensus-service block_journal_append_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn block_journal_append_bench() {
        let base = std::env::var("N42_JOURNAL_BENCH_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = tempfile::tempdir_in(base).unwrap();
        let (journal, _) = open(dir.path());
        let chain = persisted(&[h(0)]);
        journal.set_persisted_chain(chain.clone());
        let txs: usize = std::env::var("N42_JOURNAL_BENCH_TXS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1200);
        let rounds = 200u64;
        // The lock write that follows on the same R2 path, for comparison.
        let vote_log =
            crate::persistence::FileVoteLog::open(dir.path().join("vote_log.bin")).unwrap();
        let mut lock_us = Vec::new();
        let mut parent = h(0);
        let mut samples = Vec::new();
        let mut bytes = 0;
        for n in 1..=rounds {
            let hash = B256::from(U256::from(n));
            // A 1200-tx transfer block: ~110-byte signed transactions with
            // incompressible signature/nonce bytes, compressed as the
            // block-data path sends it.
            let mut exec = execution(parent, hash, n, 0);
            let mut state = n.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            *exec.payload.transactions_mut() = (0..txs)
                .map(|_| {
                    Bytes::from(
                        (0..110)
                            .map(|_| {
                                state ^= state << 13;
                                state ^= state >> 7;
                                state ^= state << 17;
                                state as u8
                            })
                            .collect::<Vec<u8>>(),
                    )
                })
                .collect();
            let encoded = crate::orchestrator::encode_execution_payload(&mut exec).unwrap();
            let data = bincode::serialize(&BlockDataBroadcast {
                block_hash: hash,
                view: n,
                payload_json: crate::orchestrator::compress_payload(&encoded.bytes),
                timestamp: n,
                execution_output: None,
                leader_ready_unix_ms: 0,
                native_header_rlp: None,
            })
            .unwrap();
            journal.note_lineage(hash, parent, n);
            journal.stage_data(hash, n, Arc::new(data));
            let started = Instant::now();
            let Durability::Durable(stats) = journal.ensure_durable(hash).unwrap() else {
                panic!("durable");
            };
            samples.push((started.elapsed().as_micros() as u64, stats.sync_us));
            bytes = stats.bytes;
            let lock_started = Instant::now();
            n42_consensus::VoteLogWriter::record_lock(
                &vote_log,
                &n42_primitives::consensus::QuorumCertificate {
                    view: n,
                    block_hash: hash,
                    ..n42_primitives::consensus::QuorumCertificate::genesis()
                },
            )
            .unwrap();
            lock_us.push(lock_started.elapsed().as_micros() as u64);
            // Keep the file small, as pruning does behind reth persistence.
            chain.0.lock().unwrap().push(hash);
            journal.maybe_compact().unwrap();
            parent = hash;
        }
        let pick = |idx: usize, q: f64| {
            let mut v: Vec<u64> = samples
                .iter()
                .map(|s| if idx == 0 { s.0 } else { s.1 })
                .collect();
            v.sort_unstable();
            v[((v.len() as f64 - 1.0) * q) as usize]
        };
        lock_us.sort_unstable();
        println!(
            "record_lock (tmp+fdatasync+rename+dir fsync) us p50={} p99={}",
            lock_us[lock_us.len() / 2],
            lock_us[(lock_us.len() - 1) * 99 / 100],
        );
        println!(
            "block_journal_append_bench dir={} txs={txs} frame_bytes={bytes} rounds={rounds} total_us p50={} p90={} p99={} max={} fdatasync_us p50={} p99={}",
            dir.path().display(),
            pick(0, 0.5),
            pick(0, 0.9),
            pick(0, 0.99),
            pick(0, 1.0),
            pick(1, 0.5),
            pick(1, 0.99),
        );
    }
}
