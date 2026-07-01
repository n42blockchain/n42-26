# System Architecture

## Executive summary

N42 is a high-performance blockchain node that combines:

- HotStuff-2 style BFT consensus
- reth-backed EVM execution
- a libp2p-based validator network
- a QUIC-based mobile verifier side channel
- Twig/QMDB state commitment and proof serving
- optional asynchronous ZK proof generation

The system is intentionally split into an on-critical-path validator plane and an off-critical-path verification plane.

## Runtime composition

```mermaid
flowchart LR
    CLI["bin/n42-node main()"]
    CLI --> BOOT["Bootstrap and config"]
    BOOT --> RETH["reth node components"]
    BOOT --> CONS["ConsensusEngine"]
    BOOT --> NET["NetworkService"]
    BOOT --> ORCH["ConsensusOrchestrator"]
    BOOT --> HUB["ShardedStarHub"]
    BOOT --> MBR["MobileVerificationBridge"]
    BOOT --> RPC["JSON-RPC server"]
    BOOT -.->|"default backend"| TWIG["PersistentTwig"]
    BOOT -->|"N42_ZK_PROOF=1"| ZK["ProofScheduler"]
    BOOT --> REWARD["MobileRewardManager"]
    BOOT --> STAKE["StakingManager"]
    BOOT --> ADMIN["Admin Channel (mpsc)"]

    ORCH --> CONS
    ORCH --> NET
    ORCH --> RETH
    ORCH -.->|"default backend"| TWIG
    ORCH --> ZK
    ORCH --> MBR
    ORCH --> STAKE
    RPC --> ADMIN
    ADMIN --> ORCH
```

## Major planes

### 1. Consensus plane

Purpose:

- elect leader
- propose blocks
- collect votes and timeout certificates
- finalize commits

Primary code:

- [`crates/n42-consensus/src/`](crates/n42-consensus/src)
- [`crates/n42-node/src/orchestrator/`](crates/n42-node/src/orchestrator)

### 2. Execution plane

Purpose:

- build payloads through reth integration
- import committed blocks
- optionally generate execution witness and compact execution output
- derive state diffs for the Twig/QMDB state-proof sidecar and mobile verification

Primary code:

- [`crates/n42-execution/src/`](crates/n42-execution/src)
- [`crates/n42-node/src/payload.rs`](crates/n42-node/src/payload.rs)
- [`crates/n42-node/src/orchestrator/execution_bridge.rs`](crates/n42-node/src/orchestrator/execution_bridge.rs)

### 3. P2P validator network plane

Purpose:

- consensus message dissemination
- block announcement and direct block delivery
- transaction forwarding to leader
- state sync and peer management

Primary code:

- [`crates/n42-network/src/service.rs`](crates/n42-network/src/service.rs)
- [`crates/n42-network/src/transport.rs`](crates/n42-network/src/transport.rs)

### 4. Mobile verification plane

Purpose:

- push verification packets to phones
- collect signed receipts
- aggregate attestation state
- feed reward accounting

Primary code:

- [`crates/n42-network/src/mobile/star_hub.rs`](crates/n42-network/src/mobile/star_hub.rs)
- [`crates/n42-node/src/mobile_bridge.rs`](crates/n42-node/src/mobile_bridge.rs)
- [`crates/n42-mobile/src/`](crates/n42-mobile/src)

### 5. Proof and state proof plane

Purpose:

- maintain Twig/QMDB root and proofs, with JMT/SBMT retained as a reserve path
- generate and store asynchronous ZK proofs
- expose proof queries via RPC

Primary code:

- [`crates/n42-jmt/src/`](crates/n42-jmt/src)
- [`crates/n42-zkproof/src/`](crates/n42-zkproof/src)

> **✅ 状态树默认切到 Twig/QMDB**：默认并行状态树现为 `PersistentTwig`（QMDB-style bin tree），在 `bin/n42-node/src/main.rs`（RPC + orchestrator）通过 `with_twig()` builder 注入；`N42_JMT=1` 仅保留为兼容备用路径。`apply_diff` 在 commit 后通过 spawn_blocking 异步更新，不在共识关键路径（state root 由 reth MPT 算并入 header，twig 仅服务手机 proof）。默认路径使用 `n42_twigRoot` / `n42_twigProof` RPC，proof 为 bincode 编码的 `ShardedTwigProof`，手机用 `n42-mobile::state_proof::verify_twig_*` 验证。`n42_jmtRoot` / `n42_jmtProof` / `n42_jmtVersion` RPC 仅随 `N42_JMT=1` reserve path 启用。

### 6. Staking plane

Purpose:

- track validator stake deposits and registrations
- resolve BLS pubkey → staker EVM address for reward distribution
- manage cooldown period and pending returns

Primary code:

- [`crates/n42-node/src/staking.rs`](crates/n42-node/src/staking.rs)
- Orchestrator: `scan_committed_block()` on each block commit
- Execution bridge: staked pubkey resolution for reward address mapping

## Process-level architecture

