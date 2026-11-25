//! Static header validity on a Profile-1 chain (mixed production standard
//! B1-B4, B8-B14, A23, AM4, AM24), checked against the parent header.
//!
//! [`validate_profile1_header`] is the import-time check: it depends only on
//! the header, its parent, the genesis validator set and leader schedule, and
//! an ancestry oracle for the embedded QC, never on the clock. The vote-time
//! clock bound of B4 is [`check_profile1_vote_time`]. The body checks (B6
//! rewards against `withdrawalsRoot`, transactions root) and the deferred
//! execution fields (B13 values, A10) belong to execution and are not here.

use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Address, B256, U256};
use n42_chainspec::profile1::{
    Profile1ExtraError, Profile1Header, Profile1SealError,
};
use n42_consensus::{LeaderSchedule, ValidatorSet, leader_index};
use n42_network::h2_v4::H2V4ChainIdentity;
use n42_network::h2_wire::decode_h2_qc;

use crate::h2_finality::{H2V4AnchorVerdict, verify_h2_v4_sync_anchor};

/// What a Profile-1 header is checked against.
#[derive(Clone, Copy, Debug)]
pub struct Profile1HeaderRules<'a> {
    /// `(chainId, genesisHash)`: the H2-v4 signing identity (A1).
    pub identity: H2V4ChainIdentity,
    /// The genesis validator set (A3, AM5).
    pub validators: &'a ValidatorSet,
    /// Tenure and rotate-on-timeout (A15, A22).
    pub schedule: LeaderSchedule,
    /// `config.hotstuff.gasLimit` (A23).
    pub gas_limit: u64,
    /// The slot grid (A17, AM27): the genesis timestamp G and the slot
    /// length I (`config.hotstuff.blockIntervalMs`).
    pub grid: Profile1SlotGrid,
}

/// How late after its slot start a scheduled proposal still takes that slot
/// (timer jitter); later, the leader waits for the next boundary (A17: the
/// first slot boundary that is not in the past).
pub const PROFILE1_SLOT_START_GRACE_MS: u64 = 1000;

/// The Profile-1 slot grid (A17, AM27): slot `s` starts at `G + s·I`, where
/// `G` is the genesis timestamp (unix seconds) and `I` is
/// `config.hotstuff.blockIntervalMs`. Kept in milliseconds so a slot length
/// that is not a whole number of seconds is still exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile1SlotGrid {
    /// The genesis timestamp G, unix seconds.
    pub genesis_time: u64,
    /// The slot length I, milliseconds; positive.
    pub interval_ms: u64,
}

impl Profile1SlotGrid {
    /// The grid of the chain `config` describes, whose genesis header is
    /// stamped `genesis_time`.
    pub const fn new(genesis_time: u64, interval_ms: u64) -> Self {
        Self { genesis_time, interval_ms }
    }

    /// `(time − G)·1000` in milliseconds, or `None` before genesis.
    fn offset_ms(&self, time: u64) -> Option<u128> {
        time.checked_sub(self.genesis_time)
            .map(|offset| u128::from(offset) * 1000)
    }

    /// Whether `time` (unix seconds) is a slot start: `(time − G) mod I = 0`.
    pub fn is_on_grid(&self, time: u64) -> bool {
        self.interval_ms > 0
            && self
                .offset_ms(time)
                .is_some_and(|offset| offset.is_multiple_of(u128::from(self.interval_ms)))
    }

    /// The slot that starts at `time`, when `time` is on the grid.
    pub fn slot_of(&self, time: u64) -> Option<u64> {
        if !self.is_on_grid(time) {
            return None;
        }
        u64::try_from(self.offset_ms(time)? / u128::from(self.interval_ms)).ok()
    }

    /// The start of slot `slot`, `G + slot·I`, in unix milliseconds.
    pub fn slot_start_ms(&self, slot: u64) -> u128 {
        u128::from(self.genesis_time) * 1000 + u128::from(slot) * u128::from(self.interval_ms)
    }

    /// The start of slot `slot` in unix seconds, when it is a whole second.
    pub fn slot_start(&self, slot: u64) -> Option<u64> {
        let ms = self.slot_start_ms(slot);
        ms.is_multiple_of(1000).then(|| u64::try_from(ms / 1000).ok()).flatten()
    }

    /// A17: the slot a leader holding a parent stamped `parent_time`
    /// proposes in at `now_ms` (unix milliseconds): the first slot after the
    /// parent's that is not in the past,
    /// `max(sP + 1, ceil((now − G)/I))`. A parent off the grid (genesis is
    /// slot 0 by definition) counts as the slot it falls in.
    pub fn proposal_slot(&self, parent_time: u64, now_ms: u128) -> u64 {
        let interval = u128::from(self.interval_ms.max(1));
        let parent_slot = self.offset_ms(parent_time).map_or(0, |offset| offset / interval);
        let genesis_ms = u128::from(self.genesis_time) * 1000;
        let now_slot = now_ms.saturating_sub(genesis_ms).div_ceil(interval);
        let slot = (parent_slot + 1).max(now_slot);
        u64::try_from(slot).unwrap_or(u64::MAX)
    }

    /// The first slot at or after `slot` whose start is a whole second (a
    /// header time is in seconds). With I a whole number of seconds that is
    /// `slot` itself.
    fn stampable(&self, mut slot: u64) -> u64 {
        for _ in 0..1000 {
            if self.slot_start(slot).is_some() {
                break;
            }
            slot = slot.saturating_add(1);
        }
        slot
    }

