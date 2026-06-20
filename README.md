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
- **reth v2.2.0 Integration**: Tracks `n42blockchain/reth` branch `n42-v2-upgrade`, based on upstream `v2.2.0` with N42-specific payload/cache patches applied on top
- **Jellyfish Merkle Tree (JMT)**: Blake3 hashing, 16-shard parallel updates, Merkle proofs via RPC
- **Compact Block Propagation**: Leader caches execution output, followers skip EVM re-execution (cache hit ~3ms)
- **Optimistic Voting**: Followers vote immediately after proposal validation, before block import
- **TX Forward to Leader**: O(n) message complexity replacing O(n²) gossip for transactions
- **Binary TCP Injection**: High-throughput transaction injection for stress testing (122K tx/s)
- **Execution-Spec Shards CI**: Sharded Hive/execution-spec lane for regression testing against upstream reth contracts
- **Execution Witness**: State witness generation for mobile re-execution
- **Mobile Verification Protocol**: Ed25519 receipts, commit-reveal anti-copying, LRU code cache
- **QUIC Mobile Client**: `QuicMobileClient` for phone-side connection to StarHub with deadline-based timeouts
- **Mobile Reward System**: EIP-4895 withdrawal-based rewards distributed per epoch (logarithmic scaling)
- **Mobile FFI SDK**: `n42-mobile-ffi` crate exposing C/JNI bindings for Android and iOS integration
- **Mobile Simulator**: `n42-mobile-sim` binary for load testing with deterministic BLS key generation
- **libp2p GossipSub**: QUIC transport with content-based message deduplication
- **QUIC Star-Hub**: High-concurrency mobile connections (up to 10,000 per node)
- **Parallel EVM**: Optimistic parallel EVM execution (`n42-parallel-evm`) for higher throughput
- **ZK Sidecar Proof System**: Asynchronous ZK proof generation with SP1 zkVM backend, phones verify proofs instead of re-executing EVM

## Project Structure

```
n42-26/
├── bin/
│   ├── n42-node/                  # CLI entry point (reth NodeBuilder)
│   ├── n42-stress/                # High-throughput stress testing tool
│   ├── n42-mobile-sim/            # Mobile verifier simulator for load testing
│   └── n42-evm-bench/             # EVM benchmarking utility
├── crates/
│   ├── n42-primitives/            # BLS keys, consensus message types
│   ├── n42-chainspec/             # Chain config, ValidatorInfo
│   ├── n42-consensus/             # HotStuff-2 state machine + reth adapter
│   ├── n42-execution/             # EVM config wrapper, witness & state diff
│   ├── n42-parallel-evm/          # Optimistic parallel EVM execution
│   ├── n42-jmt/                   # Jellyfish Merkle Tree (Blake3, 16-shard parallel)
│   ├── n42-zkproof/               # ZK sidecar proof system (SP1 + MockProver)
│   ├── n42-zkproof-guest/         # SP1 zkVM guest program (RISC-V ELF)
│   ├── n42-network/               # libp2p GossipSub + QUIC StarHub
│   ├── n42-mobile/                # Mobile verification protocol (no reth deps)
│   ├── n42-mobile-ffi/            # C/JNI FFI bindings for Android & iOS
│   └── n42-node/                  # Node type assembly + ConsensusOrchestrator
├── docs/                          # Development logs and performance records (devlog-01 through devlog-83)
├── scripts/                       # Testnet launch scripts
└── DEVLOG.md                      # Development log index
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
3. Packet pushed to phones via QUIC (QuicMobileClient)
4. Phone re-executes → generates VerificationReceipt (Ed25519)
5. IDC aggregates receipts → threshold (2/3) attestation
6. Per-epoch: MobileRewardManager calculates logarithmic rewards
7. Rewards injected as EIP-4895 withdrawals into next block's PayloadAttributes
```

### Anti-Copying (Commit-Reveal)

```
Phone:  commitment_hash = keccak256(block_hash || result || random_nonce)
        ──── send commitment ────▶ IDC
        [wait for window close]
        ──── send reveal (result + nonce) ────▶ IDC
IDC:    verify hash(block_hash || result || nonce) == commitment_hash
```

### Reward System (EIP-4895 Withdrawals)

Verification rewards are distributed per **epoch** (default: 21,600 blocks ≈ 24h at 4s block time):

