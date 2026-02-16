# N42

A high-performance blockchain system combining **HotStuff-2** BFT consensus with **reth** EVM execution, featuring parallel mobile device verification for enhanced security.

## Architecture Overview

```
                    ┌─────────────────────────────────────────────┐
                    │              N42 Node (IDC)                 │
                    │                                             │
                    │  ┌───────────┐  ┌────────────────────────┐  │
                    │  │  reth CLI  │  │  ConsensusOrchestrator │  │
                    │  │  (launch)  │  │  (3-way select! loop)  │  │
                    │  └─────┬─────┘  └──┬──────┬──────┬───────┘  │
                    │        │           │      │      │          │
                    │  ┌─────▼─────┐  ┌──▼──┐ ┌▼────┐ │          │
                    │  │ Execution │  │Timer│ │Net  │ │          │
                    │  │  (EVM)    │  │     │ │Event│ │          │
                    │  │ + Witness │  └──┬──┘ └┬────┘ │          │
                    │  └───────────┘     │     │      │          │
                    │                ┌───▼─────▼──┐ ┌─▼────────┐ │
                    │                │ Consensus  │ │ P2P Net  │ │
                    │                │  Engine    │ │ GossipSub│ │
                    │                │ (HotStuff2)│ │  (QUIC)  │ │
                    │                └────────────┘ └──────────┘ │
                    │                                     │      │
                    │                              ┌──────▼────┐ │
                    │                              │  StarHub  │ │
                    │                              │   (QUIC)  │ │
                    │                              └─────┬─────┘ │
                    └────────────────────────────────────┼───────┘
                                                        │
                         ┌──────────────────────────────┼──────────────┐
                         │              │               │              │
                     ┌───▼───┐     ┌────▼───┐     ┌────▼───┐    ┌────▼───┐
                     │Phone 1│     │Phone 2 │     │Phone 3 │    │Phone N │
                     │Ed25519│     │Ed25519 │     │Ed25519 │    │Ed25519 │
                     └───────┘     └────────┘     └────────┘    └────────┘
```

**Design Principles:**

- **IDC nodes** (100-500) handle block production, consensus voting, and state storage
- **Mobile devices** (~10,000 per node) perform parallel verification — not on the consensus critical path
- **8-second slot target** with measured minimum block interval of 0.4s-0.9s
- **Event-driven state machine** — fully deterministic, testable without async runtime

## Features

- **HotStuff-2 Consensus**: 2-round optimistic commit with 3-round timeout recovery
- **BLS12-381 Signatures**: Aggregated signatures for compact quorum certificates
- **reth v1.11.0 Integration**: Full EVM execution with Ethereum-compatible block/transaction format
- **Execution Witness**: State witness generation for mobile re-execution
- **Mobile Verification Protocol**: Ed25519 receipts, commit-reveal anti-copying, LRU code cache
- **libp2p GossipSub**: QUIC transport with content-based message deduplication
- **QUIC Star-Hub**: High-concurrency mobile connections (up to 10,000 per node)

## Project Structure

```
n42-26/
├── bin/n42-node/                  # CLI entry point (reth NodeBuilder)
└── crates/
    ├── n42-primitives/            # BLS keys, consensus message types
    ├── n42-chainspec/             # Chain config, ValidatorInfo
    ├── n42-consensus/             # HotStuff-2 state machine + reth adapter
    ├── n42-execution/             # EVM config wrapper, witness & state diff
    ├── n42-network/               # libp2p GossipSub + QUIC StarHub
    ├── n42-mobile/                # Mobile verification protocol (no reth deps)
    └── n42-node/                  # Node type assembly + ConsensusOrchestrator
```

## Consensus Protocol

N42 uses a **HotStuff-2** variant — a two-round BFT consensus achieving O(n) message complexity:

```
Round 1 (Prepare):
  Leader ──Proposal──▶ Validators ──Vote──▶ Leader ──▶ PrepareQC

Round 2 (Commit):
  Leader ──PrepareQC──▶ Validators ──CommitVote──▶ Leader ──▶ CommitQC ──▶ Block Committed

Timeout Recovery:
  Timer expires ──▶ Broadcast Timeout ──▶ Collect 2f+1 ──▶ TC ──▶ NewView ──▶ Next Leader
```

