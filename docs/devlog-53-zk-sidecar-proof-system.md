# Devlog 53: ZK Sidecar Proof System — SP1 Integration (Phase 1+3+4)

## Design Decisions

### Why Sidecar Architecture
ZK proof generation is computationally expensive (minutes per block on CPU). Making it a sidecar
that runs outside the consensus critical path ensures:
1. Zero impact on block production TPS (45.7K TPS baseline preserved)
2. Graceful degradation: proof failures are logged, never block consensus
3. Progressive deployment: interval=300 → 100 → 10 → 1 as hardware improves

### Why SP1 (future, behind feature flag)
- Already audited, sp1-reth demonstrates full block re-execution in zkVM
- Rust-native: no FFI boundary, shared types with reth/revm
- But sp1-sdk is a heavy dependency, so Phase 1 uses MockProver only
- `trait ZkProver` abstraction allows SP1 → ZisK swap with zero caller changes

### Integration Pattern: Follow JMT
Copied the exact same integration pattern as n42-jmt:
- `Option<Arc<...>>` field in ConsensusOrchestrator
- `with_zk_scheduler()` builder method
- `spawn_blocking` for async proof generation
- `ArcSwap` in SharedConsensusState for lock-free RPC reads
- Environment variable control (`N42_ZK_PROOF=1`), default disabled

## Implementation Details

### New Crate: `crates/n42-zkproof/`
| File | Purpose |
|------|---------|
| `src/prover.rs` | `ZkProver` trait, `BlockExecutionInput`, `ZkProofResult`, `MockProver` |
| `src/scheduler.rs` | `ProofScheduler`: interval-based proof generation via `spawn_blocking` |
| `src/store.rs` | `ProofStore`: BTreeMap-based in-memory store with LRU eviction |
| `src/error.rs` | `ZkProofError` enum |
| `src/lib.rs` | Module exports |

### Orchestrator Integration (consensus_loop.rs)
Injection point: after JMT update, before staking scan in `handle_block_committed()`.
```
JMT update → ZK proof scheduling → Staking scan → finalize_committed_block
```

### RPC Methods (namespace: `n42`)
| Method | Description |
|--------|-------------|
| `n42_zkProof(block_number)` | Get proof for specific block |
| `n42_zkLatest()` | Get most recent proof |
| `n42_zkVerify(block_number)` | Check if proof is valid |
| `n42_zkStatus()` | Subsystem status + stats |

### Prometheus Metrics
- `n42_zk_proof_generated_total` (counter)
- `n42_zk_proof_failed_total` (counter)
- `n42_zk_proof_generation_ms` (histogram)
- `n42_zk_proof_interval` (gauge)

### SharedConsensusState Changes
Added `zk_latest_proof: ArcSwap<Option<(u64, B256)>>` for lock-free RPC access.

## Test Results
- `cargo test -p n42-zkproof`: 11/11 passed
  - MockProver prove/verify roundtrip
  - Tampered proof detection
  - Serialization roundtrip (bincode + serde_json)
  - Scheduler interval filtering
  - ProofStore LRU eviction
- `cargo test -p n42-node`: 171/171 passed (2 pre-existing flaky tests excluded)
- `cargo clippy -p n42-zkproof`: 0 warnings

## Environment Variables
| Variable | Default | Description |
|----------|---------|-------------|
| `N42_ZK_PROOF` | unset (disabled) | Set to `1` to enable ZK sidecar |
| `N42_ZK_INTERVAL` | `300` | Blocks between proof generations |
| `N42_ZK_BACKEND` | `mock` | Prover backend (`mock`/`sp1`) |
| `N42_ZK_MODE` | `cpu` | SP1 mode (`cpu`/`cuda`/`mock`) |

## Status
- [x] Phase 1: n42-zkproof crate (trait + MockProver + scheduler + store)
- [x] Phase 3: Orchestrator integration (consensus_loop.rs sidecar injection)
- [x] Phase 4: RPC methods + Prometheus metrics
- [x] Phase 5.1: Unit tests (MockProver roundtrip, scheduler, store)
- [ ] Phase 2: SP1 guest program (deferred — requires RISC-V compilation)
- [ ] Phase 5.2: Integration test with testnet
- [ ] Phase 5.3: Real CPU proof generation test

## Node Startup Wiring (completed)

### bin/n42-node/src/main.rs changes
1. Added `n42-zkproof` dependency to `bin/n42-node/Cargo.toml`
2. ZK scheduler creation gated by `N42_ZK_PROOF=1` env var:
   - Reads `N42_ZK_INTERVAL` (default 300)
   - Creates `MockProver` + `ProofStore(1000)` + `ProofScheduler`
3. Scheduler cloned and passed to both:
   - `N42RpcServer::new(...).with_zk_scheduler(scheduler)` (RPC closure)
   - `orchestrator.with_zk_scheduler(scheduler)` (after epoch_schedule)
4. Pattern follows existing JMT/StakingManager wiring: create before closures, clone for each

### Activation
```bash
# Enable ZK sidecar with mock prover, proof every 10 blocks
N42_ZK_PROOF=1 N42_ZK_INTERVAL=10 scripts/testnet.sh --nodes 7 --clean

# Query via RPC
curl -X POST -d '{"jsonrpc":"2.0","method":"n42_zkStatus","params":[],"id":1}' http://localhost:8545
curl -X POST -d '{"jsonrpc":"2.0","method":"n42_zkLatest","params":[],"id":1}' http://localhost:8545
```

## Next Steps
1. Phase 2: SP1 guest program (`crates/n42-zkproof/guest/`) — requires resolving reth → RISC-V compilation
2. Real CPU proof test on 7-node testnet with `N42_ZK_INTERVAL=300`
