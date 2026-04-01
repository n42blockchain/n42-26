mod adapter;
pub mod error;
pub mod extra_data;
pub mod protocol;
pub mod rotor;
pub mod validator;

pub use adapter::{N42Consensus, ValidatorSetResolver};
pub use error::ConsensusError as N42ConsensusError;
// extra_data QC encode/decode kept for test compatibility but no longer re-exported;
// consensus evidence is now stored in MDBX and referenced via parent_beacon_block_root.
pub use protocol::quorum::{verify_commit_qc, verify_qc, verify_tc};
pub use protocol::{ConsensusEngine, ConsensusEvent, EngineOutput, ViewTiming};
pub use validator::{EpochManager, LeaderSelector, ValidatorSet};
