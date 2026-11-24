//! Range catch-up client over gov5 `/rpc/bodies_by_range/1` (N42 sync
//! standard sections 3.3-3.5 and 6.4).
//!
//! A node that has fallen far behind fetches contiguous block ranges from
//! several statused peers instead of walking back one block hash at a time.
//! This module is the pure, synchronous state machine; the orchestrator owns
//! the network requests, the QC verification context and the import.
//!
//! - **Activation** happens only on a real gap: an authenticated block whose
//!   parent is missing more than `activate_gap` (and at most `max_window`)
//!   heights above the execution head. Advertised status heights only size
//!   how far to fetch (the (f+1)-th highest, never one peer's maximum). While
//!   inactive no per-block work is done.
//! - **Planning is top-down.** Today's n42-26 headers carry no QC, so the
//!   only anchors sit at the top of the gap (the bound block that revealed
//!   it, Decides). Ranges are fetched from the top down: every reply links to
//!   the already authenticated chain above it and is authenticated on
//!   arrival, so each body is downloaded once. Ranges below the descent may
//!   be fetched speculatively in parallel; until the descent reaches them
//!   they count against the unanchored hold (`tail_blocks` / `tail_bytes`).
//! - **Holding**: authenticated bodies are held up to `reorder_bytes`; above
//!   it the highest go first and are fetched again when the importer gets
//!   close (they stay authenticated by hash, so the refetch is checked on
//!   arrival). Only a gap larger than the budget is downloaded partly twice.
//! - **Screening** of every reply: numbers contiguous from the requested
//!   start, parent links, a decodable body (tx and rewards roots are checked
//!   by the decoder), a well-formed header `Extra`, and agreement with the
//!   anchored chain at every height it covers.
//! - **Anchoring**: a block is executed only when its hash lies on the parent
//!   chain below an anchor. Strict anchors are commit-domain H2-v4 QCs
//!   embedded in fetched headers (checked against their own epoch's set by the
//!   caller, no fallback; at most one verification per reply), live Decides
//!   and the engine's committed QC. `N42_SYNC_ANCHOR=observe` (default)
//!   additionally accepts the looser consensus bindings the reverse walk has
//!   always used (a PrepareQC or a TC high QC) and counts what `enforce`
//!   would have refused; `enforce` executes strict anchors only; `off`
//!   disables the client.
//!
//!   `enforce` is **not a security boundary yet**: when this client stops,
//!   the reverse hash walk takes over and that path still executes blocks
//!   bound by loose consensus messages.
//! - **Progress** is the head advancing or the authenticated chain growing,
//!   never bytes received. Without either for `stall_timeout` the client
//!   stops. If the authenticated chain does not extend the local head (the
//!   head sits on an abandoned branch) it stops at once and reports
//!   [`Deactivation::Diverged`].
//! - **Peers**: refusals, busy, empty or short replies and transport failures
//!   back off without a score. Provable faults (broken link, wrong start,
//!   undecodable block, malformed `Extra`, a QC that fails verification
//!   against a known set, more blocks than requested, a block that contradicts
//!   a strict anchor) score; three within ten minutes exclude the peer for an
//!   hour, across reconnects.

use alloy_primitives::{B256, keccak256};
use n42_network::PeerId;
use n42_network::h2_wire::{H2QuorumCertificate, decode_h2_qc};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    /// Neither the head nor the authenticated chain moved for
    /// `stall_timeout`, or the importer kept failing.
    Stalled,
    /// The authenticated chain does not extend the local head: the head is
    /// on another branch (or the anchor is). The client cannot repair that;
    /// it stops at once so an operator sees it.
    Diverged,
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
    /// Bytes of buffered bodies. Above it the highest are dropped and fetched
    /// again later; no refetch is started that would overflow it. Replies in
    /// flight come on top (at most `max_in_flight` times the 256 MiB codec
    /// cap of one reply).
    pub reorder_bytes: usize,
    /// Bodies held without any anchor: replies that arrived before the range
    /// above them, plus speculative requests still in flight.
    pub tail_blocks: usize,
    pub tail_bytes: usize,
    /// Parent-link skeleton bound for not yet anchored blocks.
    pub max_links: usize,
    /// Largest gap the client takes on; above it the reverse walk keeps it.
    pub max_window: u64,
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
            tail_blocks: 1024,
            tail_bytes: 512 << 20,
            max_links: 1 << 16,
            max_window: 1 << 20,
            stall_timeout: Duration::from_secs(90),
            stall_cooldown: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Default)]
