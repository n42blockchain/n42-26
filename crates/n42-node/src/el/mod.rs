//! Node-side execution-layer adapter. The `ExecutionLayer` port trait + its
//! node-neutral types (`BuiltBlock`/`ElError`/`ResolveKind`) live in
//! `n42-consensus-service`; this module provides the in-process adapter
//! [`RethExecutionLayer`] over reth's engine + payload-builder handles (Caplin
//! EL-seam refactor, stage 6).

mod delegated;
mod import_gate;
mod own_block;
mod reth_engine;
pub use delegated::DelegatedExecutionLayer;
pub use reth_engine::RethExecutionLayer;

pub use n42_consensus_service::el::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
