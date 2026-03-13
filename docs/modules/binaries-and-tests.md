# Binaries and Test Surfaces

## Binaries

### `bin/n42-node`

The primary bootstrap executable:

- parses CLI and env
- loads consensus config and validator key
- creates reth node components
- starts network, StarHub, mobile bridge, RPC, JMT, ZK, reward, and orchestrator tasks

Primary file:

- [`bin/n42-node/src/main.rs`](/Users/jieliu/Documents/n42/n42-26/bin/n42-node/src/main.rs)

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
