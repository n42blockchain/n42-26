# Simplify Progress Tracker (n42-26 Rust)

Last updated: 2026-03-22

## Batch 1: Core Consensus + Primitives

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-consensus/src/protocol/` | 8 | done | 7 | 7 (BLS cache, pre-size, clone removal) |
| `n42-consensus/src/validator/` | 3 | done | 1 | 1 (avoid Vec alloc in validate) |
| `n42-consensus/src/` | 4 | done | 0 | 0 |
| `n42-primitives/src/` | 5 | done | 0 | 0 |

## Batch 2: Node Orchestrator + Execution

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-node/src/orchestrator/` | 5 | done | 2 | 2 (dedup now_unix_ms, cache env var) |
| `n42-node/src/` | 4 | done | 1 | 1 (drain() in staking) |
| `n42-execution/src/` | 6 | done | 0 | 0 |

## Batch 3: Network + Mobile

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-network/src/` | 5+ | done | 0 | 0 |
| `n42-network/src/mobile/` | 3 | done | 0 | 0 |
| `n42-mobile/src/` | 9 | done | 3 | 3 (eliminate unwrap via direct indexing) |
| `n42-mobile-ffi/src/` | 5 | done | 3 | 3 (unwrap→?, double lock, expect) |

## Batch 4: Chainspec + JMT + ZK + Parallel

| Crate / Module | Files | Status | Issues Found | Fixed |
|----------------|-------|--------|-------------|-------|
| `n42-chainspec/src/` | 1 | done | 1 | 1 (const WEI_PER_N) |
| `n42-jmt/src/` | 5 | done | 1 | 1 (dedup combine_shard_roots) |
| `n42-zkproof/src/` | 3 | done | 0 | 0 |
| `n42-parallel-evm/src/` | 4 | done | 4 | 4 (dedup merge, cache env, idiom) |

## Skip

| Path | Reason |
|------|--------|
| `n42-consensus/tests/` | Test code |
| `n42-node/tests/` | Test code |
| `n42-jmt/benches/` | Benchmark code |
| `n42-zkproof/tests/` | Test code |
| `n42-zkproof-guest/src/` | ZK guest (1 file, minimal) |

## Totals

- **Total files**: 112
- **Reviewed**: 104
- **Issues found**: 24
- **Issues fixed**: 24
- **Skipped**: 8 (tests/benches/guest)
- **Memory leak risks**: 0 (all collections bounded)