| Parameter | Value |
|-----------|-------|
| Fault tolerance | f = (n-1)/3 |
| Quorum size | 2f + 1 |
| Leader selection | Round-robin (view % n) |
| Timeout backoff | min(base × 2^consecutive_timeouts, max) |
| Signature scheme | BLS12-381 (min_pk variant) |

### Vote Signing Domains

| Message Type | Signing Content |
|-------------|----------------|
| Vote (Round 1) | `view (8B LE) \|\| block_hash (32B)` |
| CommitVote (Round 2) | `"commit" \|\| view (8B LE) \|\| block_hash (32B)` |
| Timeout | `"timeout" \|\| view (8B LE)` |

### Safety Rule

Validators maintain a `locked_qc` (highest QC seen). Before voting on a proposal:

```
proposal.justify_qc.view >= locked_qc.view  // Must hold, otherwise reject
```

## Mobile Verification

Mobile devices perform **parallel block verification** as an additional security layer, operating independently from the consensus critical path.

### Protocol Flow

```
1. IDC executes block → captures ExecutionWitness
2. Witness compacted (remove cached bytecodes) → VerificationPacket
3. Packet pushed to phones via QUIC
4. Phone re-executes → generates VerificationReceipt (Ed25519)
5. IDC aggregates receipts → threshold (2/3) attestation
```

### Anti-Copying (Commit-Reveal)

```
Phone:  commitment_hash = keccak256(block_hash || result || random_nonce)
        ──── send commitment ────▶ IDC
        [wait for window close]
        ──── send reveal (result + nonce) ────▶ IDC
IDC:    verify hash(block_hash || result || nonce) == commitment_hash
```

### Performance (Ed25519 vs BLS)

| Operation | Ed25519 | BLS12-381 |
|-----------|---------|-----------|
| Sign | ~14 us | ~320 us |
| Verify | ~38 us | ~763 us |
| Signature size | 64 B | 96 B |
| Mobile suitability | Excellent | Poor |

## Performance Benchmarks

Measured in release mode on a single machine (Apple Silicon):

### Configuration Comparison

| Metric | 500N x 500M | 100N x 2500M |
|--------|:-----------:|:------------:|
| Consensus nodes | 500 | 100 |
| Mobiles per node | 500 | 2,500 |
| Total mobiles | 250,000 | 250,000 |
| Fault tolerance (f) | 166 | 33 |
| Quorum size (2f+1) | 333 | 67 |
| Leader crypto (2 rounds) | 632 ms | 128 ms |
| Per-node mobile verification | 32 ms | 162 ms |
| **Estimated min block interval** | **~882 ms** | **~377 ms** |

> Minimum block interval = leader crypto + network latency (200ms) + block execution (50ms)
>
> Mobile verification is **not** on the consensus critical path — it runs in parallel.

### BLS QC Build Time (sign + verify + aggregate)

| Validators | Quorum | Time |
|:----------:|:------:|:----:|
| 4 | 3 | 3.4 ms |
| 10 | 7 | 7.9 ms |
| 67 | 45 | 50.8 ms |
| 100 | 67 | 76.0 ms |
| 333 | 221 | 247.6 ms |
| 500 | 333 | 388.0 ms |

Both configurations are well within the **8-second slot target**.

## Building

### Prerequisites

- Rust 1.86+
- reth v1.11.0 source at `../reth` (local path dependency)

### Build

```bash
cargo build
```

### Run

```bash
# Development node
./target/debug/n42-node node --dev

# With custom chain spec
./target/debug/n42-node node --chain /path/to/genesis.json
```

## Testing

### Integration Tests (39 tests, 7 modules)

```bash
# Run all integration tests
cargo test -p n42-consensus --test integration_test

# Run specific module
cargo test -p n42-consensus --test integration_test genesis_bootstrap
cargo test -p n42-consensus --test integration_test fault_tolerance
cargo test -p n42-consensus --test integration_test stress_performance

# With output
cargo test -p n42-consensus --test integration_test -- --nocapture
```

