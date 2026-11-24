//! Node-side execution-layer adapter. The `ExecutionLayer` port trait + its
//! node-neutral types (`BuiltBlock`/`ElError`/`ResolveKind`) live in
//! `n42-consensus-service`; this module provides the in-process adapter
//! [`RethExecutionLayer`] over reth's engine + payload-builder handles (Caplin
//! EL-seam refactor, stage 6).

mod committed_frames;
mod deferred;
mod delegated;
mod follower_import;
mod import_gate;
mod own_block;
mod reth_engine;
pub use deferred::DeferredChecker;
pub(crate) use deferred::{intrinsic_rules, plan_tx, sender_state};
pub use delegated::DelegatedExecutionLayer;
pub use follower_import::FollowerImport;
pub use reth_engine::RethExecutionLayer;

pub use n42_consensus_service::el::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