    /// A17 (AM27): the slot a leader holding a parent stamped `parent_time`
    /// proposes in, and its start. `scheduled` is a stamp chosen earlier
    /// (when the build was scheduled for a slot boundary); it is kept when it
    /// is on the grid, after the parent's slot, and `now_ms` is at most
    /// [`PROFILE1_SLOT_START_GRACE_MS`] past its start, so a build timer
    /// firing a few milliseconds after the boundary keeps the slot it was
    /// armed for instead of skipping to the next. Otherwise the slot is
    /// [`Self::proposal_slot`]. Returns `(stamp, start_ms)`: the leader
    /// publishes no earlier than `start_ms` and stamps `stamp = start_ms/1000`.
    pub fn plan_proposal(
        &self,
        parent_time: u64,
        scheduled: Option<u64>,
        now_ms: u128,
    ) -> (u64, u128) {
        let parent_slot = self.offset_ms(parent_time).map_or(0, |offset| {
            u64::try_from(offset / u128::from(self.interval_ms.max(1))).unwrap_or(u64::MAX)
        });
        let kept = scheduled.and_then(|time| self.slot_of(time)).filter(|&slot| {
            slot > parent_slot
                && now_ms
                    <= self.slot_start_ms(slot)
                        + u128::from(PROFILE1_SLOT_START_GRACE_MS.min(self.interval_ms))
        });
        let slot = self.stampable(kept.unwrap_or_else(|| self.proposal_slot(parent_time, now_ms)));
        let start_ms = self.slot_start_ms(slot);
        let stamp = u64::try_from(start_ms / 1000).unwrap_or(u64::MAX);
        (stamp, start_ms)
    }
}

/// Why a Profile-1 header is invalid. The rule id is the standard's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1HeaderInvalid {
    #[error("profile 1 (B1): invalid header extraData: {0}")]
    Extra(Profile1ExtraError),
    #[error("profile 1 (B1): invalid header QC: {0}")]
    Qc(String),
    #[error("profile 1 (B2): invalid seal: {0}")]
    Seal(Profile1SealError),
    #[error("profile 1 (B3): coinbase {coinbase} is not the leader of view {view}, {leader}")]
    Proposer {
        coinbase: Address,
        view: u64,
        leader: Address,
    },
    #[error("profile 1 (B3): difficulty is not zero: {0}")]
    Difficulty(U256),
    #[error("profile 1 (B3): nonce is not zero")]
    Nonce,
    #[error("profile 1 (B3): mixHash is not zero: {0}")]
    MixHash(B256),
    #[error("profile 1 (B3): ommersHash is not zero: {0}")]
    OmmersHash(B256),
    #[error("profile 1 (B4): timestamp before parent: {time} < parent {parent}")]
    TimeBeforeParent { time: u64, parent: u64 },
    #[error("profile 1 (B4, AM27): timestamp {time} is not on the slot grid: (time - genesis {genesis}) mod {interval_ms} ms != 0")]
    TimeOffGrid {
        time: u64,
        genesis: u64,
        interval_ms: u64,
    },
    #[error("profile 1 (B4, AM27): timestamp {time} is not in a later slot than the parent's {parent}: want >= parent + {interval_ms} ms")]
    TimeSameSlot {
        time: u64,
        parent: u64,
        interval_ms: u64,
    },
    #[error("profile 1 (B4): timestamp too far ahead of the local clock: {time} > floor(now) + 2 = {bound}")]
    TimeFuture { time: u64, bound: u64 },
    #[error("profile 1 (B8): requestsHash is missing")]
    RequestsHash(Option<B256>),
    #[error("profile 1 (B9): blob gas fields are not zero: blobGasUsed {used:?}, excessBlobGas {excess:?}")]
    BlobGas {
        used: Option<u64>,
        excess: Option<u64>,
    },
    #[error("profile 1 (B10): slot number {slot:?} is not the block number {number}")]
    SlotNumber { slot: Option<u64>, number: u64 },
    #[error("profile 1 (B13): gasUsed {used} exceeds the parent gasLimit {limit}")]
    GasUsed { used: u64, limit: u64 },
    #[error("profile 1 (B14): baseFee {got:?} is not the EIP-1559 value of the parent, want {want}")]
    BaseFee { got: Option<u64>, want: u64 },
    #[error("profile 1 (A23): gasLimit is not hotstuff.gasLimit: {got}, want {want}")]
    GasLimit { got: u64, want: u64 },
    #[error("profile 1: header does not extend its parent: {0}")]
    Parent(String),
}

/// What a valid header says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile1HeaderView {
    pub view: u64,
    /// The embedded CommitQC: `(view, block)`, when present.
    pub qc: Option<(u64, B256)>,
    /// The validator that proposed and sealed the header.
    pub leader_index: u32,
}

/// The view a Profile-1 header carries in its extra data; genesis (empty
/// extra) is view 0.
pub fn profile1_header_view(header: &Profile1Header) -> Result<u64, Profile1HeaderInvalid> {
    if header.header.number == 0 && header.header.extra_data.is_empty() {
        return Ok(0);
    }
    header
        .extra()
        .map(|extra| extra.view)
        .map_err(Profile1HeaderInvalid::Extra)
}

