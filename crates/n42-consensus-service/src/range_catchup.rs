//! Range catch-up client over gov5 `/rpc/bodies_by_range/1` (N42 sync
//! standard sections 3.3-3.5 and 6.4).
//!
//! A node that has fallen far behind fetches contiguous block ranges from
//! several statused peers instead of walking back one block hash at a time.
//! This module is the pure, synchronous state machine; the orchestrator owns
//! the network requests, the QC verification context and the import.
//!
//! - **Activation** happens only on a real gap: an authenticated block whose
//!   parent is missing more than `activate_gap` blocks above the local
//!   execution head. Advertised status heights only size how far to fetch
//!   (the (f+1)-th highest, never one peer's maximum). While inactive no
//!   per-block work is done.
//! - **Screening** of every reply: numbers contiguous from the requested
//!   start, parent links, a decodable body (tx and rewards roots are checked
//!   by the decoder) and a well-formed header `Extra`.
//! - **Anchoring**: a block is executed only when its hash lies on the parent
//!   chain below an anchor. Strict anchors are commit-domain H2-v4 QCs
//!   embedded in fetched headers (checked against their own epoch's set by the
//!   caller, no fallback), live Decides and the engine's committed QC.
//!   `N42_SYNC_ANCHOR=observe` (default) additionally accepts the looser
//!   consensus bindings the reverse walk has always used (a PrepareQC or a TC
//!   high QC) and counts what `enforce` would have refused; `enforce` accepts
//!   strict anchors only; `off` disables the client.
//! - **Unanchored tail**: blocks above the highest anchor are held, at most
//!   `tail_blocks` / `tail_bytes`; on overflow the highest bodies are dropped
//!   (no score) while their parent links are kept, so a later anchor can
//!   authenticate them and they are fetched again.
//! - **Peers**: refusals, busy, empty or short replies and transport failures
//!   back off without a score. Provable faults (broken link, wrong start,
//!   undecodable block, malformed `Extra`, a QC that fails verification
//!   against a known set, more blocks than requested) score; three within ten
//!   minutes exclude the peer for an hour.

use alloy_primitives::{B256, keccak256};
use n42_network::PeerId;
use n42_network::h2_wire::{H2QuorumCertificate, decode_h2_qc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Environment switch for the anchor rule of the range client.
pub const ANCHOR_MODE_ENV: &str = "N42_SYNC_ANCHOR";

const SEAL_BYTES: usize = 96;
const EXTRA_PREFIX: usize = 12;
const FAULT_WINDOW: Duration = Duration::from_secs(600);
const FAULTS_TO_EXCLUDE: usize = 3;
const EXCLUSION: Duration = Duration::from_secs(3600);
const BACKOFF_BASE: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(3600);
const MAX_QC_CACHE: usize = 4096;
const MAX_ANCHORS: usize = 1 << 16;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `N42_SYNC_ANCHOR`: off | observe | enforce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorMode {
    /// The range client is disabled; catch-up is the reverse hash walk.
    Off,
    /// Execute blocks linked to any consensus binding (today's rule) and
    /// count the ones the strict rule would hold back.
    Observe,
    /// Execute only blocks linked to a strict commit-domain anchor.
    Enforce,
}

impl AnchorMode {
    pub fn parse(value: Option<&str>) -> Self {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("off" | "0" | "false" | "disabled") => Self::Off,
            Some("enforce") => Self::Enforce,
            _ => Self::Observe,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::var(ANCHOR_MODE_ENV).ok().as_deref())
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

/// The QC a header `Extra` carries (`"N42H" || view || [QC] || seal(96)`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtraQc {
    /// No QC: a legacy or QC-less producer (n42-26 today), or a view-0 QC.
    Absent,
    Present(H2QuorumCertificate),
    /// `Extra` claims a QC that does not decode: a provable fault.
    Malformed,
}

/// Reads the QC embedded in a header `Extra`. Layouts accepted (standard
/// section 3.2): `magic||view`, `magic||view||seal`, `magic||view||QC` and
/// `magic||view||QC||seal`; a payload longer than the seal is read as QC
/// followed by seal first.
pub fn extra_qc(extra: &[u8]) -> ExtraQc {
    if extra.len() < EXTRA_PREFIX || &extra[..4] != b"N42H" {
        return ExtraQc::Absent;
    }
    let rest = &extra[EXTRA_PREFIX..];
    if rest.is_empty() || rest.len() == SEAL_BYTES {
        return ExtraQc::Absent;
    }
    let decoded = if rest.len() > SEAL_BYTES {
        decode_h2_qc(&rest[..rest.len() - SEAL_BYTES]).or_else(|_| decode_h2_qc(rest))
    } else {
        decode_h2_qc(rest)
    };
    match decoded {
        Ok(qc) if qc.view == 0 => ExtraQc::Absent,
        Ok(qc) => ExtraQc::Present(qc),
        Err(_) => ExtraQc::Malformed,
    }
}

/// Result of checking one embedded QC (the caller resolves the set strictly).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QcCheck {
    Verified,
    /// A valid quorum in the vote/prepare domain: not an anchor, no fault.
    PrepareDomain,
    /// Fails against a known set: a provable fault of the serving peer.
    Invalid,
    /// The set of the QC's epoch is unknown: fail closed, no fault.
    UnknownEpoch,
}

/// One decoded block of a range reply.
#[derive(Clone, Debug)]
pub struct RangeBlock {
    pub number: u64,
    pub hash: B256,
    pub parent: B256,
    /// Header view (`Extra[4..12]`).
    pub view: u64,
    pub qc: ExtraQc,
    pub rlp: Vec<u8>,
}

/// The network outcome of one request, with the blocks already decoded by
/// the caller (an `Err` is an undecodable block at that position).
#[derive(Debug)]
pub enum RangeReply {
    Blocks(Vec<Result<RangeBlock, String>>),
    Refused,
    Failed { unsupported: bool },
}

/// A request the caller must send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeRequest {
    pub peer: PeerId,
    pub start: u64,
    pub count: u64,
}

/// A buffered, authenticated block ready for the importer.
#[derive(Clone, Debug)]
pub struct ReadyBlock {
    pub source: PeerId,
    pub number: u64,
    pub hash: B256,
    pub parent: B256,
    pub view: u64,
    pub rlp: Vec<u8>,
    /// Authenticated only by a loose (non-commit) binding: executed in
    /// observe mode, would be held in enforce mode.
    pub loose_only: bool,
}

/// Why the client stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Deactivation {
    /// The head reached the target and stayed there.
    CaughtUp,
    /// No block arrived for `stall_timeout`; the reverse walk takes over.
    Stalled,
}

