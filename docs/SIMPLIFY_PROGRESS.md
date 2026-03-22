# Simplify Progress Tracker (n42-26 Rust)

Track `/simplify` review progress across all Rust crates.
Status: `pending` → `done` (or `skip` for tests/benches)

Last updated: 2026-03-22

## Batch 1: Core Consensus + Primitives

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-consensus/src/protocol/` | 8 | pending | | |
| `n42-consensus/src/validator/` | 3 | pending | | |
| `n42-consensus/src/` (adapter, error, extra_data, lib) | 4 | pending | | |
| `n42-primitives/src/` | 5 | pending | | |

## Batch 2: Node Orchestrator + Execution

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-node/src/orchestrator/` | 5 | pending | | |
| `n42-node/src/` (main files) | 10 | pending | | |
| `n42-execution/src/` | 3 | pending | | |

## Batch 3: Network + Mobile

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-network/src/` | 6 | pending | | |
| `n42-network/src/gossipsub/` | 2 | pending | | |
| `n42-network/src/mobile/` | 3 | pending | | |
| `n42-mobile/src/` | 9 | pending | | |
| `n42-mobile-ffi/src/` | 5 | pending | | |

## Batch 4: Chainspec + JMT + ZK + Parallel

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-chainspec/src/` | 1 | pending | | |
| `n42-jmt/src/` | 5 | pending | | |
| `n42-zkproof/src/` | 3 | pending | | |
| `n42-parallel-evm/src/` | 2 | pending | | |

## Skip (tests/benches)

| Path | Reason |
|------|--------|
| `n42-consensus/tests/` | Test code |
| `n42-node/tests/` | Test code |
| `n42-jmt/benches/` | Benchmark code |
| `n42-zkproof/tests/` | Test code |
| `n42-zkproof-guest/src/` | ZK guest (minimal, 1 file) |

## Totals

- **Total files**: 112
- **Reviewed**: 0
- **Issues found**: 0
- **Issues fixed**: 0
- **Skipped**: ~8 (tests/benches/guest)