/// The import-time checks of a Profile-1 header against its parent.
/// `is_ancestor(hash)` answers whether `hash` is the parent or one of its
/// ancestors (B1: the embedded QC names an ancestor of the block).
pub fn validate_profile1_header(
    header: &Profile1Header,
    parent: &Profile1Header,
    rules: &Profile1HeaderRules<'_>,
    is_ancestor: impl Fn(B256) -> bool,
) -> Result<Profile1HeaderView, Profile1HeaderInvalid> {
    let h = &header.header;
    let p = &parent.header;
    let parent_hash = parent.hash();
    if h.parent_hash != parent_hash || Some(h.number) != p.number.checked_add(1) {
        return Err(Profile1HeaderInvalid::Parent(format!(
            "block {} names parent {}, given parent is block {} {}",
            h.number, h.parent_hash, p.number, parent_hash
        )));
    }

    // B1: one extra layout.
    let extra = header.extra().map_err(Profile1HeaderInvalid::Extra)?;
    let parent_view = profile1_header_view(parent)?;
    if extra.view <= parent_view {
        return Err(Profile1HeaderInvalid::Parent(format!(
            "view {} is not above the parent's view {parent_view}",
            extra.view
        )));
    }

    // B3: constant fields and the proposer.
    if h.difficulty != U256::ZERO {
        return Err(Profile1HeaderInvalid::Difficulty(h.difficulty));
    }
    if !h.nonce.is_zero() {
        return Err(Profile1HeaderInvalid::Nonce);
    }
    if h.mix_hash != B256::ZERO {
        return Err(Profile1HeaderInvalid::MixHash(h.mix_hash));
    }
    if h.ommers_hash != B256::ZERO {
        return Err(Profile1HeaderInvalid::OmmersHash(h.ommers_hash));
    }
    let n = u64::from(rules.validators.len());
    let leader = leader_index(extra.view, n, rules.schedule.tenure);
    let leader_address = *rules
        .validators
        .get_address(leader)
        .map_err(|error| Profile1HeaderInvalid::Qc(error.to_string()))?;
    if h.beneficiary != leader_address {
        return Err(Profile1HeaderInvalid::Proposer {
            coinbase: h.beneficiary,
            view: extra.view,
            leader: leader_address,
        });
    }

    // B4 (import half, AM27): not before the parent, on the slot grid, and
    // in a later slot than the parent's (`time >= parent.time + I`).
    check_profile1_import_time(h.timestamp, p.timestamp, rules.grid)?;
    // B8: present. Its value is a deferred field (AM25: N−1's EIP-7685
    // requests hash), compared with this node's execution of the parent on
    // import (the QMDB state-root job), not here.
    if h.requests_hash.is_none() {
        return Err(Profile1HeaderInvalid::RequestsHash(h.requests_hash));
    }
    // B9.
    if h.blob_gas_used != Some(0) || h.excess_blob_gas != Some(0) {
        return Err(Profile1HeaderInvalid::BlobGas {
            used: h.blob_gas_used,
            excess: h.excess_blob_gas,
        });
    }
    // B10.
    if h.slot_number != Some(h.number) {
        return Err(Profile1HeaderInvalid::SlotNumber {
            slot: h.slot_number,
            number: h.number,
        });
    }
    // A23 and B13's bound.
    if h.gas_limit != rules.gas_limit {
        return Err(Profile1HeaderInvalid::GasLimit {
            got: h.gas_limit,
            want: rules.gas_limit,
        });
    }
    if h.gas_used > p.gas_limit {
        return Err(Profile1HeaderInvalid::GasUsed {
            used: h.gas_used,
            limit: p.gas_limit,
        });
    }
    // B14: from the parent header as written.
    let want = p
        .next_block_base_fee(BaseFeeParams::ethereum())
        .ok_or_else(|| Profile1HeaderInvalid::Parent("parent has no base fee".to_owned()))?;
    if h.base_fee_per_gas != Some(want) {
        return Err(Profile1HeaderInvalid::BaseFee {
            got: h.base_fee_per_gas,
            want,
        });
    }

    // B1: the embedded CommitQC, zero bytes when absent.
    let qc = if extra.qc.is_empty() {
        None
    } else {
        let qc = decode_h2_qc(extra.qc)
            .map_err(|error| Profile1HeaderInvalid::Qc(format!("QC does not decode: {error}")))?;
        if qc.view == 0 || qc.block_hash == B256::ZERO {
            return Err(Profile1HeaderInvalid::Qc(
                "an absent QC is zero bytes, not an encoded empty QC".to_owned(),
            ));
        }
        if qc.view >= extra.view {
            return Err(Profile1HeaderInvalid::Qc(format!(
                "QC view {} is not below header view {}",
                qc.view, extra.view
            )));
        }
        match verify_h2_v4_sync_anchor(&qc, rules.identity, rules.validators) {
            H2V4AnchorVerdict::Verified => {}
            H2V4AnchorVerdict::PrepareDomain => {
                return Err(Profile1HeaderInvalid::Qc(format!(
                    "QC for view {} is signed in the vote domain, not the commit domain",
                    qc.view
                )));
            }
            H2V4AnchorVerdict::Invalid => {
                return Err(Profile1HeaderInvalid::Qc(format!(
                    "QC for view {} is not a commit-domain quorum of the genesis set",
                    qc.view
                )));
            }
        }
        if !is_ancestor(qc.block_hash) {
            return Err(Profile1HeaderInvalid::Qc(format!(
                "QC block {} is not an ancestor",
                qc.block_hash
            )));
        }
        Some((qc.view, qc.block_hash))
    };

    // B2 (AM4): the leader's seal over sealHash, verified on import.
    let key = rules
        .validators
        .get_public_key(leader)
        .map_err(|error| Profile1HeaderInvalid::Qc(error.to_string()))?;
    header
        .verify_seal(key)
        .map_err(Profile1HeaderInvalid::Seal)?;

    Ok(Profile1HeaderView {
        view: extra.view,
        qc,
        leader_index: leader,
    })
}

/// B4, import half (AM27): `time >= parent.time`, `(time − G) mod I = 0` and
/// `time >= parent.time + I`. Clock-free; checked on every import path.
pub fn check_profile1_import_time(
    time: u64,
    parent_time: u64,
    grid: Profile1SlotGrid,
) -> Result<(), Profile1HeaderInvalid> {
    if time < parent_time {
        return Err(Profile1HeaderInvalid::TimeBeforeParent {
            time,
            parent: parent_time,
        });
    }
    if !grid.is_on_grid(time) {
        return Err(Profile1HeaderInvalid::TimeOffGrid {
            time,
            genesis: grid.genesis_time,
            interval_ms: grid.interval_ms,
        });
    }
    if u128::from(time) * 1000 < u128::from(parent_time) * 1000 + u128::from(grid.interval_ms) {
        return Err(Profile1HeaderInvalid::TimeSameSlot {
            time,
            parent: parent_time,
            interval_ms: grid.interval_ms,
        });
    }
    Ok(())
}