#[derive(Clone, Debug)]
pub struct RangeCatchupConfig {
    /// Activate only when the gap exceeds this many blocks.
    pub activate_gap: u64,
    /// Blocks per request (a server may answer with fewer).
    pub request_blocks: u64,
    /// Requests in flight in total, and per peer (standard: at most 2).
    pub max_in_flight: usize,
    pub per_peer_in_flight: usize,
    pub request_timeout: Duration,
    /// Bytes of buffered blocks plus `reply_bytes_estimate` per request in
    /// flight; no new request is sent above it (the reorder bound).
    pub reorder_bytes: usize,
    pub reply_bytes_estimate: usize,
    /// Unanchored tail bound (standard section 3.4 step 6).
    pub tail_blocks: usize,
    pub tail_bytes: usize,
    /// Parent-link skeleton bound; also the farthest look-ahead above the head.
    pub max_links: usize,
    pub stall_timeout: Duration,
    /// After a stall, refuse to re-activate for this long.
    pub stall_cooldown: Duration,
}

impl Default for RangeCatchupConfig {
    fn default() -> Self {
        Self {
            activate_gap: 32,
            request_blocks: 256,
            max_in_flight: 4,
            per_peer_in_flight: 1,
            request_timeout: Duration::from_secs(35),
            reorder_bytes: 1 << 30,
            reply_bytes_estimate: 128 << 20,
            tail_blocks: 64,
            tail_bytes: 512 << 20,
            max_links: 1 << 16,
            stall_timeout: Duration::from_secs(90),
            stall_cooldown: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Default)]
struct PeerState {
    height: u64,
    in_flight: usize,
    failures: u32,
    backoff_until: Option<Instant>,
    faults: VecDeque<Instant>,
    excluded_until: Option<Instant>,
    /// EWMA (alpha 0.3) of delivered bytes per second.
    throughput: f64,
}

impl PeerState {
    fn available(&self, now: Instant) -> bool {
        self.excluded_until.is_none_or(|until| now >= until)
            && self.backoff_until.is_none_or(|until| now >= until)
    }
}

#[derive(Clone, Debug)]
struct Buffered {
    source: PeerId,
    hash: B256,
    parent: B256,
    view: u64,
    rlp: Vec<u8>,
}

/// A bounded insertion-ordered hash set.
#[derive(Debug, Default)]
struct HashBook {
    set: HashSet<B256>,
    order: VecDeque<B256>,
}

impl HashBook {
    fn contains(&self, hash: &B256) -> bool {
        self.set.contains(hash)
    }

    fn insert(&mut self, hash: B256, limit: usize) -> bool {
        if !self.set.insert(hash) {
            return false;
        }
        self.order.push_back(hash);
        while self.order.len() > limit {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }

    fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }
}

/// Counters for logs, metrics and tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RangeCatchupStats {
    pub requests: u64,
    pub blocks_received: u64,
    pub faults: u64,
    pub soft_failures: u64,
    pub qc_verifications: u64,
    pub tail_evicted: u64,
    pub released: u64,
    /// Released in observe mode on a loose anchor only (enforce would hold).
    pub released_loose_only: u64,
    pub prepare_domain_qcs: u64,
    pub unknown_epoch_qcs: u64,
}

/// What [`RangeCatchup::on_reply`] concluded about one reply.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplyReport {
    pub accepted: usize,
    pub fault: Option<&'static str>,
    pub peer_excluded: bool,
}

pub struct RangeCatchup {
    config: RangeCatchupConfig,
    mode: AnchorMode,
    active: bool,
    target: u64,
    head_number: u64,
    peers: HashMap<PeerId, PeerState>,
    /// Unassigned work: start -> count.
    work: BTreeMap<u64, u64>,
    next_plan: u64,
    in_flight: HashMap<(PeerId, u64), (u64, Instant)>,
    buffer: BTreeMap<u64, Buffered>,
    buffer_bytes: usize,
    links: HashMap<B256, (B256, u64)>,
    link_order: VecDeque<B256>,
    link_at: BTreeMap<u64, B256>,
    strict: HashBook,
    loose: HashBook,
    evicted: BTreeSet<u64>,
    qc_cache: HashMap<B256, QcCheck>,
    qc_cache_order: VecDeque<B256>,
    settled: u32,
    import_failures: u32,
    /// Poll at once (after activation or a reply) instead of at the tick.
    kick: bool,
    /// Highest height the caller lets this client import (last poll).
    stop_at: Option<u64>,
    last_progress: Option<Instant>,
    cooldown_until: Option<Instant>,
    stats: RangeCatchupStats,
}

impl RangeCatchup {
    pub fn new(config: RangeCatchupConfig, mode: AnchorMode) -> Self {
        Self {
            config,
            mode,
            active: false,
            target: 0,
            head_number: 0,
            peers: HashMap::new(),
            work: BTreeMap::new(),
            next_plan: 0,
            in_flight: HashMap::new(),
            buffer: BTreeMap::new(),
            buffer_bytes: 0,
            links: HashMap::new(),
            link_order: VecDeque::new(),
            link_at: BTreeMap::new(),
            strict: HashBook::default(),
            loose: HashBook::default(),
            evicted: BTreeSet::new(),
            qc_cache: HashMap::new(),
            qc_cache_order: VecDeque::new(),
            settled: 0,
            import_failures: 0,
            kick: false,
            stop_at: None,
            last_progress: None,
            cooldown_until: None,
            stats: RangeCatchupStats::default(),
        }
    }

    pub const fn mode(&self) -> AnchorMode {
        self.mode
    }

    pub const fn is_active(&self) -> bool {
        self.active
    }

    pub const fn target(&self) -> u64 {
        self.target
    }

    pub fn stats(&self) -> &RangeCatchupStats {
        &self.stats
    }

    pub fn buffered_blocks(&self) -> usize {
        self.buffer.len()
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer_bytes
    }

    /// A peer completed a status exchange on this genesis. Its height is a
    /// fetch-sizing hint only.
    pub fn note_peer_status(&mut self, peer: PeerId, height: u64) {
        self.peers.entry(peer).or_default().height = height;
    }

    pub fn peer_disconnected(&mut self, peer: &PeerId) {
        self.peers.remove(peer);
        let lost: Vec<(PeerId, u64)> = self
            .in_flight
            .keys()
            .filter(|(owner, _)| owner == peer)
            .copied()
            .collect();
        for key in lost {
            if let Some((count, _)) = self.in_flight.remove(&key) {
                self.requeue(key.1, count);
            }
        }
    }

    /// The `k`-th highest advertised height (k = f+1), if that many peers
    /// reported one. Never authenticates anything.
    pub fn quorum_height(&self, k: usize) -> Option<u64> {
        if k == 0 {
            return None;
        }
        let mut heights: Vec<u64> = self.peers.values().map(|peer| peer.height).collect();
        if heights.len() < k {
            return None;
        }
        heights.sort_unstable_by(|a, b| b.cmp(a));
        Some(heights[k - 1])
    }

