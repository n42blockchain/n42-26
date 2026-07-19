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
pub mod rotor;
pub mod validator;
pub mod vote_log;

pub use adapter::{N42Consensus, ValidatorSetResolver};
pub use error::ConsensusError as N42ConsensusError;
pub use protocol::quorum::{validator_changes_hash, verify_commit_qc, verify_qc, verify_tc};
pub use protocol::state_machine::{AuthenticatedConsensusMessage, FUTURE_VIEW_WINDOW};
pub use protocol::{ConsensusEngine, ConsensusEvent, EngineOutput, ViewTiming};
pub use validator::{
    DEFAULT_MAX_HISTORICAL_EPOCHS, EpochManager, LeaderSelector, MAX_HISTORICAL_EPOCHS_LIMIT,
    ValidatorSet,
};
pub use vote_log::{NoopVoteLog, VoteLogWriter};