/// B4, vote half: `time <= floor(now) + 2`, checked before the Round-1 vote
/// only (never on import of a committed block).
pub fn check_profile1_vote_time(
    header: &Profile1Header,
    now_unix: u64,
) -> Result<(), Profile1HeaderInvalid> {
    let bound = now_unix.saturating_add(2);
    if header.header.timestamp > bound {
        return Err(Profile1HeaderInvalid::TimeFuture {
            time: header.header.timestamp,
            bound,
        });
    }
    Ok(())
}

impl Profile1HeaderInvalid {
    /// The standard's rule id, for logs and metric labels.
    pub const fn rule(&self) -> &'static str {
        match self {
            Self::Extra(_) | Self::Qc(_) => "B1",
            Self::Seal(_) => "B2",
            Self::Proposer { .. }
            | Self::Difficulty(_)
            | Self::Nonce
            | Self::MixHash(_)
            | Self::OmmersHash(_) => "B3",
            Self::TimeBeforeParent { .. }
            | Self::TimeOffGrid { .. }
            | Self::TimeSameSlot { .. }
            | Self::TimeFuture { .. } => "B4",
            Self::RequestsHash(_) => "B8",
            Self::BlobGas { .. } => "B9",
            Self::SlotNumber { .. } => "B10",
            Self::GasUsed { .. } => "B13",
            Self::BaseFee { .. } => "B14",
            Self::GasLimit { .. } => "A23",
            Self::Parent(_) => "parent",
        }
    }
}

/// Why the import gate did not admit a block.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1Refusal {
    /// The header breaks a rule: permanent for this hash.
    #[error(transparent)]
    Invalid(#[from] Profile1HeaderInvalid),
    /// The header is not in the 24-element layout (B10-B12): permanent.
    #[error("profile 1 (B12): header layout: {0}")]
    Layout(String),
    /// No Profile-1 header is known for the hash: retryable.
    #[error("profile 1: no header known for block {0}")]
    HeaderUnknown(B256),
    /// The parent has not been admitted yet: retryable.
    #[error("profile 1: parent {parent} of block {hash} has not been admitted")]
    ParentUnknown { hash: B256, parent: B256 },
}

impl Profile1Refusal {
    /// The rule id, for logs and metric labels.
    pub const fn rule(&self) -> &'static str {
        match self {
            Self::Invalid(invalid) => invalid.rule(),
            Self::Layout(_) => "B12",
            Self::HeaderUnknown(_) => "header_unknown",
            Self::ParentUnknown { .. } => "parent_unknown",
        }
    }

    /// Whether the refusal holds for the hash whatever arrives later.
    pub const fn is_permanent(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::Layout(_))
    }
}

/// Logs and counts a refusal with its rule id. `path` names the import path.
pub fn log_profile1_refusal(hash: B256, path: &'static str, refusal: &Profile1Refusal) {
    let rule = refusal.rule();
    metrics::counter!("n42_profile1_header_refused_total", "path" => path, "rule" => rule)
        .increment(1);
    tracing::warn!(
        target: "n42::profile1",
        %hash,
        path,
        rule,
        permanent = refusal.is_permanent(),
        %refusal,
        "profile 1: header refused; block not imported, not voted on"
    );
}

/// Admitted headers kept for parent lookups and the QC ancestry walk.
const PROFILE1_GATE_CAPACITY: usize = 4096;
/// How far back the B1 ancestry walk looks for the QC's block.
const PROFILE1_ANCESTRY_DEPTH: usize = 256;

/// The import gate of a Profile-1 node: every block header passes
/// [`validate_profile1_header`] against its admitted parent before its
/// payload reaches execution, and the Round-1 vote passes
/// [`check_profile1_vote_time`].
#[derive(Debug)]
pub struct Profile1ImportGate {
    identity: H2V4ChainIdentity,
    validators: ValidatorSet,
    schedule: LeaderSchedule,
    gas_limit: u64,
    grid: Profile1SlotGrid,
    admitted: std::collections::HashMap<B256, Profile1Header>,
    order: std::collections::VecDeque<B256>,
    refused: std::collections::HashMap<B256, Profile1Refusal>,
    clock: fn() -> u64,
    /// This validator's BLS key, with which its blocks are sealed (B2);
    /// `None` on a node that does not produce.
    seal_key: Option<std::sync::Arc<n42_primitives::BlsSecretKey>>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Profile1ImportGate {
    /// The gate of the chain `config` describes, seeded with its genesis
    /// header. The validator set is the genesis set (AM5: no staged change).
    pub fn new(
        config: &n42_chainspec::profile1::Profile1Config,
        genesis: Profile1Header,
    ) -> Result<Self, String> {
        let validators = ValidatorSet::try_new(&config.validator_infos(), config.fault_tolerance())
            .map_err(|error| error.to_string())?;
        let schedule = LeaderSchedule::resolve_profile1(config, None, None)
            .map_err(|error| error.to_string())?
            .schedule;
        let mut gate = Self {
            identity: H2V4ChainIdentity {
                chain_id: config.chain_id,
                genesis_hash: genesis.hash(),
            },
            validators,
            schedule,
            gas_limit: config.gas_limit,
            grid: Profile1SlotGrid::new(genesis.header.timestamp, config.block_interval_ms),
            admitted: Default::default(),
            order: Default::default(),
            refused: Default::default(),
            clock: unix_now,
            seal_key: None,
        };
        gate.seed(genesis);
        Ok(gate)
    }

    /// The key this node seals its own blocks with (B2: the leader of the
    /// view signs `sealHash`).
    pub fn with_seal_key(mut self, key: std::sync::Arc<n42_primitives::BlsSecretKey>) -> Self {
        self.seal_key = Some(key);
        self
    }

    /// The key this node seals its own blocks with, when it produces.
    pub fn seal_key(&self) -> Option<std::sync::Arc<n42_primitives::BlsSecretKey>> {
        self.seal_key.clone()
    }

    /// Replaces the vote clock (unix seconds); for tests.
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self
    }

