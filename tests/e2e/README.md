# E2E Test Lanes

This harness runs the real `n42-node` binary. Not every scenario is a merge-gate.

## Correctness CI

- `1` single-node smoke
- `3` ERC-20 execution
- `4` multi-node consensus
- `5` mobile attestation with live QUIC authorization
- `8` mobile EVM verification over QUIC
- `12` Blockscout RPC compatibility

These are the scenarios selected by `.github/workflows/e2e.yml` and `.github/workflows/nightly.yml`.
For `scenario4`, CI sets `E2E_SCENARIO4_PROFILE=correctness`; the default manual profile remains broader.

## Manual Integrated E2E

- `2` RPC load
- `6` stress / pool boundary
- `7` 21x21 topology
- `9` long-run recovery
- `10` chaos / fault injection
- `13` reward observability

Keep these runnable. They are still used to evolve node, tx sender, mobile, and explorer flows, even when they are too noisy or too observational for correctness CI.

## LAN Pressure / Timing

- `11` 10K QUIC connection stress
- `bin/n42-stress`
- `scripts/testnet*.sh`
- `scripts/step_stress.sh`
- `scripts/sysmon.sh`

Use these for throughput studies, timing diagrams, and bottleneck analysis such as `48K cap / 7-node` runs.

## Usage

```bash
cargo build --release -p n42-node-bin -p e2e-test
target/release/e2e-test --binary target/release/n42-node --scenario 5
E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node
```
