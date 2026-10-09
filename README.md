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
- **reth v2.7.0 Integration**: Uses upstream commit `3d592ece6de8c4559987416a544fc215fd6d6921` plus the six checked-in N42 integration patches; aligned with Alloy 2.5.0 / REVM 43.0.3. LLVM JIT remains explicit opt-in.
- **Reserve SBMT Path**: `N42_JMT=1` explicitly selects the legacy-compatible 16-shard sparse binary backend and RPC surface
- **Compact Block Propagation**: Leader caches execution output; the default follower path skips duplicate EVM execution (cache hit ~3ms)
- **QMDB Binary Twig Backend**: The QMDB-style 16-shard binary twig tree is the default N42 state-proof backend (`N42_TWIG` defaults on)
- **Follower Validation Modes**: Target production default is cache-hit + QMDB/LtHash commitment verification; six-follower independent full replay is the optional audit mode (see [`docs/follower-validation-modes.md`](docs/follower-validation-modes.md))
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
├── docs/                          # Development logs (devlog-01 through devlog-53)
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

### Latest seven-node records and four-node plan

The latest controlled N42-26 record is **170,546.56 committed TPS over
60.001068 seconds** (10,232,976 transactions, round53, Linux). This run enabled
trusted-ingest signature bypass and deferred state-root benchmark options;
it does not establish throughput with QMDB-only execution reads and full validation.
The native chain-94 seven-node fleet separately passed startup, commitment and
restart checks on an authenticated QMDB snapshot, with an empty transaction workload.

See the [original round53 report](docs/devlog-141-one-minute-rerun-20260901.md)
and the [reuse plan and upstream record comparison](docs/devlog-165-fleet-records-and-reuse-plan-20260914.md).
The first execution adaptation is available behind `N42_FAST_TRANSFER=1`:
eligible Cancun/Prague/Osaka plain transfers use the existing N42 EVM factory's fast
transition, with other transactions retaining the interpreter. QMDB-only
consecutive-block and full-state differential checks are described in the
[fast-transfer implementation notes](docs/devlog-166-fast-transfer-adaptation-20260914.md).
The parallel transfer block adapter now preserves system calls, receipts, withdrawals
and complete revert records, with consecutive QMDB-only commit/restart checks.
`N42_PARALLEL_BUILD=1` now enables a parallel prefix in the online payload builder,
followed by the standard serial tail and complete synchronous root calculation.
`N42_PARALLEL_IMPORT=1` now enables complete eligible transfer batches in the
production Engine import loop and BasicBlockExecutor. Imports retain recovered
signatures, sealed order, indexed receipts, complete state notifications, QMDB root
validation and rollback records; other blocks fall back to serial execution.
See the [follower integration](docs/devlog-170-parallel-engine-import-20260914.md), the
[parallel execution adaptation](docs/devlog-167-parallel-transfer-adaptation-20260914.md)
and [online builder integration](docs/devlog-168-parallel-payload-builder-20260914.md).
The default Cancun chain is now eligible; production QMDB-only provider reads and
native candidate-root preparation are covered by the
[Cancun integration checks](docs/devlog-169-cancun-parallel-qmdb-20260914.md).
A fresh four-validator native H2/QMDB genesis can now be generated with
`n42-native-fleet`; the existing fleet lifecycle supports its authenticated inputs.
See the [four-node bootstrap notes](docs/devlog-171-native-four-node-genesis-20260914.md)
for the first-block fork fix and actual startup evidence. The
[independent-recipient workload](docs/devlog-172-independent-recipient-workload-20260914.md)
now uses bounded signing workers and atomic file publication; a generated 220k-transaction
startup file passed full signature/nonce/distribution checks. A subsequent
[native Engine and full-size workload check](docs/devlog-173-native-engine-and-minute-workload-20260914.md)
found and fixed account-major publication and the launcher's inherited 5K pool limit.
The interleaved 72M file passes complete framing and a 220k signature/distribution audit;
the native first-block payload path passes Debug and Release Engine integration tests.
Qualification rejects wrong-chain or undersized workloads before contacting nodes.
Full four-node execution remains unmeasured. TCP/UDP binding is now available;
heavy work is paused pending shared-hardware coordination with the Claude n42-rs
and n42-gov5 sessions. Shared claim writes and host-wide process visibility are
not available in the current sandbox; see the
[coordination status and A/B procedure](docs/devlog-174-shared-box-coordination-20260915.md).
The [latest upstream payload audit](docs/devlog-175-upstream-payload-count-audit-20260915.md)
checks n42-rs's own-block transaction-list fix against this project's complete-payload
Engine path and records the still-unresolved upstream handover stalls.
The 3.27M average / 13.33M peak batch-transfer experiment omits EVM execution,
receipts, state roots and canonical persistence; it is not full-chain TPS.

### Earlier TPS records (seven-node experiments; see each report for hardware and scope)