    /// Start (or extend) catch-up toward `target`. Returns whether the client
    /// is active afterwards. Inactive unless `target - head > activate_gap`.
    pub fn activate(&mut self, head_number: u64, target: u64, now: Instant) -> bool {
        if self.mode == AnchorMode::Off {
            return false;
        }
        if self.active {
            self.target = self.target.max(target);
            return true;
        }
        if self.cooldown_until.is_some_and(|until| now < until) {
            return false;
        }
        if target <= head_number.saturating_add(self.config.activate_gap) {
            return false;
        }
        self.reset_round();
        self.active = true;
        self.target = target;
        self.head_number = head_number;
        self.next_plan = head_number.saturating_add(1);
        self.last_progress = Some(now);
        self.kick = true;
        true
    }

    /// A hash named by a commit-domain QC the node verified (a Decide, the
    /// engine's committed QC, or a checked header QC).
    pub fn note_strict_anchor(&mut self, hash: B256) {
        if hash != B256::ZERO {
            mark(&mut self.strict, &self.links, hash);
        }
    }

    /// A hash bound by a weaker consensus message (PrepareQC, a TC high QC,
    /// or the parent of a bound block). Executable in observe mode only.
    pub fn note_loose_anchor(&mut self, hash: B256) {
        if hash != B256::ZERO {
            mark(&mut self.loose, &self.links, hash);
        }
    }

    /// The parent link of a block known outside this client (the bound block
    /// that revealed the gap), so anchors above it reach the fetched chain.
    pub fn note_link(&mut self, hash: B256, parent: B256, number: u64) {
        self.record_link(hash, parent, number);
        if self.strict.contains(&hash) {
            mark(&mut self.strict, &self.links, hash);
        }
        if self.loose.contains(&hash) {
            mark(&mut self.loose, &self.links, hash);
        }
    }