| Parameter | Value | Description |
|-----------|-------|-------------|
| Epoch length | 21,600 blocks | Reward settlement interval |
| Max rewards per block | 32 | Throughput cap for large validator sets |
| Reward queue limit | 1,000,000 | Prevents unbounded memory growth |
| Address derivation | `keccak256(bls_pubkey_bytes)[12..]` | BLS pubkey → ETH address |
| Scaling | Logarithmic | Diminishing returns per attestation count |

Rewards are injected as `Withdrawal` entries in `PayloadAttributes` — no transaction signing, gas, or nonce required.

### Performance (Ed25519 vs BLS)

| Operation | Ed25519 | BLS12-381 |
|-----------|---------|-----------|
| Sign | ~14 us | ~320 us |
| Verify | ~38 us | ~763 us |
| Signature size | 64 B | 96 B |
| Mobile suitability | Excellent | Poor |

## Performance Benchmarks

### TPS Records (7 nodes, LAN, Apple Silicon)

| Mode | TPS | Block Cap | Notes |
|------|----:|----------:|-------|
| TCP Inject + Pool Gate + Fast Propose | **45,668** | 48K | Zero nonce gaps, zero stuck tx |
| TCP Inject + Fast Propose | **47,527** | 48K | Sustained injection 122K tx/s |
| Sync Wave + Fast Propose | **25,797** | 48K | Inject→drain→inject cycle |
| Cache Hit Fast Path | **90,949** | 90K | Peak single-block TPS |
| 2s Slot + All Optimizations | **13,858** | 48K | Production-like timing |

Benchmark-only upper-bound:

| Mode | TPS | Block Shape | Notes |
|------|----:|------------:|-------|
| Batch Transfer Fast-Lane | **3.27M avg / 11.28M max** | 256 x 10k = 2.56M transfers | 12.008 B/transfer, direct-only sidecar, skips reth/EVM/state root/receipts/persistence |
| Batch Transfer Fast-Lane | **3.24M avg / 13.33M max** | 512 x 10k = 5.12M transfers | 60 MB sidecar stress; peak exploration, not production TPS |

See [docs/performance-records.md](docs/performance-records.md) and [docs/devlog-83-batch-transfer-profile-optimize.md](docs/devlog-83-batch-transfer-profile-optimize.md) for the caveats and profiling breakdown.

### Key Optimizations

| Optimization | Impact |
|--------------|--------|
| Compact Block (cache hit) | Follower import: 209ms → 3ms |
| Optimistic Voting | R1 vote_delay: 363ms → 0ms |
| OrdMap + Packing | Pool overhead: 430ms → 23ms |
| Channel Split + Dedicated Runtime | R2 p50: 369ms → 221ms |
| TX Forward to Leader | O(n²) gossip → O(n) direct |

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

- Rust 1.93+
- N42 `reth` fork checked out at `../reth`
- Android local builds: JDK 17 recommended for Gradle/Kotlin
- SP1 toolchain v4.2.1 (optional, for ZK proof guest build): `curl -L https://sp1up.succinct.xyz | bash && sp1up --version v4.2.1`

### Prepare `reth`

```bash
git clone https://github.com/n42blockchain/reth.git ../reth
git -C ../reth checkout n42-v2-upgrade
```

### Build

```bash
# Verify the full workspace against the patched N42 reth fork
cargo check --all-targets

# Main binaries
cargo build --release -p n42-node-bin -p n42-stress -p e2e-test

# Optional mobile / SDK artifacts
cargo build --target aarch64-apple-ios-sim -p n42-mobile-ffi
JAVA_HOME=$(/usr/libexec/java_home -v 17) \
  ./mobile/android/gradlew :app:compileDebugKotlin
```

### Update The `reth` Fork

```bash
git -C ../reth fetch origin
git -C ../reth checkout n42-v2-upgrade
```

### Run

```bash
# Development node
./target/debug/n42-node node --dev

# With custom chain spec
./target/debug/n42-node node --chain /path/to/genesis.json

# Mobile simulator (connect to running node's StarHub)
./target/debug/n42-mobile-sim --starhub-ports 9100,9101,9102 --phone-count 100 --duration 60
```

## Testing

### Integration Tests (7 modules)

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
# Run ignored benchmark-style tests explicitly
cargo test -p n42-consensus --test performance_bench --release -- --ignored --nocapture
cargo test -p n42-node --test comm_stress_bench --release -- --ignored --nocapture
```

### Unit Tests

```bash
# All workspace unit/integration tests
cargo test --workspace

