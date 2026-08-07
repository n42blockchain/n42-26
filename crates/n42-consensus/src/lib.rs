#![cfg_attr(
    test,
    allow(
        clippy::collapsible_if,
        clippy::needless_range_loop,
        clippy::unnecessary_literal_unwrap
    )
)]

mod adapter;
pub mod error;
pub mod extra_data;
pub mod protocol;
mod receipt;
pub mod rotor;
pub mod validator;
pub mod vote_log;

pub use adapter::{
    GOV5_HEADER_EXTRA_MAGIC, MAX_GOV5_HEADER_EXTRA_BYTES, MIN_GOV5_HEADER_EXTRA_BYTES,
    N42Consensus, N42HeaderProfile, N42HeaderProfileError, ValidatorSetResolver,
    validate_gov5_h2_header, validate_gov5_header_extra, validate_gov5_interop_header,
    validate_gov5_replay_v2_header,
};
pub use error::ConsensusError as N42ConsensusError;
pub use protocol::quorum::{
    validator_changes_hash, verify_commit_qc, verify_commit_qc_with_profile, verify_qc, verify_tc,
};
pub use protocol::state_machine::{AuthenticatedConsensusMessage, FUTURE_VIEW_WINDOW};
pub use protocol::{ConsensusEngine, ConsensusEvent, EngineOutput, ViewTiming};
pub use receipt::gov5_native_receipts_root;
pub use validator::{
    DEFAULT_MAX_HISTORICAL_EPOCHS, EpochManager, LeaderSelector, MAX_HISTORICAL_EPOCHS_LIMIT,
    ValidatorSet,
};
pub use vote_log::{NoopVoteLog, VoteLogWriter};