| Mode | TPS | Block Cap | Notes |
|------|----:|----------:|-------|
| TCP Inject + Pool Gate + Fast Propose | **45,668** | 48K | Zero nonce gaps, zero stuck tx |
| TCP Inject + Fast Propose | **47,527** | 48K | Sustained injection 122K tx/s |
| Sync Wave + Fast Propose | **25,797** | 48K | Inject→drain→inject cycle |
| Cache Hit Fast Path | **90,949** | 90K | Peak single-block TPS |
| 2s Slot + All Optimizations | **13,858** | 48K | Production-like timing |

Latest cadence follow-up: [devlog-82](docs/devlog-82-continuous-cadence.md)
uses continuous per-node ingest with repeated 90K blocks. It reached 67.7K
active-block TPS and 90K max block TPS, but wall sustained TPS over 90.3s was
13.5K because remaining gaps are view-timeout/cadence stalls plus a leader
build/broadcast path near the 2s slot budget.

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

- Rust 1.97+ (development and CI are pinned to Rust 1.97.1)
- N42 `reth` v2.7.0 source checked out at `../reth` (`3d592ece6de8c4559987416a544fc215fd6d6921`), with the pinned N42 patch set applied
- Android local builds: JDK 17 recommended for Gradle/Kotlin
- SP1 toolchain v4.2.1 (optional, for ZK proof guest build): `curl -L https://sp1up.succinct.xyz | bash && sp1up --version v4.2.1`

### Prepare `reth`

```bash
git clone https://github.com/paradigmxyz/reth.git ../reth
git -C ../reth checkout 3d592ece6de8c4559987416a544fc215fd6d6921
bash scripts/apply-reth-patches.sh ../reth
bash scripts/check-reth-source.sh ../reth
```

The patch series restores N42's QMDB provider integration, execution cache,
transaction-root reuse, import batching and state-root job accessors. The adapted
source identity is recorded in [`reth-source.lock`](reth-source.lock). CI and
Docker apply the same patch set.
Re-running the script on an already patched checkout is safe; an incompatible
checkout fails before any missing patch is applied.

On an authenticated Gov5 QMDB execution configuration, `N42_QMDB_READS=verify`
compares account/storage reads with the matching reth provider and rejects a
mismatch. `on` answers available versions from QMDB and counts unavailable
fallbacks; `only` rejects unavailable versions. The default is `off` while fleet
qualification is pending. Views are pinned by block hash and published after
root validation and WAL durability. This currently covers the exact-block
provider path used by the payload builder and engine; latest/pending RPC paths
are not yet fully migrated. Do not disable the original state table writes.

The derived read index uses 64 shards and at most eight dedicated build workers;
small updates stay sequential. Values up to 32 bytes, including storage words,
are stored inline. Provider
views are retained by LRU, with limits of 64 versions and 512 MiB of logical
key/value bytes, charging shared versions in full. This is not an RSS limit:
active providers retain their pinned views, allocator overhead is additional,
and one oversized most recent view is kept to avoid repeated full rebuilds.
Inspect `n42_qmdb_read_cache_logical_bytes`, `n42_qmdb_read_cache_over_budget_bytes`
and `n42_qmdb_read_view_build_ms` along with process RSS during qualification.

QMDB undo retention is capped at 8,192 records and 256 MiB of charged vector/value
capacity; the newest oversized record is retained for rollback. Historical
branches outside the available undo/leaf-heap window rebuild from the authenticated
base. Persistent stores create a separate `<checkpoint>.replay.base.qmdb` anchor
on open, while memory-only stores retain a base-tree copy. The anchor is separate
from the rolling checkpoint and is checked against the original base hash/root
on load. Monitor `n42_qmdb_undo_heap_bytes`, `n42_qmdb_undo_over_budget_bytes` and
`n42_qmdb_replay_base_load_ms`. This does not bound total RSS: block metadata still
grows, and a cold replay loads a base tree before replacing the current one.
See [the retention notes](docs/devlog-152-qmdb-undo-budget-20260914.md).
Restart validation uses the same bounded undo and base replay path. Checkpoints
and WAL records are read incrementally, avoiding a complete encoded-file copy. See
[the recovery notes](docs/devlog-153-qmdb-streaming-recovery-20260914.md).
Persistent stores retain at most 128 MiB of charged decoded operation capacity
after each durable commit. Older operations are read from validated WAL locations;
eviction happens only after successful synchronization. Legacy checkpoint records
gain durable WAL locations on open. Memory-only stores retain their sole copy,
and pending commits, cold decode buffers, legacy checkpoint loading, metadata,
and WAL growth are outside this budget. Monitor
`n42_qmdb_operations_resident_bytes`, `n42_qmdb_operations_over_budget_bytes`,
and `n42_qmdb_cold_operations_read_ms`. See
[the cold-operation notes](docs/devlog-154-qmdb-cold-operations-20260914.md).