    /// Earliest instant [`Self::poll`] should run again; `None` while idle.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.active {
            return None;
        }
        let mut deadline = now + POLL_INTERVAL;
        for (_, sent) in self.in_flight.values() {
            deadline = deadline.min(*sent + self.config.request_timeout);
        }
        if self.kick || self.has_ready() {
            deadline = now;
        }
        Some(deadline)
    }

    fn has_ready(&self) -> bool {
        let next = self.head_number.saturating_add(1);
        self.stop_at.is_none_or(|stop| next <= stop)
            && self
                .buffer
                .get(&next)
                .is_some_and(|block| self.executable(&block.hash))
    }

    /// Expire timed-out requests, plan work and assign it to peers.
    /// `head_number` is the local execution head; `status_quorum` is f+1;
    /// `stop_at` is the highest height this client may fetch and import (the
    /// live path owns everything above it).
    pub fn poll(
        &mut self,
        now: Instant,
        head_number: u64,
        status_quorum: usize,
        stop_at: Option<u64>,
    ) -> (Vec<RangeRequest>, Option<Deactivation>) {
        if !self.active {
            return (Vec::new(), None);
        }
        self.kick = false;
        self.stop_at = stop_at;
        self.advance_head(head_number);
        if let Some(height) = self.quorum_height(status_quorum) {
            self.target = self.target.max(height);
        }

        let expired: Vec<(PeerId, u64)> = self
            .in_flight
            .iter()
            .filter(|(_, (_, sent))| {
                now.saturating_duration_since(*sent) >= self.config.request_timeout
            })
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            if let Some((count, _)) = self.in_flight.remove(&key) {
                if let Some(state) = self.peers.get_mut(&key.0) {
                    state.in_flight = state.in_flight.saturating_sub(1);
                }
                self.soft_fail(&key.0, now, false);
                self.requeue(key.1, count);
            }
        }

        let goal = stop_at.map_or(self.target, |stop| stop.min(self.target));
        if head_number >= goal.saturating_sub(2) && !self.has_ready() {
            self.settled += 1;
            if self.settled >= 3 {
                self.deactivate();
                return (Vec::new(), Some(Deactivation::CaughtUp));
            }
        } else {
            self.settled = 0;
        }
        if self
            .last_progress
            .is_some_and(|at| now.saturating_duration_since(at) >= self.config.stall_timeout)
        {
            self.deactivate();
            self.cooldown_until = Some(now + self.config.stall_cooldown);
            return (Vec::new(), Some(Deactivation::Stalled));
        }

        self.plan(goal);
        (self.assign(now), None)
    }

    fn advance_head(&mut self, head_number: u64) {
        if head_number <= self.head_number && self.head_number != 0 {
            return;
        }
        if head_number > self.head_number {
            self.import_failures = 0;
        }
        self.head_number = head_number;
        while let Some((&number, _)) = self.buffer.first_key_value() {
            if number > head_number {
                break;
            }
            if let Some(dropped) = self.buffer.remove(&number) {
                self.buffer_bytes = self.buffer_bytes.saturating_sub(dropped.rlp.len());
            }
        }
        self.evicted = self.evicted.split_off(&head_number.saturating_add(1));
        let stale: Vec<u64> = self
            .work
            .range(..=head_number)
            .map(|(start, _)| *start)
            .collect();
        for start in stale {
            if let Some(count) = self.work.remove(&start) {
                let end = start.saturating_add(count);
                if end > head_number + 1 {
                    self.work.insert(head_number + 1, end - head_number - 1);
                }
            }
        }
        self.next_plan = self.next_plan.max(head_number.saturating_add(1));
    }

    fn plan(&mut self, goal: u64) {
        // Bodies dropped by the tail bound become fetchable once a later
        // anchor authenticates the link recorded at their height.
        let refetch: Vec<u64> = self
            .evicted
            .iter()
            .copied()
            .filter(|number| {
                self.link_at
                    .get(number)
                    .is_some_and(|hash| self.executable(hash))
            })
            .collect();
        for number in refetch.into_iter().rev() {
            self.evicted.remove(&number);
            self.requeue(number, 1);
        }
        let window_end = goal.min(
            self.head_number
                .saturating_add(self.config.max_links as u64),
        );
        let max_work = self.config.max_in_flight.saturating_mul(2).max(1);
        while self.next_plan <= window_end && self.work.len() < max_work {
            let count = self
                .config
                .request_blocks
                .min(window_end - self.next_plan + 1)
                .max(1);
            self.work.insert(self.next_plan, count);
            self.next_plan = self.next_plan.saturating_add(count);
        }
    }

    /// Re-adds `[start, start+count)` as unassigned work, merged with an
    /// adjacent following item so short replies do not fragment the queue.
    fn requeue(&mut self, start: u64, count: u64) {
        if count == 0 || start.saturating_add(count) <= self.head_number.saturating_add(1) {
            return;
        }
        let start = start.max(self.head_number.saturating_add(1));
        let mut count = count;
        let end = start.saturating_add(count);
        if let Some(next) = self.work.remove(&end) {
            count = count.saturating_add(next).min(1024);
        }
        let existing = self.work.entry(start).or_insert(0);
        *existing = (*existing).max(count);
    }

    /// First height above the head that is neither buffered nor executed.
    fn first_missing(&self) -> u64 {
        let mut number = self.head_number.saturating_add(1);
        while self.buffer.contains_key(&number) {
            number += 1;
        }
        number
    }

    fn assign(&mut self, now: Instant) -> Vec<RangeRequest> {
        let mut requests = Vec::new();
        let first_missing = self.first_missing();
        loop {
            if self.in_flight.len() >= self.config.max_in_flight {
                break;
            }
            let Some((&start, &count)) = self.work.first_key_value() else {
                break;
            };
            let inflight_bytes = self
                .in_flight
                .len()
                .saturating_mul(self.config.reply_bytes_estimate);
            // The reorder bound never blocks the block the importer waits for.
            if start > first_missing
                && self.buffer_bytes.saturating_add(inflight_bytes) >= self.config.reorder_bytes
            {
                break;
            }
            let Some(peer) = self.pick_peer(start, count, now) else {
                break;
            };
            self.work.remove(&start);
            let count = count.min(1024);
            self.in_flight.insert((peer, start), (count, now));
            if let Some(state) = self.peers.get_mut(&peer) {
                state.in_flight += 1;
            }
            self.stats.requests += 1;
            requests.push(RangeRequest { peer, start, count });
        }
        requests
    }

    /// Best available peer: those advertising the whole range first, then by
    /// throughput. Peers advertising less are still used (status heights are
    /// only refreshed on connect).
    fn pick_peer(&self, start: u64, count: u64, now: Instant) -> Option<PeerId> {
        let end = start.saturating_add(count - 1);
        self.peers
            .iter()
            .filter(|(peer, state)| {
                state.available(now)
                    && state.in_flight < self.config.per_peer_in_flight
                    && !self.in_flight.contains_key(&(**peer, start))
            })
            .max_by(|(a_id, a), (b_id, b)| {
                let tier = |state: &PeerState| {
                    u8::from(state.height >= end) * 2 + u8::from(state.height >= start)
                };
                tier(a)
                    .cmp(&tier(b))
                    .then(a.throughput.total_cmp(&b.throughput))
                    .then(b_id.cmp(a_id))
            })
            .map(|(peer, _)| *peer)
    }

    fn soft_fail(&mut self, peer: &PeerId, now: Instant, unsupported: bool) {
        self.stats.soft_failures += 1;
        if let Some(state) = self.peers.get_mut(peer) {
            state.failures = state.failures.saturating_add(1);
            let backoff = if unsupported {
                UNSUPPORTED_BACKOFF
            } else {
                BACKOFF_BASE
                    .saturating_mul(1u32 << state.failures.saturating_sub(1).min(8))
                    .min(BACKOFF_MAX)
            };
            state.backoff_until = Some(now + backoff);
        }
    }

    fn fault(&mut self, peer: &PeerId, now: Instant) -> bool {
        self.stats.faults += 1;
        let Some(state) = self.peers.get_mut(peer) else {
            return false;
        };
        while state
            .faults
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > FAULT_WINDOW)
        {
            state.faults.pop_front();
        }
        state.faults.push_back(now);
        if state.faults.len() >= FAULTS_TO_EXCLUDE {
            state.excluded_until = Some(now + EXCLUSION);
            state.faults.clear();
            return true;
        }
        false
    }

    /// Screens, anchors and buffers one reply. `verify` checks an embedded
    /// QC strictly (commit domain, the set of the QC's own epoch).
    pub fn on_reply(
        &mut self,
        peer: PeerId,
        start: u64,
        reply: RangeReply,
        now: Instant,
        verify: &mut dyn FnMut(&H2QuorumCertificate) -> QcCheck,
    ) -> ReplyReport {
        let mut report = ReplyReport::default();
        let Some((count, sent)) = self.in_flight.remove(&(peer, start)) else {
            return report;
        };
        if let Some(state) = self.peers.get_mut(&peer) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        if !self.active {
            return report;
        }
        let blocks = match reply {
            RangeReply::Refused => {
                self.soft_fail(&peer, now, false);
                self.requeue(start, count);
                return report;
            }
            RangeReply::Failed { unsupported } => {
                self.soft_fail(&peer, now, unsupported);
                self.requeue(start, count);
                return report;
            }
            RangeReply::Blocks(blocks) if blocks.is_empty() => {
                // A clean empty reply: the peer does not hold the range.
                self.soft_fail(&peer, now, false);
                self.requeue(start, count);
                return report;
            }
            RangeReply::Blocks(blocks) => blocks,
        };

        let enforce = self.mode == AnchorMode::Enforce;
        let mut fault = None;
        let served = blocks.len();
        let mut kept: Vec<RangeBlock> = Vec::with_capacity(served.min(count as usize));
        for (index, block) in blocks.into_iter().enumerate() {
            if index as u64 >= count {
                fault = Some("more blocks than requested");
                break;
            }
            let block = match block {
                Ok(block) => block,
                Err(_) => {
                    fault = Some("invalid block");
                    break;
                }
            };
            let linked = match kept.last() {
                None => block.number == start,
                Some(previous) => {
                    block.number == previous.number + 1 && block.parent == previous.hash
                }
            };
            if !linked {
                fault = Some(if kept.is_empty() {
                    "wrong start"
                } else {
                    "broken link"
                });
                break;
            }
            if matches!(block.qc, ExtraQc::Malformed) {
                fault = Some("malformed extra");
                break;
            }
            kept.push(block);
        }
        self.stats.blocks_received += kept.len() as u64;

        // Harvest anchors before deciding: links first, so a QC naming any
        // block of this reply authenticates everything below it.
        for block in &kept {
            self.record_link(block.hash, block.parent, block.number);
        }
        for book in [&mut self.strict, &mut self.loose] {
            if let Some(top) = kept.iter().rev().find(|block| book.contains(&block.hash)) {
                mark(book, &self.links, top.hash);
            }
        }
        if fault.is_none() || !enforce {
            for index in (0..kept.len()).rev() {
                if kept
                    .first()
                    .is_some_and(|lowest| self.strict.contains(&lowest.hash))
                {
                    break;
                }
                let ExtraQc::Present(qc) = &kept[index].qc else {
                    continue;
                };
                let useful = self
                    .links
                    .get(&qc.block_hash)
                    .is_some_and(|(_, number)| *number > self.head_number);
                if self.strict.contains(&qc.block_hash) || !useful {
                    // Already anchored, executed, or names nothing held here.
                    continue;
                }
                match self.check_qc(qc, verify) {
                    QcCheck::Verified => {
                        mark(&mut self.strict, &self.links, qc.block_hash);
                    }
                    QcCheck::PrepareDomain => self.stats.prepare_domain_qcs += 1,
                    QcCheck::UnknownEpoch => self.stats.unknown_epoch_qcs += 1,
                    QcCheck::Invalid => {
                        fault.get_or_insert("bad qc");
                        if enforce {
                            break;
                        }
                    }
                }
            }
        }

        if let Some(reason) = fault {
            report.fault = Some(reason);
            report.peer_excluded = self.fault(&peer, now);
            if enforce {
                // The whole reply is dropped; its links stay (they are only
                // ever authenticated from above by a verified anchor).
                self.requeue(start, count);
                return report;
            }
        }

        let accepted = kept.len();
        report.accepted = accepted;
        let delivered_bytes: usize = kept.iter().map(|block| block.rlp.len()).sum();
        for block in kept {
            if block.number <= self.head_number {
                continue;
            }
            self.evicted.remove(&block.number);
            match self.buffer.get(&block.number) {
                Some(existing) if existing.hash == block.hash => continue,
                Some(existing) if self.executable(&existing.hash) => continue,
                Some(_) if !self.executable(&block.hash) => continue,
                _ => {}
            }
            self.buffer_bytes = self.buffer_bytes.saturating_add(block.rlp.len());
            if let Some(replaced) = self.buffer.insert(
                block.number,
                Buffered {
                    source: peer,
                    hash: block.hash,
                    parent: block.parent,
                    view: block.view,
                    rlp: block.rlp,
                },
            ) {
                self.buffer_bytes = self.buffer_bytes.saturating_sub(replaced.rlp.len());
            }
        }
        if (accepted as u64) < count {
            // A short reply is not a fault: ask again from the next number.
            self.requeue(
                start.saturating_add(accepted as u64),
                count - accepted as u64,
            );
        }
        self.kick = true;
        if accepted > 0 {
            self.last_progress = Some(now);
            if let Some(state) = self.peers.get_mut(&peer) {
                state.failures = 0;
                state.backoff_until = None;
                let seconds = now.saturating_duration_since(sent).as_secs_f64().max(0.001);
                let sample = delivered_bytes as f64 / seconds;
                state.throughput = if state.throughput == 0.0 {
                    sample
                } else {
                    0.7 * state.throughput + 0.3 * sample
                };
            }
        }
        self.bound_tail();
        report
    }

    fn check_qc(
        &mut self,
        qc: &H2QuorumCertificate,
        verify: &mut dyn FnMut(&H2QuorumCertificate) -> QcCheck,
    ) -> QcCheck {
        let key = qc_key(qc);
        if let Some(cached) = self.qc_cache.get(&key) {
            return *cached;
        }
        self.stats.qc_verifications += 1;
        let verdict = verify(qc);
        if verdict != QcCheck::UnknownEpoch {
            // An unknown set may become known later; every other verdict is final.
            self.qc_cache.insert(key, verdict);
            self.qc_cache_order.push_back(key);
            while self.qc_cache_order.len() > MAX_QC_CACHE {
                if let Some(oldest) = self.qc_cache_order.pop_front() {
                    self.qc_cache.remove(&oldest);
                }
            }
        }
        verdict
    }

    fn executable(&self, hash: &B256) -> bool {
        self.strict.contains(hash)
            || (self.mode == AnchorMode::Observe && self.loose.contains(hash))
    }

    /// Holds at most `tail_blocks` / `tail_bytes` of unauthenticated bodies;
    /// the highest go first. Their links stay for a later anchor.
    fn bound_tail(&mut self) {
        let mut held_blocks = 0usize;
        let mut held_bytes = 0usize;
        let mut evict = Vec::new();
        for (number, block) in &self.buffer {
            if self.executable(&block.hash) {
                continue;
            }
            if held_blocks < self.config.tail_blocks
                && (held_blocks == 0 || held_bytes + block.rlp.len() <= self.config.tail_bytes)
            {
                held_blocks += 1;
                held_bytes += block.rlp.len();
            } else {
                evict.push(*number);
            }
        }
        for number in evict {
            if let Some(dropped) = self.buffer.remove(&number) {
                self.buffer_bytes = self.buffer_bytes.saturating_sub(dropped.rlp.len());
                self.evicted.insert(number);
                self.stats.tail_evicted += 1;
            }
        }
    }

    fn record_link(&mut self, hash: B256, parent: B256, number: u64) {
        if self.links.insert(hash, (parent, number)).is_none() {
            self.link_order.push_back(hash);
            while self.link_order.len() > self.config.max_links {
                if let Some(oldest) = self.link_order.pop_front()
                    && let Some((_, old_number)) = self.links.remove(&oldest)
                    && self.link_at.get(&old_number) == Some(&oldest)
                {
                    self.link_at.remove(&old_number);
                }
            }
        }
        // On a fork keep an executable link at the height, else the latest.
        let keep = self
            .link_at
            .get(&number)
            .is_some_and(|existing| *existing != hash && self.executable(existing));
        if !keep {
            self.link_at.insert(number, hash);
        }
    }

    /// Pops the authenticated blocks that extend `(head_number, head_hash)`,
    /// in order, at most `max` and never above `stop_at`. A buffered block
    /// that does not extend the head (another branch) is dropped and fetched
    /// again.
    pub fn take_ready(
        &mut self,
        head_number: u64,
        head_hash: B256,
        stop_at: Option<u64>,
        max: usize,
    ) -> Vec<ReadyBlock> {
        if !self.active {
            return Vec::new();
        }
        self.stop_at = stop_at;
        self.advance_head(head_number);
        let mut ready = Vec::new();
        let (mut number, mut hash) = (head_number, head_hash);
        while ready.len() < max {
            let next = number.saturating_add(1);
            if stop_at.is_some_and(|stop| next > stop) {
                break;
            }
            let Some(block) = self.buffer.get(&next) else {
                break;
            };
            if block.parent != hash {
                if let Some(dropped) = self.buffer.remove(&next) {
                    self.buffer_bytes = self.buffer_bytes.saturating_sub(dropped.rlp.len());
                }
                self.requeue(next, 1);
                break;
            }
            if !self.executable(&block.hash) {
                break;
            }
            let loose_only = !self.strict.contains(&block.hash);
            let Some(block) = self.buffer.remove(&next) else {
                break;
            };
            self.buffer_bytes = self.buffer_bytes.saturating_sub(block.rlp.len());
            self.stats.released += 1;
            if loose_only {
                self.stats.released_loose_only += 1;
            }
            number = next;
            hash = block.hash;
            ready.push(ReadyBlock {
                source: block.source,
                number: next,
                hash: block.hash,
                parent: block.parent,
                view: block.view,
                rlp: block.rlp,
                loose_only,
            });
        }
        ready
    }

    /// The importer could not execute the block at `number`: drop what is
    /// buffered from there and fetch it again. Three failures without the
    /// head moving stop the client (the reverse walk takes over).
    pub fn import_failed(&mut self, number: u64, now: Instant) -> Option<Deactivation> {
        let dropped = self.buffer.split_off(&number);
        for block in dropped.values() {
            self.buffer_bytes = self.buffer_bytes.saturating_sub(block.rlp.len());
        }
        self.import_failures += 1;
        if self.import_failures >= 3 {
            self.deactivate();
            self.cooldown_until = Some(now + self.config.stall_cooldown);
            return Some(Deactivation::Stalled);
        }
        self.requeue(number, self.config.request_blocks);
        None
    }

    fn reset_round(&mut self) {
        self.work.clear();
        self.in_flight.clear();
        for state in self.peers.values_mut() {
            state.in_flight = 0;
        }
        self.buffer.clear();
        self.buffer_bytes = 0;
        self.evicted.clear();
        self.settled = 0;
        self.import_failures = 0;
    }

    fn deactivate(&mut self) {
        self.active = false;
        self.reset_round();
        self.links.clear();
        self.link_order.clear();
        self.link_at.clear();
        self.strict.clear();
        self.loose.clear();
    }
}

