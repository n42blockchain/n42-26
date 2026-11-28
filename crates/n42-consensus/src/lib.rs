#![cfg_attr(
    test,
    allow(
        clippy::collapsible_if,
        clippy::needless_range_loop,
        clippy::unnecessary_literal_unwrap
    )
)]

mod adapter;
pub mod committee_pool;
pub mod error;
pub mod extra_data;
pub mod gov5_native_header;
pub mod gov5_rewards;
pub mod protocol;
mod receipt;
pub mod rotor;
pub mod validator;
pub mod vote_log;
pub mod deferred_depth;
pub mod executed_fields;
pub mod vote_rules;

pub use adapter::{
    GOV5_HEADER_EXTRA_MAGIC, MAX_GOV5_HEADER_EXTRA_BYTES, MIN_GOV5_HEADER_EXTRA_BYTES,
    N42Consensus, N42HeaderProfile, N42HeaderProfileError, ValidatorSetResolver,
    validate_gov5_h2_header, validate_gov5_header_extra, validate_gov5_interop_header,
    validate_gov5_replay_v2_header,
};
pub use committee_pool::{
    CommitteeLinkError, CommitteePoolConfig, CommitteePoolConfigError, CommitteePoolError,
    ConsensusEvidence as CommitteeConsensusEvidence, SimulatedCommitteePool, shared_committee_pool,
};
pub use error::ConsensusError as N42ConsensusError;
pub use executed_fields::{
    CarriedMismatch, ExecutedFields, ExecutedFieldsError, RestartError, carried_mismatch, executed_fields, record_carried_mismatch, require_head_result, install_executed_fields_log,
    remember_executed_fields, wait_for_executed_fields,
};
pub use gov5_native_header::{
    Gov5NativeHeader, Gov5NativeHeaderError, gov5_native_header_rlp, remember_gov5_native_header,
    remembered_gov5_native_header, remembered_profile1_header,
};
pub use gov5_rewards::{
    Gov5Reward, Gov5RewardError, gov5_native_rewards_root, gov5_rewards_to_withdrawals,
    gov5_withdrawals_to_rewards,
};
pub use protocol::quorum::{
    validator_changes_hash, verify_commit_qc, verify_commit_qc_with_profile, verify_qc, verify_tc,
};
pub use protocol::state_machine::{AuthenticatedConsensusMessage, FUTURE_VIEW_WINDOW};
pub use protocol::{
    ConsensusEngine, ConsensusEvent, EngineOutput, EquivocationKind, EquivocationProof,
    SignedMessage, ViewTiming,
};
pub use receipt::{gov5_native_receipt_rlp, gov5_native_receipts_root};
pub use validator::{
    DEFAULT_MAX_HISTORICAL_EPOCHS, EpochManager, LeaderSelector, MAX_HISTORICAL_EPOCHS_LIMIT,
    LEADER_TENURE_ENV, LeaderSchedule, LeaderScheduleError, MAX_CERTIFIED_VIEW, certifiable_view, ROTATE_ON_TIMEOUT_ENV,
    ResolvedLeaderSchedule, ValidatorSet, install_leader_schedule, leader_index, leader_schedule,
    leader_tenure, view_after_tc,
};
pub use vote_log::{BlockDurability, NoopVoteLog, VoteLogWriter};
pub use vote_rules::VoteRules;