`n42_stateReadStatus([blockHashOrNull])` reports the registered adapter's mode,
chain identity, process instance, local validator public key, and monotonic
account/storage read and error counters. Supplying a block hash also returns
its known durable QMDB root; unknown or still-pending blocks have no root.
The counters are process-local and independent of the Prometheus recorder.

`scripts/qualify-1m-tps.sh` requires four distinct active validator processes
with `N42_QMDB_READS=only` and persistent QMDB stores before loading the workload.
Every node must show account reads and pinned providers during the window,
with no QMDB errors, unavailable versions, counter resets, or identity changes.
Its QMDB root must match its H2-committed execution block. The default threshold
is 1,000,000 successful common committed transactions per second. The audit
fetches exact native headers and receipts from every node, recomputes header
hashes, checks transaction/log identity and gas accounting, and recomputes the
Gov5 receipt root before counting success. Inclusion TPS remains a diagnostic field; failures return
nonzero. Build `cargo build --release --bin n42-keccak --bin n42-verify-commit`
before qualification, or set `N42_KECCAK_BIN` and `N42_COMMIT_VERIFY_BIN`.
Set `N42_BENCH_TRUSTED_CONFIG` to an operator-authenticated JSON file containing
the static H2-v4 chain identity, four ordered validator public keys and f=1.
The script freezes this input before observations; it never derives trust from RPC.
Header/receipt support and commit certificates are checked before load.
`n42_nativeHeader(blockHash)` serves exact bytes from the existing 8,192-header
registry; unavailable headers fail qualification. It does not reconstruct
lossy headers from standard RPC fields or provide a persistent archive.
Evidence is saved in `h2-audit.json` and `summary.tsv`. Boundary BLS commit
certificates are verified against the supplied trust configuration, and native
parent chains connect their actual signed blocks to the common measured interval.
These reports do not authenticate the operator's trust source, verify transaction
trie roots, independently replay EVM execution, cover all state-read paths, or
establish hardware durability. See
[the H2-v4 qualification notes and trust-file format](docs/devlog-160-h2-trusted-qc-gate-20260914.md).

`n42_consensusStatus` also exports `commitQc` (view, block hash, signature and
explicit signer bits) from the same QC snapshot as its existing status fields.
The standalone `n42-verify-commit` binary checks the commit signing domain against
caller-authenticated profile, chain/changes context, validator keys and fault
tolerance. Schema 5 qualification requires H2-v4 certificate verification; older
boundaries and diagnostic Native/Gov5 legacy profiles cannot satisfy this gate. See
[the commit verifier notes](docs/devlog-159-h2-commit-verifier-20260914.md).

The QMDB leaf tree now deduplicates dirty twig marking with membership bits and
reuses its work buffer to fold shared upper ancestors once. The measured account
update workload improves by 5.2–5.7%; this is not whole-node TPS. Empty-tree
rollback also invalidates stale upper caches before refilling. See
[the dirty-twig comparison and regression evidence](docs/devlog-161-qmdb-dirty-twigs-20260914.md).

Gov5 normalization can retain one bounded QMDB candidate calculation. Import
still executes the payload and must match the exact parent and full operations
before reusing the applied tree; root checks, read-view publication and WAL sync
remain mandatory. The measured local candidate-plus-durable-commit path improves
by 46–47%, with no claim about fleet hit rate or whole-node TPS. See
[the prepared-candidate checks and persistent comparison](docs/devlog-162-qmdb-prepared-candidate-20260914.md).

Profiling now separates immutable read-view construction from WAL encoding and
sync. Alternative indexes and larger shard counts exposed update/read tradeoffs,
so the production reader remains unchanged. See
[the read-cost measurements and rejected alternatives](docs/devlog-163-qmdb-read-cost-20260914.md).

Large QMDB imports with a cached exact parent can now derive the immutable read
view while the committing thread computes the binary root. Both finish before
root validation and durable publication; unavailable workers or parents use
the sequential path. Prepared candidate hits stay sequential. See
[the concurrency checks and durable comparison](docs/devlog-164-qmdb-overlap-20260914.md).

### Build

```bash
# Verify the full workspace against the patched N42 reth fork
cargo check --all-targets --locked

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
git -C ../reth checkout 3d592ece6de8c4559987416a544fc215fd6d6921
bash scripts/apply-reth-patches.sh ../reth
bash scripts/check-reth-source.sh ../reth
```

When upgrading the baseline, update `reth-source.lock`, the patch series, CI
refs and `Cargo.lock` together.

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

### Workspace coverage

```bash
rustup component add llvm-tools-preview
python3 scripts/workspace-coverage.py
```

The coverage CI requires at least **70% aggregate Rust source line coverage** and
passing workspace tests. Reports include inline test modules and all production
crate/bin sources for the default host features. See
[coverage scope, prerequisites, and reports](docs/testing-coverage.md).

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