/// Authenticates `hash` and walks the parent links downward, stopping at a
/// missing link or an already authenticated hash.
fn mark(book: &mut HashBook, links: &HashMap<B256, (B256, u64)>, hash: B256) {
    let mut current = hash;
    let mut first = true;
    loop {
        if !book.insert(current, MAX_ANCHORS) && !first {
            return;
        }
        first = false;
        let Some((parent, _)) = links.get(&current) else {
            return;
        };
        current = *parent;
    }
}

fn qc_key(qc: &H2QuorumCertificate) -> B256 {
    let mut bytes =
        Vec::with_capacity(8 + 32 + qc.aggregate_signature.len() + qc.signers_bitmap.len());
    bytes.extend_from_slice(&qc.view.to_le_bytes());
    bytes.extend_from_slice(qc.block_hash.as_slice());
    bytes.extend_from_slice(&qc.aggregate_signature);
    bytes.extend_from_slice(&qc.signers_bitmap);
    keccak256(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIED: u8 = 1;
    const PREPARE: u8 = 2;
    const INVALID: u8 = 3;
    const UNKNOWN: u8 = 4;

    fn qc(view: u64, block_hash: B256, marker: u8) -> H2QuorumCertificate {
        H2QuorumCertificate {
            view,
            block_hash,
            aggregate_signature: vec![marker; 96],
            signers_bitmap: vec![4, 0, 0b0111],
        }
    }

    fn verifier(calls: &mut u32) -> impl FnMut(&H2QuorumCertificate) -> QcCheck + '_ {
        move |qc| {
            *calls += 1;
            match qc.aggregate_signature[0] {
                VERIFIED => QcCheck::Verified,
                PREPARE => QcCheck::PrepareDomain,
                INVALID => QcCheck::Invalid,
                _ => QcCheck::UnknownEpoch,
            }
        }
    }

    fn chain(first: u64, parent: B256, len: u64, salt: u8) -> Vec<RangeBlock> {
        let mut parent = parent;
        (first..first + len)
            .map(|number| {
                let mut seed = number.to_le_bytes().to_vec();
                seed.extend_from_slice(parent.as_slice());
                seed.push(salt);
                let block = RangeBlock {
                    number,
                    hash: keccak256(seed),
                    parent,
                    view: number,
                    qc: ExtraQc::Absent,
                    rlp: vec![salt; 100],
                };
                parent = block.hash;
                block
            })
            .collect()
    }

    /// Every block carries a commit QC naming its parent (gov5 leaders).
    fn with_parent_qcs(mut blocks: Vec<RangeBlock>, marker: u8) -> Vec<RangeBlock> {
        for block in &mut blocks {
            block.qc = ExtraQc::Present(qc(block.view.max(1), block.parent, marker));
        }
        blocks
    }

    fn reply(blocks: &[RangeBlock]) -> RangeReply {
        RangeReply::Blocks(blocks.iter().cloned().map(Ok).collect())
    }

    fn config() -> RangeCatchupConfig {
        RangeCatchupConfig {
            activate_gap: 8,
            request_blocks: 16,
            max_in_flight: 2,
            ..RangeCatchupConfig::default()
        }
    }

    const GENESIS: B256 = B256::repeat_byte(0xee);

    fn active(mode: AnchorMode, peers: &[PeerId], target: u64) -> (RangeCatchup, Instant) {
        let mut client = RangeCatchup::new(config(), mode);
        for peer in peers {
            client.note_peer_status(*peer, target);
        }
        let now = Instant::now();
        assert!(client.activate(0, target, now));
        (client, now)
    }

    fn request_for(requests: &[RangeRequest], peer: PeerId) -> RangeRequest {
        *requests
            .iter()
            .find(|request| request.peer == peer)
            .expect("request for peer")
    }

    #[test]
    fn anchor_mode_parses_with_observe_default() {
        assert_eq!(AnchorMode::parse(None), AnchorMode::Observe);
        assert_eq!(AnchorMode::parse(Some("")), AnchorMode::Observe);
        assert_eq!(AnchorMode::parse(Some(" Enforce ")), AnchorMode::Enforce);
        assert_eq!(AnchorMode::parse(Some("off")), AnchorMode::Off);
        assert_eq!(AnchorMode::parse(Some("observe")), AnchorMode::Observe);
    }

    fn encode_qc(qc: &H2QuorumCertificate) -> Vec<u8> {
        let mut out = qc.view.to_le_bytes().to_vec();
        for field in [
            qc.block_hash.as_slice(),
            &qc.aggregate_signature,
            &qc.signers_bitmap,
        ] {
            out.extend_from_slice(&(field.len() as u32).to_le_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    #[test]
    fn extra_qc_reads_every_layout() {
        let mut prefix = b"N42H".to_vec();
        prefix.extend_from_slice(&7u64.to_le_bytes());
        let anchor = qc(6, B256::repeat_byte(3), VERIFIED);
        let wire = encode_qc(&anchor);

        assert_eq!(extra_qc(&prefix), ExtraQc::Absent);
        assert_eq!(
            extra_qc(&[prefix.clone(), vec![0; 96]].concat()),
            ExtraQc::Absent
        );
        assert_eq!(
            extra_qc(&[prefix.clone(), wire.clone(), vec![0; 96]].concat()),
            ExtraQc::Present(anchor.clone())
        );
        assert_eq!(
            extra_qc(&[prefix.clone(), wire.clone()].concat()),
            ExtraQc::Present(anchor)
        );
        assert_eq!(
            extra_qc(&[prefix.clone(), vec![1; 40]].concat()),
            ExtraQc::Malformed
        );
        let genesis = H2QuorumCertificate {
            view: 0,
            block_hash: B256::ZERO,
            aggregate_signature: Vec::new(),
            signers_bitmap: Vec::new(),
        };
        assert_eq!(
            extra_qc(&[prefix, encode_qc(&genesis), vec![0; 96]].concat()),
            ExtraQc::Absent
        );
        assert_eq!(extra_qc(b"not a consensus header"), ExtraQc::Absent);
    }

    #[test]
    fn activates_only_on_a_real_gap_and_never_when_off() {
        let now = Instant::now();
        let mut client = RangeCatchup::new(config(), AnchorMode::Observe);
        assert!(
            !client.activate(100, 108, now),
            "a gap at the threshold is not catch-up"
        );
        assert!(!client.is_active());
        assert_eq!(
            client.next_deadline(now),
            None,
            "idle client schedules nothing"
        );
        assert!(client.activate(100, 109, now));
        let mut off = RangeCatchup::new(config(), AnchorMode::Off);
        assert!(!off.activate(0, 1_000_000, now));
    }

    #[test]
    fn status_heights_size_the_target_by_the_f_plus_1_th_peer_only() {
        let peers: Vec<PeerId> = (0..4).map(|_| PeerId::random()).collect();
        let mut client = RangeCatchup::new(config(), AnchorMode::Observe);
        for (peer, height) in peers.iter().zip([1_000_000, 50, 40, 30]) {
            client.note_peer_status(*peer, height);
        }
        assert_eq!(
            client.quorum_height(2),
            Some(50),
            "one lying peer cannot set the target"
        );
        let now = Instant::now();
        assert!(client.activate(0, 20, now));
        client.poll(now, 0, 2, None);
        assert_eq!(client.target(), 50);
    }

    #[test]
    fn short_replies_keep_the_prefix_and_refusals_are_not_scored() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        let blocks = with_parent_qcs(chain(1, GENESIS, 40, 1), VERIFIED);
        let (requests, _) = client.poll(now, 0, 1, None);
        assert_eq!(
            requests,
            vec![RangeRequest {
                peer,
                start: 1,
                count: 16
            }]
        );
        let mut calls = 0;
        let report = client.on_reply(peer, 1, reply(&blocks[..4]), now, &mut verifier(&mut calls));
        assert_eq!(
            report,
            ReplyReport {
                accepted: 4,
                fault: None,
                peer_excluded: false
            }
        );
        // Block 4's QC anchors 3: blocks 1..=3 execute, 4 waits for an anchor.
        let ready = client.take_ready(0, GENESIS, None, 64);
        assert_eq!(
            ready.iter().map(|block| block.number).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let (requests, _) = client.poll(now, 3, 1, None);
        assert_eq!(requests[0].start, 5, "the remainder is asked for again");

        for reply in [
            RangeReply::Refused,
            RangeReply::Blocks(Vec::new()),
            RangeReply::Failed { unsupported: false },
        ] {
            client.in_flight.insert((peer, 5), (12, now));
            client.peers.get_mut(&peer).unwrap().in_flight = 1;
            let report = client.on_reply(peer, 5, reply, now, &mut verifier(&mut calls));
            assert_eq!(report.fault, None);
            assert!(client.work.contains_key(&5), "the range is asked for again");
        }
        assert_eq!(
            client.stats().faults,
            0,
            "refused, empty and failed replies carry no score"
        );
        assert!(client.peers[&peer].excluded_until.is_none());
        assert!(
            client.peers[&peer].backoff_until.is_some(),
            "they back off instead"
        );
    }

    #[test]
    fn broken_link_is_refused_in_enforce_and_counted_in_observe() {
        let good = chain(1, GENESIS, 6, 1);
        let mut forged = good.clone();
        forged[3] = chain(4, B256::repeat_byte(9), 1, 2).remove(0);

        for mode in [AnchorMode::Enforce, AnchorMode::Observe] {
            let peer = PeerId::random();
            let (mut client, now) = active(mode, &[peer], 40);
            client.poll(now, 0, 1, None);
            let mut calls = 0;
            let report = client.on_reply(peer, 1, reply(&forged), now, &mut verifier(&mut calls));
            assert_eq!(report.fault, Some("broken link"));
            assert_eq!(client.stats().faults, 1);
            if mode == AnchorMode::Enforce {
                assert_eq!(report.accepted, 0);
                assert_eq!(client.buffered_blocks(), 0, "enforce drops the whole reply");
            } else {
                assert_eq!(report.accepted, 3, "observe keeps the linked prefix");
            }
        }
    }

    #[test]
    fn three_faults_exclude_the_peer_for_an_hour() {
        let bad = PeerId::random();
        let honest = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[bad], 400);
        client.note_peer_status(honest, 0);
        let mut calls = 0;
        for round in 0..3u64 {
            let at = now + Duration::from_secs(round);
            client.in_flight.insert((bad, 1), (16, at));
            let report = client.on_reply(
                bad,
                1,
                reply(&chain(2, GENESIS, 3, 1)),
                at,
                &mut verifier(&mut calls),
            );
            assert_eq!(report.fault, Some("wrong start"));
            assert_eq!(report.peer_excluded, round == 2);
        }
        let (requests, _) = client.poll(now + Duration::from_secs(10), 0, 1, None);
        assert!(requests.iter().all(|request| request.peer != bad));
        assert!(requests.iter().any(|request| request.peer == honest));
    }

    #[test]
    fn forged_chain_with_a_copied_qc_is_never_released() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        client.poll(now, 0, 1, None);
        let real_hash = B256::repeat_byte(0x77);
        let mut fake = chain(1, GENESIS, 10, 5);
        for block in &mut fake {
            // A real, verifying commit QC for a block that is not this chain.
            block.qc = ExtraQc::Present(qc(9, real_hash, VERIFIED));
        }
        let mut calls = 0;
        let report = client.on_reply(peer, 1, reply(&fake), now, &mut verifier(&mut calls));
        assert_eq!(report.fault, None);
        assert_eq!(
            calls, 0,
            "a QC naming nothing held here is not even verified"
        );
        assert!(client.take_ready(0, GENESIS, None, 64).is_empty());
        assert_eq!(client.stats().released, 0);
    }

    #[test]
    fn invalid_qc_is_a_fault_and_enforce_drops_the_reply() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        client.poll(now, 0, 1, None);
        let blocks = with_parent_qcs(chain(1, GENESIS, 5, 1), INVALID);
        let mut calls = 0;
        let report = client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
        assert_eq!(report.fault, Some("bad qc"));
        assert_eq!(client.buffered_blocks(), 0);
    }

    #[test]
    fn prepare_domain_and_unknown_epoch_qcs_do_not_anchor_and_are_not_faults() {
        for marker in [PREPARE, UNKNOWN] {
            let peer = PeerId::random();
            let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
            client.poll(now, 0, 1, None);
            let blocks = with_parent_qcs(chain(1, GENESIS, 5, 1), marker);
            let mut calls = 0;
            let report = client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
            assert_eq!(report.fault, None);
            assert!(client.take_ready(0, GENESIS, None, 64).is_empty());
            if marker == UNKNOWN {
                assert_eq!(client.stats().unknown_epoch_qcs, 4);
                assert!(
                    client.qc_cache.is_empty(),
                    "an unknown set may become known later"
                );
            } else {
                assert_eq!(client.stats().prepare_domain_qcs, 4);
            }
        }
    }

    #[test]
    fn observe_executes_loose_bindings_and_counts_what_enforce_would_hold() {
        let blocks = chain(1, GENESIS, 10, 1);
        for mode in [AnchorMode::Observe, AnchorMode::Enforce] {
            let peer = PeerId::random();
            let (mut client, now) = active(mode, &[peer], 40);
            client.poll(now, 0, 1, None);
            // A PrepareQC-style binding of block 10.
            client.note_loose_anchor(blocks[9].hash);
            let mut calls = 0;
            client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
            let ready = client.take_ready(0, GENESIS, None, 64);
            if mode == AnchorMode::Observe {
                assert_eq!(ready.len(), 10);
                assert!(ready.iter().all(|block| block.loose_only));
                assert_eq!(client.stats().released_loose_only, 10);
            } else {
                assert!(ready.is_empty(), "enforce needs a commit-domain anchor");
            }
        }
    }

    #[test]
    fn qc_less_stretch_is_held_within_the_tail_bound_then_anchored_and_refetched() {
        let peer = PeerId::random();
        let mut config = config();
        config.request_blocks = 100;
        config.max_in_flight = 1;
        let mut client = RangeCatchup::new(config, AnchorMode::Enforce);
        client.note_peer_status(peer, 101);
        let now = Instant::now();
        assert!(client.activate(0, 101, now));
        let (requests, _) = client.poll(now, 0, 1, None);
        assert_eq!(
            requests,
            vec![RangeRequest {
                peer,
                start: 1,
                count: 100
            }]
        );

        // An n42-26-led stretch: 100 blocks, no embedded QC.
        let stretch = chain(1, GENESIS, 100, 1);
        let mut calls = 0;
        client.on_reply(peer, 1, reply(&stretch), now, &mut verifier(&mut calls));
        assert_eq!(client.buffered_blocks(), 64, "tail bound: 64 blocks");
        assert_eq!(client.stats().tail_evicted, 36);
        assert!(
            client.take_ready(0, GENESIS, None, 128).is_empty(),
            "nothing anchored yet"
        );

        // The next block (gov5-led) embeds a commit QC for block 100.
        let (requests, _) = client.poll(now, 0, 1, None);
        assert_eq!(
            requests,
            vec![RangeRequest {
                peer,
                start: 101,
                count: 1
            }]
        );
        let mut next = chain(101, stretch[99].hash, 1, 1);
        next[0].qc = ExtraQc::Present(qc(101, stretch[99].hash, VERIFIED));
        client.on_reply(peer, 101, reply(&next), now, &mut verifier(&mut calls));
        assert_eq!(calls, 1, "one verification anchors the whole stretch");

        let ready = client.take_ready(0, GENESIS, None, 128);
        assert_eq!(ready.len(), 64);
        assert!(ready.iter().all(|block| !block.loose_only));
        // The evicted bodies are now authenticated by their links: fetch again.
        let (requests, _) = client.poll(now, 64, 1, None);
        assert_eq!(
            requests,
            vec![RangeRequest {
                peer,
                start: 65,
                count: 36
            }]
        );
        client.on_reply(
            peer,
            65,
            reply(&stretch[64..]),
            now,
            &mut verifier(&mut calls),
        );
        let ready = client.take_ready(64, stretch[63].hash, None, 128);
        assert_eq!(ready.len(), 36);
        assert_eq!(ready.last().unwrap().number, 100);
    }

    #[test]
    fn a_decide_above_the_range_anchors_through_the_bound_block_link() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 20);
        let blocks = chain(1, GENESIS, 21, 1);
        // Block 21 revealed the gap; a Decide (strict) names it.
        client.note_link(blocks[20].hash, blocks[20].parent, 21);
        client.note_strict_anchor(blocks[20].hash);
        client.poll(now, 0, 1, None);
        let mut calls = 0;
        client.on_reply(
            peer,
            1,
            reply(&blocks[..16]),
            now,
            &mut verifier(&mut calls),
        );
        assert!(client.take_ready(0, GENESIS, None, 64).is_empty());
        let (requests, _) = client.poll(now, 0, 1, None);
        assert_eq!(requests[0].start, 17);
        client.on_reply(
            peer,
            17,
            reply(&blocks[16..20]),
            now,
            &mut verifier(&mut calls),
        );
        // Handover: the range importer stops below the reverse-walk suffix.
        let ready = client.take_ready(0, GENESIS, Some(20), 64);
        assert_eq!(ready.len(), 20);
        assert_eq!(ready.last().unwrap().number, 20);
        assert_eq!(calls, 0);
    }

    #[test]
    fn parallel_ranges_from_two_peers_are_reordered_before_release() {
        let a = PeerId::random();
        let b = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[a, b], 32);
        let blocks = with_parent_qcs(chain(1, GENESIS, 33, 1), VERIFIED);
        let (requests, _) = client.poll(now, 0, 1, None);
        assert_eq!(requests.len(), 2, "two peers, two disjoint ranges");
        let low = *requests.iter().min_by_key(|request| request.start).unwrap();
        let high = *requests.iter().max_by_key(|request| request.start).unwrap();
        assert_eq!((low.start, high.start), (1, 17));
        assert_ne!(low.peer, high.peer);
        let _ = request_for(&requests, a);

        let mut calls = 0;
        client.on_reply(
            high.peer,
            17,
            reply(&blocks[16..32]),
            now,
            &mut verifier(&mut calls),
        );
        assert!(
            client.take_ready(0, GENESIS, None, 64).is_empty(),
            "waits for the lower range"
        );
        client.on_reply(
            low.peer,
            1,
            reply(&blocks[..16]),
            now,
            &mut verifier(&mut calls),
        );
        let ready = client.take_ready(0, GENESIS, None, 64);
        // 1..=31 are anchored by the QCs of their children; 32 waits.
        assert_eq!(ready.len(), 31);
        assert!(ready.windows(2).all(|pair| pair[1].parent == pair[0].hash));
    }

    #[test]
    fn caught_up_or_stalled_client_switches_off() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 20);
        for tick in 0..2 {
            let (_, done) = client.poll(now + Duration::from_secs(tick), 19, 1, None);
            assert_eq!(done, None);
        }
        let (_, done) = client.poll(now + Duration::from_secs(2), 19, 1, None);
        assert_eq!(done, Some(Deactivation::CaughtUp));
        assert!(!client.is_active());
        assert_eq!(client.next_deadline(now), None);

        let (mut client, now) = active(AnchorMode::Observe, &[peer], 2000);
        client.poll(now, 0, 1, None);
        let later = now + client.config.stall_timeout;
        let (_, done) = client.poll(later, 0, 1, None);
        assert_eq!(done, Some(Deactivation::Stalled));
        assert!(!client.activate(0, 5000, later), "cool-down after a stall");
    }

    #[test]
    fn import_failures_refetch_then_give_up() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        assert_eq!(client.import_failed(1, now), None);
        assert_eq!(client.import_failed(1, now), None);
        assert_eq!(client.import_failed(1, now), Some(Deactivation::Stalled));
        assert!(!client.is_active());
    }

    #[test]
    fn more_blocks_than_requested_is_a_fault() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 40);
        client.in_flight.insert((peer, 1), (2, now));
        client.peers.get_mut(&peer).unwrap().in_flight = 1;
        let mut calls = 0;
        let report = client.on_reply(
            peer,
            1,
            reply(&chain(1, GENESIS, 3, 1)),
            now,
            &mut verifier(&mut calls),
        );
        assert_eq!(report.fault, Some("more blocks than requested"));
        assert_eq!(report.accepted, 2);
    }
}