| Module | Tests | Coverage |
|--------|:-----:|----------|
| genesis_bootstrap | 3 | Initial state, single-validator genesis, first block commit |
| multi_node_consensus | 6 | 4/7/10/100-node consensus, consecutive blocks, leader rotation |
| mobile_verification | 6 | Receipt signing, aggregation threshold, dedup, commit-reveal |
| fault_tolerance | 9 | f-crash, byzantine votes, duplicate votes, view change, safety |
| boundary_conditions | 7 | Single-node instant commit, exact quorum, f+1 crash stall |
| stress_performance | 4 | 100 consecutive blocks, 500 validators, 1000 mobile receipts |
| stability | 4 | 1000 mixed views, channel leak check, locked_qc monotonicity |

### Performance Benchmarks

```bash
# Run benchmarks (release mode recommended)
cargo test -p n42-consensus --test performance_bench --release -- --nocapture
```

### Unit Tests

```bash
# All workspace tests
cargo test --workspace

# Specific crate
cargo test -p n42-consensus
cargo test -p n42-primitives
cargo test -p n42-mobile
```

## Crate Dependency Graph

```
bin/n42-node
  └── n42-node
      ├── n42-consensus ──┬── n42-primitives ── blst (BLS12-381)
      │                   └── n42-chainspec ──── n42-primitives
      ├── n42-execution ──┬── reth-evm, reth-revm
      │                   └── n42-chainspec
      ├── n42-network ────┬── n42-primitives
      │                   ├── n42-mobile ─── ed25519-dalek
      │                   └── libp2p (gossipsub, quic)
      └── reth-node-builder, reth-ethereum-*
```

Key design: **n42-mobile** has zero reth dependencies — only `alloy-primitives`, `ed25519-dalek`, `lru`, `serde`.

## Key Types

### Consensus Engine

```rust
// Event-driven state machine — no internal event loop
let engine = ConsensusEngine::new(my_index, secret_key, validator_set,
                                  base_timeout_ms, max_timeout_ms, output_tx);

// External driver feeds events
engine.process_event(ConsensusEvent::BlockReady(block_hash))?;
engine.process_event(ConsensusEvent::Message(msg))?;
engine.on_timeout()?;

// Read outputs from channel
match output_rx.recv() {
    EngineOutput::BroadcastMessage(msg) => network.broadcast(msg),
    EngineOutput::BlockCommitted { view, block_hash, .. } => storage.commit(block_hash),
    EngineOutput::ViewChanged { new_view } => log::info!("view changed to {}", new_view),
    // ...
}
```

### Mobile Verification

```rust
// Sign receipt (mobile side)
let receipt = sign_receipt(block_hash, block_number, true, true, timestamp, &ed25519_key);

// Verify and aggregate (IDC side)
receipt.verify_signature()?;
let mut aggregator = ReceiptAggregator::new(threshold, max_blocks);
aggregator.register_block(block_hash, block_number);
if aggregator.process_receipt(&receipt) == Some(true) {
    println!("Block attested by sufficient mobile verifiers");
}
```

## Configuration

### Consensus Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `slot_time_ms` | 8000 | Target block interval |
| `base_timeout_ms` | 4000 | Initial view-change timeout |
| `max_timeout_ms` | 8000 | Maximum timeout (cap for exponential backoff) |
| `chain_id` | 4242 | N42 chain identifier |

### Network Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| GossipSub mesh degree (D) | 8 | Target peers in mesh |
| GossipSub heartbeat | 1s | Mesh maintenance interval |
| StarHub max connections | 10,000 | Mobile devices per node |
| StarHub idle timeout | 300s | Inactive connection timeout |
| QUIC handshake timeout | 5s | Mobile must send pubkey within |

### GossipSub Topics

| Topic | Path | Purpose |
|-------|------|---------|
| Consensus | `/n42/consensus/1` | All HotStuff-2 messages |
| Block Announce | `/n42/blocks/1` | Header-first block dissemination |
| Verification | `/n42/verification/1` | Mobile verification receipts |

## License

See [LICENSE](LICENSE) for details.
