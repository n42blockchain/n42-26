# Core Flows

## 1. Node startup and recovery

```mermaid
flowchart TD
    A["Parse CLI and env"] --> B["Load consensus config"]
    B --> C["Load validator key / keystore"]
    C --> D["Build reth node components"]
    D --> E["Create SharedConsensusState"]
    E --> F["Load consensus snapshot"]
    F --> G["Create ConsensusEngine fresh or recovered"]
    G --> H["Start NetworkService"]
    H --> I["Start StarHub shards"]
    I --> J["Start MobileVerificationBridge"]
    J --> K["Start RPC"]
    K --> L["Spawn ConsensusOrchestrator"]
```

### Notes

- Snapshot load is an optimization and recovery enhancement, not the only boot path.
- If snapshot loading fails, current behavior is to log and start fresh.
- Mobile bridge is attached during bootstrap and subscribes to committed-block notifications.

## 2. Consensus commit path

```mermaid
sequenceDiagram
    participant Leader
    participant Validators
    participant Orchestrator
    participant Engine
    participant Reth
    participant State

    Leader->>Validators: Proposal / block data
    Validators->>Engine: Validate + vote
    Engine->>Leader: Prepare votes
    Leader->>Validators: PrepareQC / commit phase
    Validators->>Engine: CommitVote
    Engine->>Orchestrator: BlockCommitted(view, hash, commit_qc)
    Orchestrator->>State: update_committed_qc()
    Orchestrator->>State: notify_block_committed(block_hash, block_number)
    Orchestrator->>Reth: finalize/import
```

### State transitions

- `ConsensusEngine` emits `EngineOutput`
- `ConsensusOrchestrator` owns runtime side effects
- `SharedConsensusState` is updated on commit
- observers such as RPC and mobile bridge consume committed-block notifications

## 3. Validator network message path

```mermaid
flowchart LR
    A["ConsensusEngine output"] --> B["ConsensusOrchestrator"]
    B --> C["NetworkHandle"]
    C --> D["NetworkService priority/data channels"]
    D --> E["libp2p Swarm"]
    E --> F["GossipSub or request-response transport"]
    F --> G["Remote validator"]
```

### Transport split

- consensus messages are routed via a high-priority channel
- block data and sync are also treated as priority traffic
- transaction gossip can be disabled in favor of direct leader forwarding

## 4. Mobile verification path

```mermaid
sequenceDiagram
    participant Node
    participant StarHub
    participant Phone
    participant Bridge
    participant AttStore
    participant Reward

    Phone->>StarHub: QUIC connect + BLS pubkey handshake
    StarHub->>Bridge: PhoneConnected(session_id, verifier_pubkey)
    Node->>Bridge: committed block notification
    Bridge->>StarHub: register tracking / packet dispatch side path
    StarHub->>Phone: verification packet / cache sync
    Phone->>StarHub: signed VerificationReceipt
    StarHub->>Bridge: ReceiptReceived
    Bridge->>AttStore: aggregate receipt / build attestation
    Bridge->>Reward: emit participant pubkeys for rewards
```

### Security invariants

- handshake pubkey must match receipt pubkey
- verifier authorization is runtime session state
- receipts must belong to tracked blocks
- reward emission should happen only after finalized aggregate attestation

## 5. Snapshot persistence path

```mermaid
flowchart TD
    A["ConsensusOrchestrator on commit/shutdown"] --> B["build_snapshot()"]
    B --> C["persistence::save_consensus_state()"]
    C --> D["validate snapshot"]
    D --> E["write temp file"]
    E --> F["fsync"]
    F --> G["rename atomically"]
```

### Recovery rules

- invalid snapshots must not be injected into `with_recovered_state`
- parse/validation failure should be visible in logs
- startup may choose to degrade to fresh-start behavior depending on product policy

## 6. JMT update path

```mermaid
flowchart LR
    A["Committed block"] --> B["Extract state diff"]
    B --> C["Background JMT apply_diff"]
    C --> D["Update root/version in SharedConsensusState"]
    D --> E["RPC jmtRoot / jmtProof"]
```

## 7. ZK sidecar path

```mermaid
flowchart LR
    A["Committed block"] --> B["ProofScheduler enqueue"]
    B --> C["MockProver or SP1 prover"]
    C --> D["ProofStore persist"]
    D --> E["RPC zkProof / zkLatest / zkVerify"]
```

## 8. Reward settlement path

```mermaid
flowchart TD
    A["Bridge finalizes attestation"] --> B["Reward manager records verifier pubkeys"]
    B --> C["Epoch boundary reached"]
    C --> D["Compute logarithmic reward allocation"]
    D --> E["Inject EIP-4895 withdrawals into payload attributes"]
```

## Where to debug each flow

| Flow | Primary files |
|---|---|
| Startup | [`bin/n42-node/src/main.rs`](/Users/jieliu/Documents/n42/n42-26/bin/n42-node/src/main.rs) |
| Consensus runtime | [`crates/n42-node/src/orchestrator/mod.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-node/src/orchestrator/mod.rs), [`crates/n42-node/src/orchestrator/consensus_loop.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-node/src/orchestrator/consensus_loop.rs) |
| P2P network | [`crates/n42-network/src/service.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-network/src/service.rs), [`crates/n42-network/src/transport.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-network/src/transport.rs) |
| Mobile QUIC ingress | [`crates/n42-network/src/mobile/star_hub.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-network/src/mobile/star_hub.rs) |
| Mobile aggregation | [`crates/n42-node/src/mobile_bridge.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-node/src/mobile_bridge.rs) |
| Snapshot persistence | [`crates/n42-node/src/persistence.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-node/src/persistence.rs) |
| RPC surface | [`crates/n42-node/src/rpc.rs`](/Users/jieliu/Documents/n42/n42-26/crates/n42-node/src/rpc.rs) |
