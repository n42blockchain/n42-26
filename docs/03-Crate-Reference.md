# Crate Reference

## Crate matrix

| Crate | Role | Key exports / entrypoints | Depends on |
|---|---|---|---|
| `n42-primitives` | shared domain types | BLS keys, signatures, consensus messages | foundational |
| `n42-chainspec` | configuration and genesis | `ConsensusConfig`, dev/test specs | primitives |
| `n42-consensus` | deterministic consensus engine | `ConsensusEngine`, `EpochManager`, `ValidatorSet` | primitives, chainspec |
| `n42-execution` | execution helper layer | `execute_block_full`, witness/state diff | reth stack |
| `n42-network` | validator and mobile network plumbing | `NetworkService`, `StarHub`, `ShardedStarHub` | primitives, libp2p, quinn |
| `n42-mobile` | phone-side protocol and verification | packets, receipts, attestation builder | primitives |
| `n42-mobile-ffi` | mobile SDK bindings | C/JNI exported functions | mobile, chainspec |
| `n42-node` | orchestration and product assembly | `ConsensusOrchestrator`, RPC, persistence | consensus, network, mobile, JMT, ZK |
| `n42-parallel-evm` | parallel execution engine | `parallel_execute` | revm |
| `n42-jmt` | merkle tree and proof serving | `ShardedJmt`, proofs, snapshots | jmt, blake3 |
| `n42-zkproof` | proof scheduling and storage | `ProofScheduler`, `ProofStore` | SP1/mock backend |

## Module hotspots by crate

### `n42-node`

- orchestration: `orchestrator/*`
- mobile path: `mobile_bridge.rs`, `mobile_reward.rs`, `mobile_packet.rs`
- control plane: `rpc.rs`, `consensus_state.rs`, `persistence.rs`
- assembly: `node.rs`, `components.rs`, `payload.rs`, `pool.rs`

### `n42-network`

- P2P runtime: `service.rs`, `transport.rs`
- direct paths: `consensus_direct.rs`, `block_direct.rs`, `tx_forward.rs`
- mobile ingress: `mobile/star_hub.rs`, `mobile/sharded_hub.rs`

### `n42-consensus`

- protocol stages: `protocol/*`
- validator lifecycle: `validator/*`
- reth bridge: `adapter.rs`

### `n42-mobile`

- packets and wire format: `packet.rs`, `wire.rs`
- receipts: `receipt.rs`
- verifier runtime: `verifier.rs`, `verification.rs`
- aggregate signatures: `attestation.rs`
- cache sync: `code_cache.rs`

### `n42-jmt`

- tree core: `tree.rs`, `store.rs`
- key derivation: `keys.rs`
- proof creation: `proof.rs`
- horizontal scaling: `sharded.rs`
- persistence: `snapshot.rs`

### `n42-zkproof`

- scheduler: `scheduler.rs`
- proving backend: `prover.rs`, `sp1_prover.rs`
- persistence: `store.rs`
- error boundary: `error.rs`

## Entry binaries

| Binary | Operational role |
|---|---|
| `n42-node` | primary node bootstrap and runtime host |
| `n42-mobile-sim` | simulated phone swarm for mobile verification load |
| `n42-stress` | transaction injection and stress benchmarking |
| `n42-evm-bench` | execution benchmarking / profiling |

## Cross-cutting state surfaces

| State surface | Producers | Consumers |
|---|---|---|
| `SharedConsensusState` | orchestrator, mobile bridge, JMT, ZK | RPC, bridge, startup/recovery |
| `ConsensusSnapshot` | orchestrator persistence | startup bootstrap |
| `AttestationStore` | mobile bridge | reward manager, later queries |
| `VerificationTask` | committed-block notifier | RPC subscription, mobile bridge |

## Recommended deep dives

- [`Docs/modules/foundations.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/foundations.md)
- [`Docs/modules/n42-node.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/n42-node.md)
- [`Docs/modules/n42-network.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/n42-network.md)
- [`Docs/modules/n42-consensus.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/n42-consensus.md)
- [`Docs/modules/n42-mobile-and-ffi.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/n42-mobile-and-ffi.md)
- [`Docs/modules/data-exec-zk.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/data-exec-zk.md)
- [`Docs/modules/binaries-and-tests.md`](/Users/jieliu/Documents/n42/n42-26/Docs/modules/binaries-and-tests.md)