    /// The slot grid the gate checks timestamps against (A17, AM27).
    pub const fn grid(&self) -> Profile1SlotGrid {
        self.grid
    }

    /// The H2-v4 identity the gate checks QCs in.
    pub const fn identity(&self) -> H2V4ChainIdentity {
        self.identity
    }

    /// Trusts a header without checking it: genesis, or the durable head
    /// this node executed before a restart.
    pub fn seed(&mut self, header: Profile1Header) {
        self.remember(header.hash(), header);
    }

    fn remember(&mut self, hash: B256, header: Profile1Header) {
        if self.admitted.insert(hash, header).is_none() {
            self.order.push_back(hash);
        }
        while self.order.len() > PROFILE1_GATE_CAPACITY {
            if let Some(evicted) = self.order.pop_front() {
                self.admitted.remove(&evicted);
            }
        }
    }

    /// An admitted (or seeded) header.
    pub fn header(&self, hash: &B256) -> Option<&Profile1Header> {
        self.admitted.get(hash)
    }

    /// Checks `header` against its admitted parent and admits it.
    /// Idempotent for an admitted header; a permanent refusal is remembered.
    pub fn admit(&mut self, header: Profile1Header) -> Result<B256, Profile1Refusal> {
        let hash = header.hash();
        if self.admitted.contains_key(&hash) {
            return Ok(hash);
        }
        if let Some(refusal) = self.refused.get(&hash) {
            return Err(refusal.clone());
        }
        let parent_hash = header.header.parent_hash;
        if !self.admitted.contains_key(&parent_hash) {
            self.admit_own_sealed(parent_hash);
        }
        let Some(parent) = self.admitted.get(&parent_hash) else {
            return Err(Profile1Refusal::ParentUnknown {
                hash,
                parent: parent_hash,
            });
        };
        let rules = Profile1HeaderRules {
            identity: self.identity,
            validators: &self.validators,
            schedule: self.schedule,
            gas_limit: self.gas_limit,
            grid: self.grid,
        };
        let admitted = &self.admitted;
        let is_ancestor = |target: B256| {
            let mut cursor = parent_hash;
            for _ in 0..PROFILE1_ANCESTRY_DEPTH {
                if cursor == target {
                    return true;
                }
                match admitted.get(&cursor) {
                    Some(ancestor) if ancestor.header.number > 0 => {
                        cursor = ancestor.header.parent_hash;
                    }
                    _ => return false,
                }
            }
            false
        };
        match validate_profile1_header(&header, parent, &rules, is_ancestor) {
            Ok(_) => {
                self.remember(hash, header);
                Ok(hash)
            }
            Err(invalid) => {
                let refusal = Profile1Refusal::Invalid(invalid);
                self.refuse(hash, refusal.clone());
                Err(refusal)
            }
        }
    }

    /// Decodes the exact header encoding behind `hash` and admits it.
    pub fn admit_raw(&mut self, hash: B256, raw: &[u8]) -> Result<B256, Profile1Refusal> {
        if self.admitted.contains_key(&hash) {
            return Ok(hash);
        }
        if alloy_primitives::keccak256(raw) != hash {
            return Err(Profile1Refusal::HeaderUnknown(hash));
        }
        let header = match Profile1Header::decode(raw) {
            Ok(header) => header,
            Err(error) => {
                let refusal = Profile1Refusal::Layout(error.to_string());
                self.refuse(hash, refusal.clone());
                return Err(refusal);
            }
        };
        self.admit(header)
    }

    /// Admits the header of `hash` from `raw` when given, else from the
    /// encoding the network layer remembered for it, else as already
    /// admitted. A header admitted from `raw` is remembered for the
    /// execution side's payload binding.
    pub fn admit_known(&mut self, hash: B256, raw: Option<&[u8]>) -> Result<B256, Profile1Refusal> {
        match raw {
            Some(raw) => {
                let admitted = self.admit_raw(hash, raw)?;
                n42_consensus::remember_gov5_native_header(raw);
                Ok(admitted)
            }
            None => match n42_consensus::gov5_native_header_rlp(&hash) {
                Some(raw) => self.admit_raw(hash, &raw),
                None => self
                    .header(&hash)
                    .map(|_| hash)
                    .ok_or(Profile1Refusal::HeaderUnknown(hash)),
            },
        }
    }

    /// A block this validator sealed itself never arrives over an import
    /// path, so its children would find no parent. Its header is taken from
    /// the native-header registry only when the seal verifies under this
    /// gate's own key, and is then admitted under the same rules as any other.
    pub fn admit_own_sealed(&mut self, hash: B256) -> bool {
        if self.admitted.contains_key(&hash) {
            return true;
        }
        let Some(key) = self.seal_key.clone() else {
            return false;
        };
        let Some(header) = n42_consensus::remembered_profile1_header(&hash) else {
            return false;
        };
        if header.verify_seal(&key.public_key()).is_err() {
            return false;
        }
        self.admit(header).is_ok()
    }

    /// Admits the header of a block decoded from the gov5 wire with
    /// [`n42_network::decode_profile1_block_rlp`].
    pub fn admit_block(
        &mut self,
        block: &n42_network::Gov5GossipBlock,
    ) -> Result<B256, Profile1Refusal> {
        let hash = block.block_hash;
        let Some(mobile_registry_root) = block.mobile_registry_root else {
            return Err(Profile1Refusal::Layout(format!(
                "block {hash} has no mobileRegistryRoot (element 24)"
            )));
        };
        let header = Profile1Header {
            header: block.header.clone(),
            mobile_registry_root,
        };
        if header.hash() != hash {
            return Err(Profile1Refusal::Layout(format!(
                "block {hash} does not carry a Profile-1 header"
            )));
        }
        self.admit(header)
    }

