# Binaries and Test Surfaces

## Binaries

### `bin/n42-node`

The primary bootstrap executable:

- parses CLI and env
- loads consensus config and validator key
- creates reth node components
- starts network, StarHub, mobile bridge, RPC, JMT, ZK, reward, and orchestrator tasks

Primary file:

- [`bin/n42-node/src/main.rs`](bin/n42-node/src/main.rs)

### `bin/n42-mobile-sim`

Operational simulator for mobile verifiers:

- creates deterministic or generated phone identities
- connects to StarHub ports
- receives cache sync and verification packets
- verifies and returns receipts

Useful for:

- protocol validation
- load testing
- smoke testing mobile packet formats

### `bin/n42-stress`

High-throughput tx injection tool:

- JSON-RPC batch mode
- binary TCP inject mode
- pre-sign and wave modes
- backpressure-aware sending

Useful for:

- TPS ceiling tests
- tx-pool stress
- comparing pipeline changes

### `bin/n42-evm-bench`

Benchmark/support utility for execution-focused investigations.

## End-to-end tests

`tests/e2e` provides scenario-oriented coverage rather than just unit coverage.

Keep three lanes separate:

- correctness CI: deterministic real-bin scenarios that gate merges
- manual integrated E2E: node + tx sender + mobile + explorer flows still used for product evolution
- LAN pressure / timing: `n42-stress`, `scripts/testnet*.sh`, `scripts/step_stress.sh`, `scripts/sysmon.sh`

### Structure

```text
tests/e2e/src/
├── main.rs
├── node_manager.rs
├── rpc_client.rs
├── tx_engine.rs
├── mobile_sim.rs
├── genesis.rs
├── test_helpers.rs
├── erc20.rs
└── scenarios/
    ├── scenario1_single_node.rs
    ├── scenario4_multi_node.rs
    ├── scenario5_mobile.rs
    ├── scenario6_stress.rs
    ├── scenario8_mobile_evm.rs
    ├── scenario10_chaos.rs
    ├── scenario11_quic_10k.rs
    ├── scenario12_blockscout_rpc.rs
    └── scenario13_rewards.rs
```

### What the test harness covers

- single-node bring-up
- multi-node networking and consensus
- mobile verification and reward paths
- stress and long-run behavior
- chaos scenarios
- high-scale QUIC connection behavior

Not every scenario belongs in correctness CI. Use `tests/e2e/README.md` for the current CI/manual/LAN split.

## Benchmarks and manual stress tests

- `crates/n42-consensus/tests/performance_bench.rs` is preserved as an ignored manual benchmark suite
- `crates/n42-node/tests/comm_stress_bench.rs` is preserved as an ignored manual communication benchmark
- `bin/n42-stress` and `scripts/testnet*.sh` remain the primary LAN timing/tuning entrypoints
- `scripts/test-7node-integrated-smoke.sh` is the fastest repeatable real-bin check for `7-node + tx sender + mobile + Blockscout` without forcing a full `testnet.sh` rebuild cycle

## Suggested release-gate test buckets

### Bucket 1: startup and recovery

- node boot with no snapshot
- node boot with valid snapshot
- node boot with invalid snapshot and fresh-start fallback

### Bucket 2: consensus and networking

- multi-node commit path
- direct messaging and sync recovery
- leader tx forwarding path

### Bucket 3: mobile verification

- phone connect/disconnect authorization lifecycle
- packet dispatch and receipt aggregation
- reward issuance after threshold

### Bucket 4: state proof systems

- JMT root/proof queries
- ZK proof scheduler and retrieval

## Documentation gap to keep in mind

The project has many tests and devlogs, but fewer stable operator-facing runbooks.
That gap is one reason this `Docs/` tree is useful before release.
