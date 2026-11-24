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
Use `E2E_SCENARIO4_PROFILE=seven` to rerun the real-binary 7-validator rendezvous and consensus regression lane in isolation.

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
E2E_SCENARIO_FILTER=4 E2E_SCENARIO4_PROFILE=seven target/release/e2e-test --binary target/release/n42-node
```

## Profile-1 follower (AM6, verifying non-validator)

A node on a Profile-1 genesis (`n42Profile` in the genesis) joins as a follower with
`N42_FOLLOWER_MODE=1`. It imports and checks every block exactly as a validator does (header gate,
deferred check, engine import, E14 tags, E12 halt) and never votes, times out, proposes or builds.

| Variable | Follower value |
|----------|----------------|
| `N42_FOLLOWER_MODE` | `1` (Profile-1 genesis only; refused with `N42_OBSERVER_MODE`) |
| `N42_VALIDATOR_KEY`, `N42_KEYSTORE_PATH` | unset (a follower holds no validator key; set = refusal) |
| `N42_TRUSTED_PEERS` | validators' `/ip4/<ip>/tcp/<port>/p2p/<peer id>` (TCP/Noise/Yamux), comma-separated; no auto-connect |
| `N42_P2P_KEY` | optional 32-byte hex ed25519 secret; random when unset. Fix it when validators must trust the follower |
| `N42_CONSENSUS_PORT` | QUIC and gov5 TCP listen port (`N42_GOV5_TCP_PORT` is refused on Profile 1, AM10) |
| `N42_DATA_DIR`, `N42_STARHUB_PORT`, `N42_LISTEN_IP` | as for a validator |
| `N42_ALLOW_DETERMINISTIC_P2P` | `1` only when the validators use the deterministic test identities |

Range catch-up: an n42-26 validator answers `bodies_by_range` only to peers it trusts, so its
`N42_TRUSTED_PEERS` must list the follower (fixed `N42_P2P_KEY`). Every AM10 forbidden variable is
still refused. A local run with four validators and two followers is in devlog-190 §12.