struct PeerState {
    height: u64,
    connected: bool,
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
        self.connected
            && self.excluded_until.is_none_or(|until| now >= until)
            && self.backoff_until.is_none_or(|until| now >= until)
    }

    /// Worth remembering after a disconnect (a score or an exclusion).
    fn carries_a_score(&self, now: Instant) -> bool {
        self.excluded_until.is_some_and(|until| now < until)
            || self
                .faults
                .back()
                .is_some_and(|at| now.saturating_duration_since(*at) <= FAULT_WINDOW)
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

/// The authenticated hash at one height.
#[derive(Clone, Copy, Debug)]
struct Anchored {
    hash: B256,
    parent: B256,
    strict: bool,
}

#[derive(Clone, Copy, Debug)]
struct InFlight {
    count: u64,
    sent: Instant,
    speculative: bool,
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
    /// Unanchored bodies dropped by the unanchored hold.
    pub tail_evicted: u64,
    /// Authenticated bodies dropped by the byte budget (fetched again).
    pub budget_evicted: u64,
    /// Held bodies dropped because they contradict the anchored chain.
    pub conflicts: u64,
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
    /// The importer's position: the external head, or a block this client
    /// released and the caller reported imported, whichever is higher.
    head_number: u64,
    head_hash: Option<B256>,
    peers: HashMap<PeerId, PeerState>,
    /// Heights still to fetch, as disjoint intervals: start -> end (excl.).
    work: BTreeMap<u64, u64>,
    planned_to: u64,
    in_flight: HashMap<(PeerId, u64), InFlight>,
    buffer: BTreeMap<u64, Buffered>,
    buffer_bytes: usize,
    /// Buffered heights whose body is not on the anchored chain (yet).
    unanchored: BTreeMap<u64, usize>,
    unanchored_bytes: usize,
    links: HashMap<B256, (B256, u64)>,
    link_order: VecDeque<B256>,
    /// Authenticated hash per height above the head.
    anchored: BTreeMap<u64, Anchored>,
    anchored_total: u64,
    /// Heights touched by anchoring since the last conflict sweep.
    touched: Option<(u64, u64)>,
    /// Anchor hashes whose height is not known yet.
    strict: HashBook,
    loose: HashBook,
    qc_cache: HashMap<B256, QcCheck>,
    qc_cache_order: VecDeque<B256>,
    settled: u32,
    import_failures: u32,
    /// Poll at once (after activation or a reply) instead of at the tick.
    kick: bool,
    /// Highest height the caller lets this client import (last poll).
    stop_at: Option<u64>,
    last_progress: Option<Instant>,
    progress_head: u64,
    progress_anchors: u64,
    cooldown_until: Option<Instant>,
    stopped: Option<Deactivation>,
    avg_block_bytes: usize,
    /// Highest height handed to the importer by [`Self::take_ready`].
    released_to: u64,
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
            head_hash: None,
            peers: HashMap::new(),
            work: BTreeMap::new(),
            planned_to: 0,
            in_flight: HashMap::new(),
            buffer: BTreeMap::new(),
            buffer_bytes: 0,
            unanchored: BTreeMap::new(),
            unanchored_bytes: 0,
            links: HashMap::new(),
            link_order: VecDeque::new(),
            anchored: BTreeMap::new(),
            anchored_total: 0,
            touched: None,
            strict: HashBook::default(),
            loose: HashBook::default(),
            qc_cache: HashMap::new(),
            qc_cache_order: VecDeque::new(),
            settled: 0,
            import_failures: 0,
            kick: false,
            stop_at: None,
            last_progress: None,
            progress_head: 0,
            progress_anchors: 0,
            cooldown_until: None,
            stopped: None,
            avg_block_bytes: 0,
            released_to: 0,
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

    /// The importer position: the height and hash the next release builds
    /// on (ahead of the execution head after imports without a forkchoice
    /// update).
    pub fn position(&self) -> (u64, Option<B256>) {
        (self.head_number, self.head_hash)
    }

    /// Why the client stopped inside [`Self::take_ready`], once.
    pub fn take_stop(&mut self) -> Option<Deactivation> {
        self.stopped.take()
    }

    /// A peer completed a status exchange on this genesis. Its height is a
    /// fetch-sizing hint only.
    pub fn note_peer_status(&mut self, peer: PeerId, height: u64) {
        let state = self.peers.entry(peer).or_default();
        state.height = height;
        state.connected = true;
    }

    /// Requeues the peer's requests. Its score and exclusion survive the
    /// disconnect (a reconnect does not wash a fault away).
    pub fn peer_disconnected(&mut self, peer: &PeerId) {
        let lost: Vec<(PeerId, u64)> = self
            .in_flight
            .keys()
            .filter(|(owner, _)| owner == peer)
            .copied()
            .collect();
        for key in lost {
            if let Some(flight) = self.in_flight.remove(&key) {
                self.add_work(key.1, key.1.saturating_add(flight.count));
            }
        }
        let now = Instant::now();
        if let Some(state) = self.peers.get_mut(peer) {
            if state.carries_a_score(now) {
                state.connected = false;
                state.in_flight = 0;
            } else {
                self.peers.remove(peer);
            }
        }
    }

    /// The `k`-th highest advertised height (k = f+1) among connected peers,
    /// if that many reported one. Never authenticates anything.
    pub fn quorum_height(&self, k: usize) -> Option<u64> {
        if k == 0 {
            return None;
        }
        let mut heights: Vec<u64> = self
            .peers
            .values()
            .filter(|peer| peer.connected)
            .map(|peer| peer.height)
            .collect();
        if heights.len() < k {
            return None;
        }
        heights.sort_unstable_by(|a, b| b.cmp(a));
        Some(heights[k - 1])
    }

    /// Start (or extend) catch-up toward `target`. Returns whether the client
    /// is active afterwards. Inactive unless `activate_gap < target - head <=
    /// max_window`.
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
        let gap = target.saturating_sub(head_number);
        if gap <= self.config.activate_gap || gap > self.config.max_window {
            return false;
        }
        self.deactivate();
        self.active = true;
        self.target = target;
        self.head_number = head_number;
        self.head_hash = None;
        self.planned_to = head_number;
        self.last_progress = Some(now);
        self.progress_head = head_number;
        self.progress_anchors = self.anchored_total;
        self.kick = true;
        true
    }

    /// A hash named by a commit-domain QC the node verified (a Decide, the
    /// engine's committed QC, or a checked header QC).
    pub fn note_strict_anchor(&mut self, hash: B256) {
        if hash != B256::ZERO {
            self.mark(hash, true);
        }
    }

    /// A hash bound by a weaker consensus message (PrepareQC, a TC high QC,
    /// or the parent of a bound block). Executable in observe mode only.
    pub fn note_loose_anchor(&mut self, hash: B256) {
        if hash != B256::ZERO {
            self.mark(hash, false);
        }
    }

    /// The parent link of a block known outside this client (the bound block
    /// that revealed the gap), so anchors above it reach the fetched chain.
    pub fn note_link(&mut self, hash: B256, parent: B256, number: u64) {
        self.record_link(hash, parent, number);
        if self.strict.contains(&hash) {
            self.mark(hash, true);
        }
        if self.loose.contains(&hash) {
            self.mark(hash, false);
        }
    }

    /// The caller executed a released block (with or without moving its
    /// execution head: a repeated header view imports without a forkchoice
    /// update). The next release builds on it.
    pub fn imported(&mut self, number: u64, hash: B256) {
        if self.active && number > self.head_number {
            self.advance_head(number, Some(hash));
        }
    }

    /// Earliest instant [`Self::poll`] should run again; `None` while idle.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.active {
            return None;
        }
        let mut deadline = now + POLL_INTERVAL;
        for flight in self.in_flight.values() {
            deadline = deadline.min(flight.sent + self.config.request_timeout);
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
                .is_some_and(|block| self.executable(next, &block.hash))
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
        self.advance_head(head_number, None);
        if let Some(height) = self.quorum_height(status_quorum) {
            self.target = self.target.max(height);
        }

        let expired: Vec<(PeerId, u64)> = self
            .in_flight
            .iter()
            .filter(|(_, flight)| {
                now.saturating_duration_since(flight.sent) >= self.config.request_timeout
            })
            .map(|(key, _)| *key)
            .collect();
        for key in expired {
            if let Some(flight) = self.in_flight.remove(&key) {
                if let Some(state) = self.peers.get_mut(&key.0) {
                    state.in_flight = state.in_flight.saturating_sub(1);
                }
                self.soft_fail(&key.0, now, false);
                self.add_work(key.1, key.1.saturating_add(flight.count));
            }
        }

        // Progress is the head moving or the authenticated chain growing.
        if self.head_number > self.progress_head || self.anchored_total > self.progress_anchors {
            self.progress_head = self.head_number;
            self.progress_anchors = self.anchored_total;
            self.last_progress = Some(now);
        }

        // The client fetches up to below the lowest staged block, whose hash is
        // its top anchor. Nothing staged: the live path owns the chain.
        let goal = stop_at.map_or(self.head_number, |stop| stop.min(self.target.max(stop)));
        if self.head_number >= goal.saturating_sub(2) && !self.has_ready() {
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

        if goal > self.planned_to {
            self.add_work(self.planned_to.saturating_add(1), goal.saturating_add(1));
            self.planned_to = goal;
        }
        (self.assign(now, goal), None)
    }

    /// Moves the importer position up to `number` (never down) and drops
    /// everything at or below it.
    fn advance_head(&mut self, number: u64, hash: Option<B256>) {
        if number < self.head_number {
            return;
        }
        if number == self.head_number {
            if hash.is_some() {
                self.head_hash = hash;
            }
            return;
        }
        self.head_number = number;
        self.head_hash = hash;
        self.import_failures = 0;
        let keep = self.buffer.split_off(&number.saturating_add(1));
        for block in self.buffer.values() {
            self.buffer_bytes = self.buffer_bytes.saturating_sub(block.rlp.len());
        }
        self.buffer = keep;
        let keep = self.unanchored.split_off(&number.saturating_add(1));
        for bytes in self.unanchored.values() {
            self.unanchored_bytes = self.unanchored_bytes.saturating_sub(*bytes);
        }
        self.unanchored = keep;
        self.anchored = self.anchored.split_off(&number);
        let stale: Vec<u64> = self
            .work
            .range(..=number)
            .map(|(start, _)| *start)
            .collect();
        for start in stale {
            if let Some(end) = self.work.remove(&start)
                && end > number + 1
            {
                self.work.insert(number + 1, end);
            }
        }
    }

    /// Adds `[start, end)` to the work intervals, merged with any interval
    /// it touches. Nothing is ever dropped or capped here.
    fn add_work(&mut self, start: u64, end: u64) {
        let mut start = start.max(self.head_number.saturating_add(1));
        let mut end = end;
        if start >= end {
            return;
        }
        if let Some((&before, &before_end)) = self.work.range(..=start).next_back()
            && before_end >= start
        {
            start = before;
            end = end.max(before_end);
            self.work.remove(&before);
        }
        while let Some((&next, &next_end)) = self.work.range(start..).next() {
            if next > end {
                break;
            }
            end = end.max(next_end);
            self.work.remove(&next);
        }
        self.work.insert(start, end);
    }

    /// Removes `[start, end)`, which lies inside one work interval.
    fn take_work(&mut self, start: u64, end: u64) {
        let Some((&from, &to)) = self.work.range(..=start).next_back() else {
            return;
        };
        if to <= start {
            return;
        }
        self.work.remove(&from);
        if from < start {
            self.work.insert(from, start);
        }
        if end < to {
            self.work.insert(end, to);
        }
    }

    /// First height above the head that is neither buffered nor executed.
    fn first_missing(&self) -> u64 {
        let mut number = self.head_number.saturating_add(1);
        while self.buffer.contains_key(&number) {
            number += 1;
        }
        number
    }

    /// The hash the anchored chain requires at `number`, and whether a strict
    /// anchor requires it.
    fn expected_at(&self, number: u64) -> Option<(B256, bool)> {
        self.anchored
            .get(&number)
            .map(|anchor| (anchor.hash, anchor.strict))
            .or_else(|| {
                self.anchored
                    .get(&number.saturating_add(1))
                    .map(|child| (child.parent, child.strict))
            })
    }

    fn executable(&self, number: u64, hash: &B256) -> bool {
        self.anchored.get(&number).is_some_and(|anchor| {
            anchor.hash == *hash && (anchor.strict || self.mode == AnchorMode::Observe)
        })
    }

    fn executable_height(&self, number: u64) -> bool {
        self.anchored
            .get(&number)
            .is_some_and(|anchor| anchor.strict || self.mode == AnchorMode::Observe)
    }

    /// Blocks that still fit the byte budget, counting replies in flight at
    /// the observed average body size (unbounded before the first reply).
    fn room_blocks(&self) -> u64 {
        let in_flight: u64 = self.in_flight.values().map(|flight| flight.count).sum();
        let committed = self
            .buffer_bytes
            .saturating_add((in_flight as usize).saturating_mul(self.avg_block_bytes));
        self.config
            .reorder_bytes
            .saturating_sub(committed)
            .checked_div(self.avg_block_bytes)
            .map_or(u64::MAX, |blocks| blocks as u64)
    }

    /// The next range to request, by priority:
    /// 1. the block the importer waits for, when its hash is known;
    /// 2. the descent: the highest range right below the anchored chain;
    /// 3. refetching dropped authenticated bodies, lowest first, in budget;
    /// 4. a speculative range below the descent, within the unanchored hold.
    fn choose(&self, goal: u64) -> Option<(u64, u64, bool)> {
        let limit = goal.saturating_add(1);
        let blocks = self.config.request_blocks.max(1);
        let first_missing = self.first_missing();
        let room = self.room_blocks();
        // The importer's next block is fetched even over budget, but never
        // more than fits: a refetch that overflows would only be dropped.
        let importer_blocked = first_missing == self.head_number.saturating_add(1);
        if first_missing < limit
            && self.expected_at(first_missing).is_some()
            && (importer_blocked || room > 0)
            && let Some((_, &end)) = self.work.range(..=first_missing).next_back()
            && end > first_missing
        {
            let count = blocks.min(room).max(1);
            let end = end.min(limit).min(first_missing.saturating_add(count));
            return Some((first_missing, end, false));
        }
        for (&start, &end) in self.work.range(..limit).rev() {
            let end = end.min(limit);
            if end <= start {
                continue;
            }
            let top = end - 1;
            if !self.anchored.contains_key(&top) && self.anchored.contains_key(&end) {
                return Some((start.max(end.saturating_sub(blocks)), end, false));
            }
        }
        if room > 0 {
            for (&start, &end) in self.work.range(..limit) {
                if self.executable_height(start) {
                    let end = end.min(limit).min(start.saturating_add(blocks.min(room)));
                    return Some((start, end, false));
                }
            }
        }
        let speculative: u64 = self
            .in_flight
            .values()
            .filter(|flight| flight.speculative)
            .map(|flight| flight.count)
            .sum();
        for (&start, &end) in self.work.range(..limit).rev() {
            let end = end.min(limit);
            if end <= start || self.anchored.contains_key(&(end - 1)) {
                continue;
            }
            let from = start.max(end.saturating_sub(blocks));
            let held = self.unanchored.len() as u64 + speculative + (end - from);
            if held <= self.config.tail_blocks as u64
                && self.unanchored_bytes <= self.config.tail_bytes
            {
                return Some((from, end, true));
            }
            break;
        }
        None
    }

    fn assign(&mut self, now: Instant, goal: u64) -> Vec<RangeRequest> {
        let mut requests = Vec::new();
        while self.in_flight.len() < self.config.max_in_flight {
            let Some((start, end, speculative)) = self.choose(goal) else {
                break;
            };
            let count = end - start;
            let Some(peer) = self.pick_peer(start, count, now) else {
                break;
            };
            self.take_work(start, end);
            self.in_flight.insert(
                (peer, start),
                InFlight {
                    count,
                    sent: now,
                    speculative,
                },
            );
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
        let end = start.saturating_add(count.saturating_sub(1));
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
    /// QC strictly (commit domain, the set of the QC's own epoch); it runs at
    /// most once per reply.
    pub fn on_reply(
        &mut self,
        peer: PeerId,
        start: u64,
        reply: RangeReply,
        now: Instant,
        verify: &mut dyn FnMut(&H2QuorumCertificate) -> QcCheck,
    ) -> ReplyReport {
        let mut report = ReplyReport::default();
        let Some(flight) = self.in_flight.remove(&(peer, start)) else {
            return report;
        };
        let (count, sent) = (flight.count, flight.sent);
        if let Some(state) = self.peers.get_mut(&peer) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
        if !self.active {
            return report;
        }
        self.kick = true;
        let end = start.saturating_add(count);
        let blocks = match reply {
            RangeReply::Refused => {
                self.soft_fail(&peer, now, false);
                self.add_work(start, end);
                return report;
            }
            RangeReply::Failed { unsupported } => {
                self.soft_fail(&peer, now, unsupported);
                self.add_work(start, end);
                return report;
            }
            RangeReply::Blocks(blocks) if blocks.is_empty() => {
                // A clean empty reply: the peer does not hold the range.
                self.soft_fail(&peer, now, false);
                self.add_work(start, end);
                return report;
            }
            RangeReply::Blocks(blocks) => blocks,
        };

        let enforce = self.mode == AnchorMode::Enforce;
        let mut fault = None;
        let mut kept: Vec<RangeBlock> = Vec::with_capacity(blocks.len().min(count as usize));
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

        // A block contradicting the anchored chain voids the reply: every
        // block below it links up to it. Only a strict anchor makes it a
        // fault (a loose binding may name an abandoned branch); otherwise the
        // peer backs off.
        let mut off_chain = false;
        if let Some(index) = kept.iter().position(|block| {
            self.expected_at(block.number)
                .is_some_and(|(hash, _)| hash != block.hash)
        }) {
            let strict = self
                .expected_at(kept[index].number)
                .is_some_and(|(_, strict)| strict);
            kept.clear();
            self.stats.conflicts += 1;
            if strict {
                fault.get_or_insert("off the anchored chain");
            } else {
                off_chain = true;
            }
        }

        // Links first, so an anchor naming any block of this reply
        // authenticates everything below it.
        for block in &kept {
            self.record_link(block.hash, block.parent, block.number);
        }
        for strict in [true, false] {
            let book = if strict { &self.strict } else { &self.loose };
            if let Some(top) = kept.iter().rev().find(|block| book.contains(&block.hash)) {
                let hash = top.hash;
                self.mark(hash, strict);
            }
        }
        // One embedded QC per reply: the highest one that would anchor
        // something not anchored strictly yet.
        if fault.is_none() || !enforce {
            let candidate = kept.iter().rev().find_map(|block| match &block.qc {
                ExtraQc::Present(qc) => {
                    let (_, number) = *self.links.get(&qc.block_hash)?;
                    let useful = number > self.head_number
                        && !self
                            .anchored
                            .get(&number)
                            .is_some_and(|anchor| anchor.strict && anchor.hash == qc.block_hash);
                    useful.then(|| qc.clone())
                }
                _ => None,
            });
            if let Some(qc) = candidate {
                match self.check_qc(&qc, verify) {
                    QcCheck::Verified => {
                        self.mark(qc.block_hash, true);
                    }
                    QcCheck::PrepareDomain => self.stats.prepare_domain_qcs += 1,
                    QcCheck::UnknownEpoch => self.stats.unknown_epoch_qcs += 1,
                    QcCheck::Invalid => {
                        fault.get_or_insert("bad qc");
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
                self.add_work(start, end);
                self.sweep_conflicts(now);
                return report;
            }
        } else if off_chain {
            self.soft_fail(&peer, now, false);
        }

        let accepted = kept.len();
        report.accepted = accepted;
        let delivered_bytes: usize = kept.iter().map(|block| block.rlp.len()).sum();
        if let Some(average) = delivered_bytes.checked_div(accepted) {
            self.avg_block_bytes = if self.avg_block_bytes == 0 {
                average
            } else {
                (self.avg_block_bytes * 7 + average * 3) / 10
            };
        }
        for block in kept {
            self.buffer_block(peer, block);
        }
        if (accepted as u64) < count {
            // A short reply is not a fault: ask again from the next number.
            self.add_work(start.saturating_add(accepted as u64), end);
        }
        if accepted > 0
            && !off_chain
            && report.fault.is_none()
            && let Some(state) = self.peers.get_mut(&peer)
        {
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
        self.sweep_conflicts(now);
        self.bound_buffer();
        report
    }

    /// Buffers one screened block. At a taken height the body on the anchored
    /// chain wins; between two unanchored bodies the first stays, and the
    /// conflict sweep drops it if an anchor later names the other.
    fn buffer_block(&mut self, source: PeerId, block: RangeBlock) {
        let number = block.number;
        if number <= self.head_number {
            return;
        }
        let linked = self
            .anchored
            .get(&number)
            .is_some_and(|anchor| anchor.hash == block.hash);
        if let Some(existing) = self.buffer.get(&number) {
            let existing_linked = self
                .anchored
                .get(&number)
                .is_some_and(|anchor| anchor.hash == existing.hash);
            if existing.hash == block.hash || existing_linked || !linked {
                return;
            }
            self.drop_buffered(number);
        }
        self.buffer_bytes = self.buffer_bytes.saturating_add(block.rlp.len());
        if !linked {
            self.unanchored.insert(number, block.rlp.len());
            self.unanchored_bytes = self.unanchored_bytes.saturating_add(block.rlp.len());
        }
        self.buffer.insert(
            number,
            Buffered {
                source,
                hash: block.hash,
                parent: block.parent,
                view: block.view,
                rlp: block.rlp,
            },
        );
    }

    /// Drops the body at `number`, which contradicts the anchored chain, and
    /// the unanchored bodies below it that link up to it (the same branch).
    /// All of them are fetched again.
    fn drop_branch(&mut self, number: u64) {
        let mut next = number;
        let mut parent = match self.drop_buffered(next) {
            Some(dropped) => dropped.parent,
            None => return,
        };
        self.stats.conflicts += 1;
        let mut low = next;
        while next > 0 {
            next -= 1;
            let linked = self.unanchored.contains_key(&next)
                && self
                    .buffer
                    .get(&next)
                    .is_some_and(|block| block.hash == parent);
            if !linked {
                break;
            }
            if let Some(dropped) = self.drop_buffered(next) {
                parent = dropped.parent;
                low = next;
            }
        }
        self.add_work(low, number + 1);
    }

    fn drop_buffered(&mut self, number: u64) -> Option<Buffered> {
        let dropped = self.buffer.remove(&number)?;
        self.buffer_bytes = self.buffer_bytes.saturating_sub(dropped.rlp.len());
        if let Some(bytes) = self.unanchored.remove(&number) {
            self.unanchored_bytes = self.unanchored_bytes.saturating_sub(bytes);
        }
        Some(dropped)
    }

    /// Drops held bodies that contradict anchors set since the last sweep,
    /// refetches their heights and scores their sources on strict anchors.
    fn sweep_conflicts(&mut self, now: Instant) {
        let Some((low, high)) = self.touched.take() else {
            return;
        };
        let conflicts: Vec<(u64, PeerId, bool)> = self
            .buffer
            .range(low.saturating_sub(1)..=high)
            .filter_map(|(number, block)| {
                self.expected_at(*number)
                    .filter(|(hash, _)| *hash != block.hash)
                    .map(|(_, strict)| (*number, block.source, strict))
            })
            .collect();
        let mut scored = HashSet::new();
        for (number, source, strict) in conflicts.into_iter().rev() {
            self.drop_branch(number);
            if strict && scored.insert(source) {
                self.fault(&source, now);
            }
        }
    }

    /// Holds at most `tail_blocks` / `tail_bytes` of unanchored bodies (the
    /// lowest go first: the ones next to the descent are the next to be
    /// authenticated) and at most `reorder_bytes` in all (the highest go
    /// first: they are imported last). Every dropped height is fetched again.
    fn bound_buffer(&mut self) {
        while self.unanchored.len() > self.config.tail_blocks
            || (self.unanchored.len() > 1 && self.unanchored_bytes > self.config.tail_bytes)
        {
            let Some((&number, _)) = self.unanchored.first_key_value() else {
                break;
            };
            self.drop_buffered(number);
            self.add_work(number, number + 1);
            self.stats.tail_evicted += 1;
        }
        while self.buffer_bytes > self.config.reorder_bytes && self.buffer.len() > 1 {
            let Some((&number, _)) = self.buffer.last_key_value() else {
                break;
            };
            self.drop_buffered(number);
            self.add_work(number, number + 1);
            self.stats.budget_evicted += 1;
        }
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

    /// Authenticates `hash` and walks the parent links downward, recording
    /// the anchored hash of every height it passes. Stops at a missing link,
    /// the head, or a height already anchored at least as strongly. A strict
    /// anchor replaces a loose one; otherwise the first anchor at a height
    /// stays.
    fn mark(&mut self, hash: B256, strict: bool) {
        let mut current = hash;
        loop {
            if strict {
                self.strict.insert(current, MAX_ANCHORS);
            } else {
                self.loose.insert(current, MAX_ANCHORS);
            }
            let Some(&(parent, number)) = self.links.get(&current) else {
                return;
            };
            if number <= self.head_number {
                return;
            }
            let fresh = match self.anchored.get(&number) {
                Some(anchor) if anchor.strict || !strict => return,
                Some(_) => false,
                None => true,
            };
            self.anchored.insert(
                number,
                Anchored {
                    hash: current,
                    parent,
                    strict,
                },
            );
            if fresh {
                self.anchored_total += 1;
            }
            if let Some(block) = self.buffer.get(&number)
                && block.hash == current
                && let Some(bytes) = self.unanchored.remove(&number)
            {
                self.unanchored_bytes = self.unanchored_bytes.saturating_sub(bytes);
            }
            self.touched = Some(match self.touched {
                Some((low, high)) => (low.min(number), high.max(number)),
                None => (number, number),
            });
            current = parent;
        }
    }

    fn record_link(&mut self, hash: B256, parent: B256, number: u64) {
        if self.links.insert(hash, (parent, number)).is_none() {
            self.link_order.push_back(hash);
            while self.link_order.len() > self.config.max_links {
                if let Some(oldest) = self.link_order.pop_front() {
                    self.links.remove(&oldest);
                }
            }
        }
    }

    /// Pops the authenticated blocks that extend the importer position, in
    /// order, at most `max` and never above `stop_at`. A held body that
    /// contradicts the anchored chain is dropped and fetched again. If the
    /// authenticated block above the head does not extend it, the client
    /// stops with [`Deactivation::Diverged`] (see [`Self::take_stop`]).
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
        self.advance_head(head_number, Some(head_hash));
        let mut hash = self.head_hash.unwrap_or(head_hash);
        let mut number = self.head_number;
        let mut ready = Vec::new();
        while ready.len() < max {
            let next = number.saturating_add(1);
            if stop_at.is_some_and(|stop| next > stop) {
                break;
            }
            let Some(block) = self.buffer.get(&next) else {
                break;
            };
            if self
                .expected_at(next)
                .is_some_and(|(expected, _)| expected != block.hash)
            {
                self.drop_branch(next);
                break;
            }
            if !self.executable(next, &block.hash) {
                break;
            }
            if block.parent != hash {
                if ready.is_empty() {
                    self.deactivate();
                    self.cooldown_until = Some(Instant::now() + self.config.stall_cooldown);
                    self.stopped = Some(Deactivation::Diverged);
                }
                break;
            }
            let loose_only = !self.anchored.get(&next).is_some_and(|anchor| anchor.strict);
            let Some(block) = self.drop_buffered(next) else {
                break;
            };
            self.stats.released += 1;
            if loose_only {
                self.stats.released_loose_only += 1;
            }
            number = next;
            hash = block.hash;
            self.released_to = self.released_to.max(next);
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

    /// The importer could not execute the block at `number`: it and the
    /// rest of the released batch are fetched again (held bodies above stay).
    /// Three failures without the head moving stop the client (the reverse
    /// walk takes over).
    pub fn import_failed(&mut self, number: u64, now: Instant) -> Option<Deactivation> {
        self.import_failures += 1;
        if self.import_failures >= 3 {
            self.deactivate();
            self.cooldown_until = Some(now + self.config.stall_cooldown);
            return Some(Deactivation::Stalled);
        }
        self.add_work(number, self.released_to.max(number).saturating_add(1));
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
        self.unanchored.clear();
        self.unanchored_bytes = 0;
        self.settled = 0;
        self.import_failures = 0;
        self.touched = None;
        self.released_to = 0;
    }

    fn deactivate(&mut self) {
        self.active = false;
        self.reset_round();
        self.links.clear();
        self.link_order.clear();
        self.anchored.clear();
        self.strict.clear();
        self.loose.clear();
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

    /// The block that revealed the gap: linked and anchored.
    fn bind(client: &mut RangeCatchup, block: &RangeBlock, strict: bool) {
        client.note_link(block.hash, block.parent, block.number);
        if strict {
            client.note_strict_anchor(block.hash);
        } else {
            client.note_loose_anchor(block.hash);
        }
    }

    fn expect(client: &mut RangeCatchup, peer: PeerId, start: u64, count: u64, now: Instant) {
        client.in_flight.insert(
            (peer, start),
            InFlight {
                count,
                sent: now,
                speculative: false,
            },
        );
        client.peers.get_mut(&peer).unwrap().in_flight += 1;
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
        assert!(
            !client.activate(0, (1 << 20) + 1, now),
            "a gap beyond the window stays with the reverse walk"
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
        client.poll(now, 0, 2, Some(19));
        assert_eq!(client.target(), 50);
    }

    #[test]
    fn descent_is_top_down_and_short_replies_keep_the_prefix() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 41);
        let blocks = chain(1, GENESIS, 41, 1);
        bind(&mut client, &blocks[40], true);
        let (requests, _) = client.poll(now, 0, 1, Some(40));
        assert_eq!(
            requests,
            vec![RangeRequest {
                peer,
                start: 25,
                count: 16
            }],
            "the range right below the anchor first"
        );
        let mut calls = 0;
        let report = client.on_reply(
            peer,
            25,
            reply(&blocks[24..28]),
            now,
            &mut verifier(&mut calls),
        );
        assert_eq!(report.accepted, 4);
        assert_eq!(report.fault, None);
        let (requests, _) = client.poll(now, 0, 1, Some(40));
        assert_eq!(
            (requests[0].start, requests[0].count),
            (29, 12),
            "the remainder is asked for again"
        );

        for reply in [
            RangeReply::Refused,
            RangeReply::Blocks(Vec::new()),
            RangeReply::Failed { unsupported: false },
        ] {
            let report = client.on_reply(peer, 29, reply, now, &mut verifier(&mut calls));
            assert_eq!(report.fault, None);
            assert_eq!(client.work.get(&29), Some(&41), "asked for again");
            client.take_work(29, 41);
            expect(&mut client, peer, 29, 12, now);
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
            expect(&mut client, peer, 1, 16, now);
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
        let top = chain(1, GENESIS, 400, 1);
        bind(&mut client, &top[399], true);
        client.note_peer_status(honest, 0);
        let mut calls = 0;
        for round in 0..3u64 {
            let at = now + Duration::from_secs(round);
            expect(&mut client, bad, 1, 16, at);
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
        let (requests, _) = client.poll(now + Duration::from_secs(10), 0, 1, Some(399));
        assert!(requests.iter().all(|request| request.peer != bad));
        assert!(requests.iter().any(|request| request.peer == honest));
    }

    #[test]
    fn a_score_survives_a_reconnect() {
        let bad = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[bad], 400);
        let mut calls = 0;
        for round in 0..3u64 {
            let at = now + Duration::from_secs(round);
            if round == 2 {
                client.peer_disconnected(&bad);
                assert_eq!(client.quorum_height(1), None, "gone from the status set");
                client.note_peer_status(bad, 400);
            }
            expect(&mut client, bad, 1, 16, at);
            let report = client.on_reply(
                bad,
                1,
                reply(&chain(2, GENESIS, 3, 1)),
                at,
                &mut verifier(&mut calls),
            );
            assert_eq!(report.peer_excluded, round == 2);
        }
    }

    #[test]
    fn forged_chain_with_a_copied_qc_is_never_released() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        expect(&mut client, peer, 1, 16, now);
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
        expect(&mut client, peer, 1, 16, now);
        let blocks = with_parent_qcs(chain(1, GENESIS, 5, 1), INVALID);
        let mut calls = 0;
        let report = client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
        assert_eq!(report.fault, Some("bad qc"));
        assert_eq!(client.buffered_blocks(), 0);
        assert_eq!(calls, 1);
    }

    #[test]
    fn one_qc_verification_per_reply_in_every_mode() {
        for mode in [AnchorMode::Observe, AnchorMode::Enforce] {
            let peer = PeerId::random();
            let (mut client, now) = active(mode, &[peer], 600);
            expect(&mut client, peer, 1, 256, now);
            let blocks = with_parent_qcs(chain(1, GENESIS, 256, 1), INVALID);
            let mut calls = 0;
            let report = client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
            assert_eq!(report.fault, Some("bad qc"));
            assert_eq!(calls, 1, "{mode:?}: stops at the first invalid QC");
        }
        // A verified one anchors everything below it: still one check.
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 600);
        expect(&mut client, peer, 1, 256, now);
        let blocks = with_parent_qcs(chain(1, GENESIS, 256, 1), VERIFIED);
        let mut calls = 0;
        client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
        assert_eq!(calls, 1);
        assert_eq!(client.take_ready(0, GENESIS, None, 1024).len(), 255);
    }

    #[test]
    fn prepare_domain_and_unknown_epoch_qcs_do_not_anchor_and_are_not_faults() {
        for marker in [PREPARE, UNKNOWN] {
            let peer = PeerId::random();
            let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
            expect(&mut client, peer, 1, 16, now);
            let blocks = with_parent_qcs(chain(1, GENESIS, 5, 1), marker);
            let mut calls = 0;
            let report = client.on_reply(peer, 1, reply(&blocks), now, &mut verifier(&mut calls));
            assert_eq!(report.fault, None);
            assert!(client.take_ready(0, GENESIS, None, 64).is_empty());
            if marker == UNKNOWN {
                assert_eq!(client.stats().unknown_epoch_qcs, 1);
                assert!(
                    client.qc_cache.is_empty(),
                    "an unknown set may become known later"
                );
            } else {
                assert_eq!(client.stats().prepare_domain_qcs, 1);
            }
        }
    }

    #[test]
    fn observe_executes_loose_bindings_and_counts_what_enforce_would_hold() {
        let blocks = chain(1, GENESIS, 10, 1);
        for mode in [AnchorMode::Observe, AnchorMode::Enforce] {
            let peer = PeerId::random();
            let (mut client, now) = active(mode, &[peer], 40);
            expect(&mut client, peer, 1, 16, now);
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
    fn enforce_holds_a_loose_linked_range_until_a_commit_anchor() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 101);
        let blocks = chain(1, GENESIS, 101, 1);
        bind(&mut client, &blocks[100], false);
        let mut calls = 0;
        let mut at = now;
        // A QC-less stretch: fetched top-down, linked, not executable yet.
        while client.buffered_blocks() < 100 {
            let (requests, _) = client.poll(at, 0, 1, Some(100));
            for request in requests {
                let from = (request.start - 1) as usize;
                let to = from + request.count as usize;
                client.on_reply(
                    request.peer,
                    request.start,
                    reply(&blocks[from..to]),
                    at,
                    &mut verifier(&mut calls),
                );
            }
            at += Duration::from_secs(1);
        }
        assert!(client.take_ready(0, GENESIS, Some(100), 128).is_empty());
        assert_eq!(client.stats().blocks_received, 100, "each body once");
        // A Decide for the staged block.
        client.note_strict_anchor(blocks[100].hash);
        let ready = client.take_ready(0, GENESIS, Some(100), 128);
        assert_eq!(ready.len(), 100);
        assert!(ready.iter().all(|block| !block.loose_only));
    }

    #[test]
    fn a_decide_above_the_range_anchors_through_the_bound_block_link() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 21);
        let blocks = chain(1, GENESIS, 21, 1);
        // Block 21 revealed the gap; a Decide (strict) names it.
        bind(&mut client, &blocks[20], true);
        let (requests, _) = client.poll(now, 0, 1, Some(20));
        assert_eq!((requests[0].start, requests[0].count), (5, 16));
        let mut calls = 0;
        client.on_reply(
            peer,
            5,
            reply(&blocks[4..20]),
            now,
            &mut verifier(&mut calls),
        );
        assert!(client.take_ready(0, GENESIS, Some(20), 64).is_empty());
        let (requests, _) = client.poll(now, 0, 1, Some(20));
        assert_eq!((requests[0].start, requests[0].count), (1, 4));
        client.on_reply(peer, 1, reply(&blocks[..4]), now, &mut verifier(&mut calls));
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
        let (mut client, now) = active(AnchorMode::Enforce, &[a, b], 33);
        let blocks = chain(1, GENESIS, 33, 1);
        bind(&mut client, &blocks[32], true);
        let (requests, _) = client.poll(now, 0, 1, Some(32));
        assert_eq!(requests.len(), 2, "two peers, two disjoint ranges");
        let low = *requests.iter().min_by_key(|request| request.start).unwrap();
        let high = *requests.iter().max_by_key(|request| request.start).unwrap();
        assert_eq!((low.start, high.start), (1, 17));
        assert_ne!(low.peer, high.peer);

        let mut calls = 0;
        // The lower range arrives first: held without an anchor.
        client.on_reply(
            low.peer,
            1,
            reply(&blocks[..16]),
            now,
            &mut verifier(&mut calls),
        );
        assert_eq!(client.unanchored.len(), 16);
        assert!(client.take_ready(0, GENESIS, Some(32), 64).is_empty());
        client.on_reply(
            high.peer,
            17,
            reply(&blocks[16..32]),
            now,
            &mut verifier(&mut calls),
        );
        assert!(client.unanchored.is_empty());
        let ready = client.take_ready(0, GENESIS, Some(32), 64);
        assert_eq!(ready.len(), 32);
        assert!(ready.windows(2).all(|pair| pair[1].parent == pair[0].hash));
    }

    #[test]
    fn caught_up_or_stalled_client_switches_off() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 20);
        for tick in 0..2 {
            let (_, done) = client.poll(now + Duration::from_secs(tick), 19, 1, Some(20));
            assert_eq!(done, None);
        }
        let (_, done) = client.poll(now + Duration::from_secs(2), 19, 1, Some(20));
        assert_eq!(done, Some(Deactivation::CaughtUp));
        assert!(!client.is_active());
        assert_eq!(client.next_deadline(now), None);

        let (mut client, now) = active(AnchorMode::Observe, &[peer], 2000);
        client.poll(now, 0, 1, Some(1999));
        let later = now + client.config.stall_timeout;
        let (_, done) = client.poll(later, 0, 1, Some(1999));
        assert_eq!(done, Some(Deactivation::Stalled));
        assert!(!client.activate(0, 5000, later), "cool-down after a stall");
    }

    #[test]
    fn received_bodies_that_never_import_are_not_progress() {
        // Blocks keep arriving but neither the head nor the anchored chain
        // moves: the client stalls instead of fetching forever.
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 2000);
        let mut calls = 0;
        let mut at = now;
        let mut done = None;
        for _ in 0..200 {
            let (requests, stopped) = client.poll(at, 0, 1, Some(1999));
            if stopped.is_some() {
                done = stopped;
                break;
            }
            for request in requests {
                let blocks = chain(request.start, B256::repeat_byte(1), request.count, 7);
                client.on_reply(
                    request.peer,
                    request.start,
                    reply(&blocks),
                    at,
                    &mut verifier(&mut calls),
                );
            }
            at += Duration::from_secs(1);
        }
        assert_eq!(done, Some(Deactivation::Stalled));
    }

    #[test]
    fn a_head_off_the_anchored_chain_stops_the_client_as_diverged() {
        let peer = PeerId::random();
        let mut client = RangeCatchup::new(config(), AnchorMode::Observe);
        client.note_peer_status(peer, 101);
        let now = Instant::now();
        assert!(client.activate(10, 101, now));
        let blocks = chain(1, GENESIS, 101, 1);
        bind(&mut client, &blocks[100], true);
        expect(&mut client, peer, 11, 90, now);
        let mut calls = 0;
        client.on_reply(
            peer,
            11,
            reply(&blocks[10..100]),
            now,
            &mut verifier(&mut calls),
        );
        let abandoned = B256::repeat_byte(0x99);
        assert!(client.take_ready(10, abandoned, Some(100), 64).is_empty());
        assert_eq!(client.take_stop(), Some(Deactivation::Diverged));
        assert!(!client.is_active());
    }

    #[test]
    fn a_repeated_view_import_moves_the_release_position() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 40);
        let blocks = chain(1, GENESIS, 40, 1);
        bind(&mut client, &blocks[39], true);
        expect(&mut client, peer, 1, 39, now);
        let mut calls = 0;
        client.on_reply(
            peer,
            1,
            reply(&blocks[..39]),
            now,
            &mut verifier(&mut calls),
        );
        let first = client.take_ready(0, GENESIS, Some(39), 2);
        assert_eq!(first.len(), 2);
        // Block 2 imported without a forkchoice update: the head stays at 1.
        client.imported(1, first[0].hash);
        client.imported(2, first[1].hash);
        let next = client.take_ready(1, first[0].hash, Some(39), 64);
        assert_eq!(next.first().map(|block| block.number), Some(3));
        assert_eq!(client.position(), (2, Some(first[1].hash)));
    }

    #[test]
    fn import_failures_refetch_then_give_up() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Enforce, &[peer], 40);
        assert_eq!(client.import_failed(1, now), None);
        assert!(client.work.contains_key(&1), "fetched again");
        assert_eq!(client.import_failed(1, now), None);
        assert_eq!(client.import_failed(1, now), Some(Deactivation::Stalled));
        assert!(!client.is_active());
    }

    #[test]
    fn more_blocks_than_requested_is_a_fault() {
        let peer = PeerId::random();
        let (mut client, now) = active(AnchorMode::Observe, &[peer], 40);
        expect(&mut client, peer, 1, 2, now);
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

    #[test]
    fn work_intervals_merge_split_and_never_drop_heights() {
        let (mut client, _) = active(AnchorMode::Observe, &[PeerId::random()], 40_000);
        for height in (1..=5000u64).rev() {
            client.add_work(height, height + 1);
        }
        assert_eq!(client.work.len(), 1);
        assert_eq!(client.work.get(&1), Some(&5001), "no cap on a merged range");
        client.take_work(100, 200);
        assert_eq!(client.work.get(&1), Some(&100));
        assert_eq!(client.work.get(&200), Some(&5001));
        client.add_work(150, 250);
        assert_eq!(client.work.get(&150), Some(&5001));
    }

    /// Simulations against honest and forging peers; written against the
    /// public interface only, so they also run on the first version of the
    /// client (where review items 1-4 make them fail).
    mod regression {
        use super::super::*;

        const ROOT: B256 = B256::repeat_byte(0xab);

        fn segment(first: u64, parent: B256, len: u64, salt: u8) -> Vec<RangeBlock> {
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

        struct Net {
            /// Block `n` at index `n - 1`; QC-less headers (n42-26 today).
            chain: Vec<RangeBlock>,
            served: u64,
            forger: Option<PeerId>,
        }

        impl Net {
            fn new(len: u64) -> Self {
                Self {
                    chain: segment(1, ROOT, len, 1),
                    served: 0,
                    forger: None,
                }
            }

            fn serve(&mut self, request: RangeRequest) -> RangeReply {
                let from = (request.start - 1) as usize;
                let to = (from + request.count as usize).min(self.chain.len());
                if from >= to {
                    return RangeReply::Blocks(Vec::new());
                }
                self.served += (to - from) as u64;
                let blocks = if Some(request.peer) == self.forger {
                    let parent = if from == 0 {
                        ROOT
                    } else {
                        self.chain[from - 1].hash
                    };
                    segment(request.start, parent, (to - from) as u64, 0xf0)
                } else {
                    self.chain[from..to].to_vec()
                };
                RangeReply::Blocks(blocks.into_iter().map(Ok).collect())
            }
        }

        /// Activates the client for a gap closed by the last block of the
        /// chain (the staged block that revealed it).
        fn start(
            net: &Net,
            config: RangeCatchupConfig,
            mode: AnchorMode,
            peers: &[PeerId],
            head: u64,
            strict: bool,
        ) -> (RangeCatchup, Instant) {
            let mut client = RangeCatchup::new(config, mode);
            let top = net.chain.last().unwrap().clone();
            for peer in peers {
                client.note_peer_status(*peer, top.number);
            }
            let now = Instant::now();
            assert!(client.activate(head, top.number, now));
            client.note_link(top.hash, top.parent, top.number);
            if strict {
                client.note_strict_anchor(top.hash);
            } else {
                client.note_loose_anchor(top.hash);
            }
            (client, now)
        }

        /// One poll, the replies in request order, one import per second.
        fn run(
            client: &mut RangeCatchup,
            net: &mut Net,
            head: &mut (u64, B256),
            seconds: u64,
            now: Instant,
        ) {
            let stop_at = net.chain.len() as u64 - 1;
            for tick in 0..seconds {
                let at = now + Duration::from_secs(tick);
                let (requests, _) = client.poll(at, head.0, 1, Some(stop_at));
                if !client.is_active() {
                    return;
                }
                for request in requests {
                    let reply = net.serve(request);
                    client.on_reply(request.peer, request.start, reply, at, &mut |_| {
                        QcCheck::UnknownEpoch
                    });
                }
                for block in client.take_ready(head.0, head.1, Some(stop_at), usize::MAX) {
                    assert_eq!(block.parent, head.1, "released in chain order");
                    *head = (block.number, block.hash);
                }
                if !client.is_active() {
                    return;
                }
            }
        }

        fn peers(count: usize) -> Vec<PeerId> {
            let mut peers: Vec<PeerId> = (0..count).map(|_| PeerId::random()).collect();
            peers.sort();
            peers
        }

        /// Review item 1: a 5000-block QC-less gap with four honest peers
        /// used to stop at 1088 (merged requeues capped at 1024 lost
        /// heights) after downloading 6024 bodies.
        #[test]
        fn qc_less_gap_of_5000_catches_up_with_four_honest_peers() {
            let mut net = Net::new(5001);
            let (mut client, now) = start(
                &net,
                RangeCatchupConfig::default(),
                AnchorMode::Enforce,
                &peers(4),
                0,
                true,
            );
            let mut head = (0, ROOT);
            run(&mut client, &mut net, &mut head, 600, now);
            assert_eq!(head.0, 5000, "reaches the staged block");
            assert_eq!(net.served, 5000, "every body downloaded once");
        }

        /// Review item 1 with a byte budget far below the gap: evicted
        /// heights are fetched again, none is lost, nothing loops.
        #[test]
        fn a_small_budget_refetches_without_losing_heights() {
            let mut net = Net::new(1001);
            let config = RangeCatchupConfig {
                reorder_bytes: 100 * 100,
                ..RangeCatchupConfig::default()
            };
            let (mut client, now) = start(&net, config, AnchorMode::Observe, &peers(4), 0, false);
            let mut head = (0, ROOT);
            run(&mut client, &mut net, &mut head, 600, now);
            assert_eq!(head.0, 1000);
            assert!(net.served <= 2000, "served {}", net.served);
        }

        /// Review item 2: QC-less headers anchor only at the top; a
        /// bottom-up plan held 64 bodies and downloaded the rest twice, in
        /// observe as well.
        #[test]
        fn a_qc_less_gap_downloads_each_body_once_in_observe() {
            let mut net = Net::new(501);
            let (mut client, now) = start(
                &net,
                RangeCatchupConfig::default(),
                AnchorMode::Observe,
                &peers(4),
                0,
                false,
            );
            let mut head = (0, ROOT);
            run(&mut client, &mut net, &mut head, 600, now);
            assert_eq!(head.0, 500);
            assert_eq!(net.served, 500);
        }

        /// Review item 2: nothing above head + max_links used to import.
        #[test]
        fn a_gap_beyond_the_link_window_still_imports() {
            let mut net = Net::new(3001);
            let config = RangeCatchupConfig {
                max_links: 1000,
                ..RangeCatchupConfig::default()
            };
            let (mut client, now) = start(&net, config, AnchorMode::Observe, &peers(4), 0, false);
            let mut head = (0, ROOT);
            run(&mut client, &mut net, &mut head, 600, now);
            assert_eq!(head.0, 3000);
        }

        /// Review item 3: a head on an abandoned branch used to drop and
        /// re-request head + 1 forever (received bytes counted as progress).
        #[test]
        fn a_head_on_an_abandoned_branch_stops_the_client() {
            let mut net = Net::new(101);
            let (mut client, now) = start(
                &net,
                RangeCatchupConfig::default(),
                AnchorMode::Observe,
                &peers(2),
                10,
                false,
            );
            let mut head = (10, B256::repeat_byte(0x99));
            run(&mut client, &mut net, &mut head, 300, now);
            assert!(!client.is_active(), "stops instead of refetching forever");
            assert_eq!(head.0, 10);
        }

        /// Review item 4: one forged head + 1 reply from a same-genesis peer
        /// used to hold the head until the client stalled.
        #[test]
        fn a_forging_peer_cannot_wedge_the_importer() {
            let mut net = Net::new(1001);
            let all = peers(5);
            net.forger = Some(all[0]);
            let config = RangeCatchupConfig {
                request_blocks: 64,
                ..RangeCatchupConfig::default()
            };
            let (mut client, now) = start(&net, config, AnchorMode::Observe, &all, 0, true);
            let mut head = (0, ROOT);
            run(&mut client, &mut net, &mut head, 60, now);
            assert_eq!(head.0, 1000, "honest peers finish the gap");
            assert!(client.stats().faults >= 1, "the forger is scored");
        }
    }
}
