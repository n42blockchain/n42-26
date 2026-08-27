//! Execution-path classification shared by benchmarks, replay, and the live node.
//!
//! The two axes are deliberately independent.  "PEVM" describes an execution
//! strategy; "historical" describes a workload.  In particular, the sibling
//! `../pevm` harness executes independent historical blocks against immutable
//! pre-state and does not write N42's canonical chain.  That result must never
//! be reported as live-EVM throughput.

/// Where the executed block came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionWorkload {
    /// Offline or startup processing of an already-known historical block.
    HistoricalReplay,
    /// A payload on the latency-sensitive active consensus path.
    Live,
}

/// How EVM transactions are scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvmStrategy {
    /// Ordinary in-order revm execution through Reth's canonical executor.
    Sequential,
    /// PEVM-style parallel execution.
    Pevm,
}

/// A complete execution classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPath {
    pub workload: ExecutionWorkload,
    pub strategy: EvmStrategy,
}

impl ExecutionPath {
    /// The independent, read-only `../pevm` historical replay control.
    pub const HISTORICAL_PEVM: Self = Self {
        workload: ExecutionWorkload::HistoricalReplay,
        strategy: EvmStrategy::Pevm,
    };

    /// Ordered historical import through `new_payload` and canonical FCU.
    pub const HISTORICAL_SEQUENTIAL: Self = Self {
        workload: ExecutionWorkload::HistoricalReplay,
        strategy: EvmStrategy::Sequential,
    };

    /// The production live path today: Reth's sequential revm executor.
    pub const LIVE_SEQUENTIAL: Self = Self {
        workload: ExecutionWorkload::Live,
        strategy: EvmStrategy::Sequential,
    };

    /// Reserved for a future, explicitly qualified live PEVM integration.
    pub const LIVE_PEVM: Self = Self {
        workload: ExecutionWorkload::Live,
        strategy: EvmStrategy::Pevm,
    };

    /// Stable low-cardinality metric label.
    pub const fn label(self) -> &'static str {
        match (self.workload, self.strategy) {
            (ExecutionWorkload::HistoricalReplay, EvmStrategy::Pevm) => "historical_pevm",
            (ExecutionWorkload::HistoricalReplay, EvmStrategy::Sequential) => {
                "historical_sequential"
            }
            (ExecutionWorkload::Live, EvmStrategy::Sequential) => "live_sequential",
            (ExecutionWorkload::Live, EvmStrategy::Pevm) => "live_pevm",
        }
    }

    /// Whether this path is implemented by the current Engine-API adapter.
    ///
    /// Historical PEVM is an independent read-only harness. Live PEVM must not
    /// silently fall back to the sequential canonical executor: it needs its
    /// own state/receipt/root adapter and differential qualification first.
    pub const fn uses_current_engine_api(self) -> bool {
        matches!(self.strategy, EvmStrategy::Sequential)
    }

    /// Whether the path is allowed to move N42's canonical head.
    pub const fn may_write_canonical_state(self) -> bool {
        !matches!(self, Self::HISTORICAL_PEVM)
    }

    /// Whether this path may start the latency-sensitive canonical payload builder.
    pub const fn may_start_payload_build(self) -> bool {
        matches!(self, Self::LIVE_SEQUENTIAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_paths_keep_workload_and_strategy_separate() {
        assert_eq!(ExecutionPath::HISTORICAL_PEVM.label(), "historical_pevm");
        assert_eq!(
            ExecutionPath::HISTORICAL_SEQUENTIAL.label(),
            "historical_sequential"
        );
        assert_eq!(ExecutionPath::LIVE_SEQUENTIAL.label(), "live_sequential");
        assert_eq!(ExecutionPath::LIVE_PEVM.label(), "live_pevm");

        assert!(!ExecutionPath::HISTORICAL_PEVM.uses_current_engine_api());
        assert!(!ExecutionPath::LIVE_PEVM.uses_current_engine_api());
        assert!(ExecutionPath::HISTORICAL_SEQUENTIAL.uses_current_engine_api());
        assert!(ExecutionPath::LIVE_SEQUENTIAL.uses_current_engine_api());
        assert!(!ExecutionPath::HISTORICAL_PEVM.may_write_canonical_state());
        assert!(ExecutionPath::LIVE_SEQUENTIAL.may_start_payload_build());
        assert!(!ExecutionPath::HISTORICAL_PEVM.may_start_payload_build());
        assert!(!ExecutionPath::HISTORICAL_SEQUENTIAL.may_start_payload_build());
        assert!(!ExecutionPath::LIVE_PEVM.may_start_payload_build());
    }
}