```mermaid
flowchart TD
    subgraph Node["IDC Node Process"]
        A["CLI/bootstrap"]
        B["ConsensusEngine"]
        C["ConsensusOrchestrator"]
        D["NetworkService"]
        E["StarHub shards"]
        F["MobileVerificationBridge"]
        G["RPC server"]
        H["reth engine API"]
        I["Twig reserve / compatibility"]
        J["ZK scheduler"]
        K["Reward manager"]
        L["StakingManager"]
        M["Admin channel"]
    end

    subgraph Phones["Mobile verifiers"]
        P1["Phone verifier runtime"]
        P2["FFI / simulator"]
    end

    A --> C
    C --> B
    C --> D
    C --> H
    C -.-> I
    C --> J
    C --> L
    E --> F
    F --> K
    K --> L
    G --> M --> C
    G --> F
    P1 --> E
    P2 --> E
```

## Critical-path versus non-critical-path work

### Critical path

- proposal creation
- proposal validation
- consensus vote and commit rounds
- block import/finalization

### Off critical path

- mobile packet dispatch
- mobile receipt aggregation
- mobile rewards
- Twig/QMDB background updates
- ZK proof generation
- many observability and persistence tasks

This split matters operationally: a failure in mobile verification should not prevent consensus progress unless the implementation incorrectly couples the two.

## Production wiring audit (2026-03-26)

Each module's end-to-end integration status: constructed in `main.rs` → events connected → RPC exposed.

| Module | Status | Activation | Key callsite |
|--------|--------|-----------|-------------|
| ConsensusEngine | ✅ Wired | always | `main.rs:948` → orchestrator select! |
| N42Consensus (reth adapter) | ✅ Wired | always | `components.rs:92` → reth node builder |
| NetworkService (GossipSub) | ✅ Wired | always | `main.rs:770` |
| StarHub + MobileBridge | ✅ Wired | always | `main.rs:815,891` → critical tasks |
| MobileRewardManager | ✅ Wired | always | `main.rs:863` → `execution_bridge:232` → EIP-4895 withdrawals |
| StakingManager | ✅ Wired | always | `main.rs:528` → block scan + reward address resolve |
| Crash Recovery | ✅ Wired | always | `main.rs:440` → `ConsensusEngine::with_recovered_state` |
| ZK ProofScheduler | ✅ Wired | `N42_ZK_PROOF=1` | `main.rs:535` → `consensus_loop:413` → `on_block_committed` |
| Validator Reconfig RPC | ✅ Wired | always | `main.rs:515` → admin channel → orchestrator select! |
| Twig/QMDB (PersistentTwig) | ✅ Wired | default (`N42_JMT=1` keeps reserve path) | `main.rs:764, 1508` → `with_twig()` → `consensus_loop.rs` apply_diff (spawn_blocking) |
| Parallel EVM | ⚙️ Opt-in | `N42_PARALLEL_EVM=1` | `executor.rs:130` `parallel_evm_enabled()` → `execute_block_parallel` (off by default) |

### Open issues

1. **Parallel EVM opt-in**：`n42-parallel-evm` 由 `N42_PARALLEL_EVM=1` 门控（`executor.rs:130` `parallel_evm_enabled`），默认走 reth 标准串行路径。Block-STM 在小块（< `N42_PARALLEL_THRESHOLD`，默认 8 tx）时回退顺序执行；devlog-28 评估 EVM 仅占 8s slot 约 5%，启用收益有限，仍是 opt-in。
2. **Admin RPC 无鉴权**：`proposeAddValidator`/`proposeRemoveValidator` 端点无权限控制，任何 RPC 客户端可调用（除非启用 `N42_ADMIN_TOKEN`）。

## Shared state objects

### `SharedConsensusState`

Acts as the main cross-subsystem read/write bridge:

- latest committed QC
- attestation state
- authorized mobile verifiers
- equivocation log
- Twig/QMDB root metadata (JMT/SBMT metadata is populated only on the reserve path)
- ZK latest-proof metadata
- committed-block broadcast channel
- admin command channel (validator reconfig)

### `AttestationStore`

Durable storage for:

- aggregated mobile attestations
- verifier registry bitfield mapping
- reward tracking continuity

### `ConsensusSnapshot`

Persistent recovery payload for:

- current view
- locked QC
- last committed QC
- consecutive timeout count
- staged epoch transition
- committed block count

## Architectural strengths

- clear separation between consensus engine and runtime orchestration
- strong modularization between validator network and mobile network
- room for scaling through background tasks, channel splits, and direct paths
- test harnesses exist at both crate and end-to-end levels

## Architectural friction points

- multiple optional subsystems are wired through one bootstrap path, making startup dense
- some state is shared by side effects across async tasks, which increases coupling risk
- security-sensitive mobile authorization spans `star_hub`, `mobile_bridge`, `consensus_state`, and `rpc`
- release readiness depends heavily on cross-module invariants rather than a single enforcement layer
- state-proof naming is still partly historical (`n42-jmt` crate and `n42_jmt*` reserve RPCs), even though the default production path is Twig/QMDB
- `n42-parallel-evm` crate is orphaned dead code
- admin RPC endpoints (`proposeAddValidator`/`proposeRemoveValidator`) lack authentication