    fn refuse(&mut self, hash: B256, refusal: Profile1Refusal) {
        if self.refused.len() >= PROFILE1_GATE_CAPACITY {
            self.refused.clear();
        }
        self.refused.insert(hash, refusal);
    }

    /// B4, vote half, for an admitted block at the gate's clock.
    pub fn check_vote(&self, hash: &B256) -> Result<(), Profile1Refusal> {
        let header = self
            .admitted
            .get(hash)
            .ok_or(Profile1Refusal::HeaderUnknown(*hash))?;
        check_profile1_vote_time(header, (self.clock)()).map_err(Profile1Refusal::Invalid)
    }
}

/// AM27 variants of V1's block 2 (parent: block 1 at `G + 8`), resealed by
/// view 2's leader: `(name, header encoding, admitted on import)`.
#[cfg(test)]
pub(crate) fn am27_block2_variants() -> Vec<(&'static str, Vec<u8>, bool)> {
    use alloy_primitives::hex;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../n42-chainspec/testdata/profile1/V1/header_profile_v1.json");
    let vector: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let decode = |value: &serde_json::Value| {
        Profile1Header::decode(&hex::decode(value.as_str().unwrap()).unwrap()).unwrap()
    };
    let block1 = decode(&vector["headers"][0]["rlp"]);
    let block2 = decode(&vector["headers"][1]["rlp"]);
    let signer = vector["headers"][1]["signerIndex"].as_u64().unwrap() as usize;
    let key: [u8; 32] = hex::decode(
        vector["validators"][signer]["blsSecretKey"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
    )
    .unwrap()
    .try_into()
    .unwrap();
    let key = n42_primitives::BlsSecretKey::from_bytes(&key).unwrap();
    let parent = block1.header.timestamp;
    assert_eq!(block2.header.timestamp, parent + 8, "V1 block 2 is parent + 8");
    [
        ("on grid, parent + 8 (V1 block 2)", parent + 8, true),
        ("skip slot, parent + 16", parent + 16, true),
        ("off grid by +1 s, parent + 9", parent + 9, false),
        ("off grid by -1 s, parent + 7", parent + 7, false),
        ("same slot, parent + 0", parent, false),
        ("before the parent, parent - 8", parent - 8, false),
    ]
    .into_iter()
    .map(|(name, time, admitted)| {
        let mut header = block2.clone();
        header.header.timestamp = time;
        header.seal(&key).unwrap();
        (name, header.encode(), admitted)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use n42_chainspec::profile1::profile1_chain_spec;
    use serde_json::Value;
    use std::path::Path;

    fn testdata(file: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../n42-chainspec/testdata/profile1/V1")
            .join(file);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn header(value: &Value) -> Profile1Header {
        Profile1Header::decode(&hex::decode(value.as_str().unwrap()).unwrap()).unwrap()
    }

    struct Fixture {
        vector: Value,
        genesis: Profile1Header,
        block1: Profile1Header,
        block2: Profile1Header,
        validators: ValidatorSet,
        identity: H2V4ChainIdentity,
        gas_limit: u64,
        schedule: LeaderSchedule,
    }

    impl Fixture {
        fn load() -> Self {
            let vector = testdata("header_profile_v1.json");
            let (spec, config, genesis) = profile1_chain_spec(&testdata("genesis_8s.json")).unwrap();
            let validators =
                ValidatorSet::try_new(&config.validator_infos(), config.fault_tolerance()).unwrap();
            let schedule = LeaderSchedule::resolve_profile1(&config, None, None)
                .unwrap()
                .schedule;
            Self {
                block1: header(&vector["headers"][0]["rlp"]),
                block2: header(&vector["headers"][1]["rlp"]),
                vector,
                genesis,
                validators,
                identity: H2V4ChainIdentity {
                    chain_id: config.chain_id,
                    genesis_hash: spec.genesis_hash(),
                },
                gas_limit: config.gas_limit,
                schedule,
            }
        }

        fn rules(&self) -> Profile1HeaderRules<'_> {
            Profile1HeaderRules {
                identity: self.identity,
                validators: &self.validators,
                schedule: self.schedule,
                gas_limit: self.gas_limit,
                grid: Profile1SlotGrid::new(self.genesis.header.timestamp, 8000),
            }
        }

        /// Block 2's ancestors: block 1 and genesis.
        fn is_ancestor(&self) -> impl Fn(B256) -> bool + '_ {
            move |hash| hash == self.block1.hash() || hash == self.genesis.hash()
        }
    }

    #[test]
    fn v1_blocks_are_valid() {
        let f = Fixture::load();
        let rules = f.rules();
        let view1 = validate_profile1_header(&f.block1, &f.genesis, &rules, |hash| {
            hash == f.genesis.hash()
        })
        .unwrap();
        assert_eq!(view1, Profile1HeaderView { view: 1, qc: None, leader_index: 1 });
        let view2 = validate_profile1_header(&f.block2, &f.block1, &rules, f.is_ancestor()).unwrap();
        assert_eq!(view2.view, 2);
        assert_eq!(view2.leader_index, 2);
        assert_eq!(view2.qc, Some((1, f.block1.hash())));
        assert_eq!(f.vector["headers"][1]["qc"]["view"].as_u64(), Some(1));
        // Committed blocks import whatever the clock says; the vote bound
        // holds at the block's own time.
        check_profile1_vote_time(&f.block2, f.block2.header.timestamp).unwrap();
    }

    /// Every V1 refusal: the import ones by [`validate_profile1_header`] (or,
    /// for a layout the decoder cannot read, by [`Profile1Header::decode`]),
    /// the vote one by [`check_profile1_vote_time`]. The execution one (body
    /// rewards against withdrawalsRoot) is not a header check.
    #[test]
    fn every_v1_header_refusal_is_refused() {
        let f = Fixture::load();
        let rules = f.rules();
        let mut checked = 0;
        for refusal in f.vector["refusals"].as_array().unwrap() {
            let name = refusal["name"].as_str().unwrap();
            let raw = hex::decode(refusal["rlp"].as_str().unwrap()).unwrap();
            match refusal["check"].as_str().unwrap() {
                "import" => {
                    let result = Profile1Header::decode(&raw)
                        .map_err(|error| error.to_string())
                        .and_then(|header| {
                            validate_profile1_header(&header, &f.block1, &rules, f.is_ancestor())
                                .map_err(|error| error.to_string())
                        });
                    let error = result.expect_err(name);
                    eprintln!("{name}: {error}  [gov5: {}]", refusal["gov5Message"]);
                    checked += 1;
                }
                "vote" => {
                    let header = Profile1Header::decode(&raw).unwrap();
                    validate_profile1_header(&header, &f.block1, &rules, f.is_ancestor())
                        .expect("a vote-time refusal imports");
                    let now = refusal["nowUnix"].as_u64().unwrap();
                    let error = check_profile1_vote_time(&header, now).expect_err(name);
                    eprintln!("{name}: {error}");
                    checked += 1;
                }
                "execution" => {
                    // The header is block 2 itself and valid on import.
                    let header = Profile1Header::decode(&raw).unwrap();
                    assert_eq!(header, f.block2, "{name}");
                }
                other => panic!("{name}: unknown check {other}"),
            }
        }
        // rc4 V1: 24 import refusals (AM27 adds the equal-time and off-grid
        // cases; "requestsHash absent" is static, B8) plus one vote refusal.
        assert_eq!(checked, 25);
    }

    /// B4 (AM27), import half: on the grid and in a later slot than the
    /// parent's; a skipped slot is allowed.
    #[test]
    fn am27_import_time_checks() {
        let grid = Profile1SlotGrid::new(1_000, 8_000);
        let ok = |time, parent| check_profile1_import_time(time, parent, grid);
        // On the grid, parent + 8.
        ok(1_016, 1_008).unwrap();
        // Skip slot: parent + 16.
        ok(1_024, 1_008).unwrap();
        // Off the grid by one second either way.
        assert!(matches!(ok(1_017, 1_008), Err(Profile1HeaderInvalid::TimeOffGrid { .. })));
        assert!(matches!(ok(1_015, 1_008), Err(Profile1HeaderInvalid::TimeOffGrid { .. })));
        // Same slot as the parent (parent + 0): refused.
        assert!(matches!(ok(1_008, 1_008), Err(Profile1HeaderInvalid::TimeSameSlot { .. })));
        // Before the parent; before genesis is off the grid.
        assert!(matches!(ok(1_000, 1_008), Err(Profile1HeaderInvalid::TimeBeforeParent { .. })));
        assert!(matches!(ok(992, 0), Err(Profile1HeaderInvalid::TimeOffGrid { .. })));
        // Block 1 on genesis: G + 8 is slot 1.
        ok(1_008, 1_000).unwrap();
        assert!(matches!(ok(1_000, 1_000), Err(Profile1HeaderInvalid::TimeSameSlot { .. })));
        // The grid is G's, not the unix epoch's: G = 1003.
        let shifted = Profile1SlotGrid::new(1_003, 8_000);
        check_profile1_import_time(1_011, 1_003, shifted).unwrap();
        assert!(check_profile1_import_time(1_016, 1_003, shifted).is_err());
        // Every refusal is a B4 refusal.
        assert_eq!(ok(1_017, 1_008).unwrap_err().rule(), "B4");
        assert_eq!(ok(1_008, 1_008).unwrap_err().rule(), "B4");
    }

    /// The slot length is the genesis's, not a constant: I = 12 s.
    #[test]
    fn am27_grid_uses_the_genesis_interval() {
        let grid = Profile1SlotGrid::new(1_000, 12_000);
        check_profile1_import_time(1_012, 1_000, grid).unwrap();
        assert!(check_profile1_import_time(1_008, 1_000, grid).is_err());
        assert!(check_profile1_import_time(1_016, 1_000, grid).is_err());
        check_profile1_import_time(1_036, 1_012, grid).unwrap();
        // A slot length that is not whole seconds: I = 1.5 s, slots at
        // 0, 1.5, 3, ...; only the whole-second starts can be stamped.
        let half = Profile1SlotGrid::new(1_000, 1_500);
        check_profile1_import_time(1_003, 1_000, half).unwrap();
        assert!(check_profile1_import_time(1_002, 1_000, half).is_err());
        assert_eq!(half.plan_proposal(1_000, None, 1_000_000), (1_003, 1_003_000));
    }

    /// A17 (AM27): the leader's stamp is the start of the first slot after
    /// the parent's that is not in the past, whatever the clock's phase.
    #[test]
    fn am27_leader_stamps_the_slot_start() {
        let g = 1_795_564_803_u64; // not a multiple of 8: the grid is G's
        let grid = Profile1SlotGrid::new(g, 8_000);
        let ms = |seconds: u64, millis: u64| u128::from(seconds) * 1000 + u128::from(millis);
        // Parent in slot 1, now early in slot 1: propose in slot 2.
        assert_eq!(grid.plan_proposal(g + 8, None, ms(g + 9, 300)), (g + 16, ms(g + 16, 0)));
        // Parent in slot 1, now late in slot 2 (a slow parent): the next
        // boundary, slot 3, not the past start of slot 2.
        assert_eq!(grid.plan_proposal(g + 8, None, ms(g + 17, 1)), (g + 24, ms(g + 24, 0)));
        // Exactly on a boundary: that slot.
        assert_eq!(grid.plan_proposal(g + 8, None, ms(g + 24, 0)), (g + 24, ms(g + 24, 0)));
        // After a timeout (24 s base timeout and a TC), the view is entered
        // mid-slot: the next boundary, still on the grid and later than the
        // parent's slot.
        let (stamp, start) = grid.plan_proposal(g + 8, None, ms(g + 8 + 24, 4_321));
        assert_eq!(stamp, g + 40);
        assert_eq!(start, ms(g + 40, 0));
        check_profile1_import_time(stamp, g + 8, grid).unwrap();
        // A scheduled stamp keeps its slot when the timer fires a few ms
        // late, and is dropped when it is in the parent's slot or long past.
        assert_eq!(grid.plan_proposal(g + 8, Some(g + 16), ms(g + 16, 7)), (g + 16, ms(g + 16, 0)));
        assert_eq!(grid.plan_proposal(g + 8, Some(g + 8), ms(g + 9, 0)), (g + 16, ms(g + 16, 0)));
        assert_eq!(grid.plan_proposal(g + 8, Some(g + 16), ms(g + 19, 0)), (g + 24, ms(g + 24, 0)));
        // An off-grid scheduled stamp (the old epoch-aligned boundary) is
        // replaced by the grid's.
        assert_eq!(grid.plan_proposal(g + 8, Some(g + 13), ms(g + 12, 0)), (g + 16, ms(g + 16, 0)));
        // Genesis is slot 0: block 1 goes in slot 1 or later.
        assert_eq!(grid.plan_proposal(g, None, ms(g, 1)), (g + 8, ms(g + 8, 0)));
        // Every planned stamp, at any clock phase, passes the import check.
        for parent in [g, g + 8, g + 80] {
            for now in (ms(g, 0)..ms(g + 120, 0)).step_by(997) {
                let (stamp, start) = grid.plan_proposal(parent, None, now);
                check_profile1_import_time(stamp, parent, grid).unwrap();
                assert!(start >= now, "never a slot in the past");
                assert!(start < now + 8_000 || stamp == parent + 8, "the first such slot");
            }
        }
    }

    /// The gate refuses AM27's off-grid and same-slot variants of V1 block 2
    /// and admits the on-grid and skip-slot ones.
    #[test]
    fn am27_gate_checks_block2_variants() {
        let f = Fixture::load();
        let (_, config, _) = profile1_chain_spec(&testdata("genesis_8s.json")).unwrap();
        let mut gate = Profile1ImportGate::new(&config, f.genesis.clone()).unwrap();
        assert_eq!(gate.grid(), Profile1SlotGrid::new(f.genesis.header.timestamp, 8000));
        gate.admit(f.block1.clone()).unwrap();
        for (name, raw, admitted) in am27_block2_variants() {
            let hash = alloy_primitives::keccak256(&raw);
            let result = gate.admit_raw(hash, &raw);
            if admitted {
                assert_eq!(result, Ok(hash), "{name}");
            } else {
                let refusal = result.expect_err(name);
                assert_eq!(refusal.rule(), "B4", "{name}: {refusal}");
                assert!(refusal.is_permanent(), "{name}");
            }
        }
    }

    /// A leader's own block reaches its gate through no import path. Its
    /// child still finds the parent: the remembered header is admitted when
    /// its seal verifies under the gate's own key, and only then.
    #[test]
    fn own_sealed_parent_is_admitted_only_under_the_own_key() {
        let f = Fixture::load();
        let (_, config, _) = profile1_chain_spec(&testdata("genesis_8s.json")).unwrap();
        let key = |index: usize| {
            let hex_key = f.vector["validators"][index]["blsSecretKey"].as_str().unwrap();
            let bytes: [u8; 32] = hex::decode(hex_key.trim_start_matches("0x"))
                .unwrap()
                .try_into()
                .unwrap();
            std::sync::Arc::new(n42_primitives::BlsSecretKey::from_bytes(&bytes).unwrap())
        };
        n42_consensus::remember_gov5_native_header(&f.block1.encode());

        let mut no_key = Profile1ImportGate::new(&config, f.genesis.clone()).unwrap();
        assert!(matches!(
            no_key.admit(f.block2.clone()),
            Err(Profile1Refusal::ParentUnknown { .. })
        ));
        // Validator 0 did not seal block 1 (view 1's leader is validator 1).
        let mut other = Profile1ImportGate::new(&config, f.genesis.clone())
            .unwrap()
            .with_seal_key(key(0));
        assert!(matches!(
            other.admit(f.block2.clone()),
            Err(Profile1Refusal::ParentUnknown { .. })
        ));
        let mut sealer = Profile1ImportGate::new(&config, f.genesis.clone())
            .unwrap()
            .with_seal_key(key(1));
        assert_eq!(sealer.admit(f.block2.clone()).unwrap(), f.block2.hash());
        assert!(sealer.header(&f.block1.hash()).is_some());
    }

    /// B6 / A8: block N's withdrawalsRoot is the rewards root of
    /// `devBlockReward` to its coinbase and to the faucet.
    #[test]
    fn v1_withdrawals_roots_are_the_dev_rewards() {
        let f = Fixture::load();
        let faucet: Address = "0x42e9819036f61bF665D5f727E8C03121f12f586e".parse().unwrap();
        let reward = U256::from(1_000_000_000_000_000_000_u64);
        for block in [&f.block1, &f.block2] {
            let rewards = [
                n42_consensus::Gov5Reward { address: block.header.beneficiary, amount: reward },
                n42_consensus::Gov5Reward { address: faucet, amount: reward },
            ];
            assert_eq!(
                Some(n42_consensus::gov5_native_rewards_root(&rewards)),
                block.header.withdrawals_root
            );
        }
    }
}
