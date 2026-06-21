//! Node-side execution-layer adapter. The `ExecutionLayer` port trait + its
//! node-neutral types (`BuiltBlock`/`ElError`/`ResolveKind`) live in
//! `n42-consensus-service`; this module provides the in-process adapter
//! [`RethExecutionLayer`] over reth's engine + payload-builder handles (Caplin
//! EL-seam refactor, stage 6).

mod reth_engine;
pub use reth_engine::RethExecutionLayer;

pub use n42_consensus_service::el::{BuiltBlock, ElError, ExecutionLayer, ResolveKind};