# Specific crate
cargo test -p n42-consensus
cargo test -p n42-primitives
cargo test -p n42-mobile
cargo test -p n42-jmt
cargo test -p n42-zkproof
cargo test -p n42-node
```

### Real-bin E2E and LAN test lanes

```bash
# Build the node binary and the E2E harness
cargo build --release -p n42-node-bin -p e2e-test

# Run correctness-oriented E2E scenarios
target/release/e2e-test --binary target/release/n42-node --scenario 5
E2E_SCENARIO_FILTER=1,3,4,5,8,12 \
  target/release/e2e-test --binary target/release/n42-node

# LAN pressure / timing work
scripts/testnet.sh
scripts/step_stress.sh
```

Use `tests/e2e/README.md` as the source of truth for the current split between:

- correctness CI
- manual integrated E2E
- LAN pressure / timing optimization

Additional lanes introduced in this branch:

- execution-spec shard workflows in `.github/workflows/execution-spec-shards.yml`
- integrated 7-node smoke via `scripts/test-7node-integrated-smoke.sh`
- reth image packaging helpers in `.github/scripts/hive/`

## Crate Dependency Graph

```
bin/n42-node                    bin/n42-stress           bin/n42-mobile-sim
  └── n42-node                    └── reth, alloy          └── n42-mobile ─── ed25519-dalek
      ├── n42-consensus ──┬── n42-primitives ── blst (BLS12-381)
      │                   └── n42-chainspec ──── n42-primitives
      ├── n42-execution ──┬── reth-evm, reth-revm
      │                   └── n42-chainspec
      ├── n42-jmt ────────── jmt, blake3, rayon (16-shard parallel)
      ├── n42-zkproof ──── alloy-primitives, serde, tokio, sp1-sdk (optional)
      │     └── n42-zkproof-guest (SP1 RISC-V, built separately)
      ├── n42-network ────┬── n42-primitives
      │                   ├── n42-mobile ─── ed25519-dalek
      │                   └── libp2p (gossipsub, quic)
      └── reth-node-builder, reth-ethereum-*

n42-mobile-ffi (Android/iOS SDK)
  ├── n42-mobile
  ├── n42-primitives
  ├── n42-chainspec
  └── n42-execution
```

Key design: **n42-mobile** has zero reth dependencies — only `alloy-primitives`, `ed25519-dalek`, `lru`, `serde`.
**n42-mobile-ffi** compiles as `staticlib` + `cdylib`, exposing a C ABI and JNI bridge for Android.
**n42-jmt** provides Jellyfish Merkle Tree with Blake3 hashing and 16-shard parallelism for state proofs.
**n42-zkproof** provides backend-agnostic ZK proof generation (`trait ZkProver`) with `MockProver` for testing and `Sp1Prover` for real SP1 zkVM proofs.
**n42-zkproof-guest** is the SP1 RISC-V guest program, built separately with `cargo prove build` (excluded from workspace).

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

// QUIC client (phone-side connection)
let client = QuicMobileClient::connect("127.0.0.1:9100", &ed25519_pubkey).await?;
let packet = client.receive_packet(Duration::from_secs(30)).await?;
client.send_receipt(&receipt_bytes).await?;

// Reward distribution (IDC side, per epoch)
let mut rewards = MobileRewardManager::new(blocks_per_epoch, base_reward_wei);
rewards.record_attestation(&bls_pubkey_hex);
// On payload build:
let withdrawals = rewards.take_pending_rewards(committed_block_number);
payload_attributes.withdrawals = Some(withdrawals);
```

## Configuration

### Consensus Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `slot_time_ms` | 8000 | Target block interval |
| `base_timeout_ms` | 4000 | Initial view-change timeout |
| `max_timeout_ms` | 8000 | Maximum timeout (cap for exponential backoff) |
| `chain_id` | 4242 | N42 chain identifier |

### ZK Proof Sidecar

| Parameter | Env Variable | Default | Description |
|-----------|-------------|---------|-------------|
| Enable | `N42_ZK_PROOF` | `0` (disabled) | Set to `1` to enable ZK sidecar |
| Interval | `N42_ZK_INTERVAL` | `300` | Blocks between proof generations |
| Backend | `N42_ZK_BACKEND` | `mock` | Prover backend (`mock` / `sp1`) |
| Mode | `N42_ZK_MODE` | `cpu` | SP1 mode (`cpu` / `cuda` / `mock`) |

RPC methods: `n42_zkProof(block_number)`, `n42_zkProofByHash(block_hash)`, `n42_zkLatest()`, `n42_zkVerify(block_number)`, `n42_zkStatus()`

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
