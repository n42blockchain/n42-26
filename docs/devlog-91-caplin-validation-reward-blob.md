# Devlog 91 - Mobile Reward and Blob True E2E

Date: 2026-06-22

Branch under test: `fix/mobile-reward-receipts-root` at `40c3eae` plus this follow-up.

Base: Caplin clean branch `c2ef42d`; this fix branch must not be folded back into
`feat/caplin-cl-stage3-6`.

Reth baseline: `../reth` at
`449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`.

## Verdict

Fix branch verdict: **READY WITH CAPLIN**.

The reward/blob coverage gap is closed without carrying `receipts_root` through
consensus:

- `WithdrawalSource` / mobile reward funds flow: PASS with a real stake and a
  real EIP-4895 withdrawal.
- `BlobStorePort` / EIP-4844 sidecar injection and propagation: PASS.

## Design Correction

The earlier `40c3eae` path incorrectly let consensus provide `receipts_root` to
mobile verification. That short-circuited the phone's independent verification
role.

This follow-up restores the intended design:

- `VerificationTask` is again only `(block_hash, block_number)`.
- `SharedConsensusState::notify_block_committed` no longer accepts or broadcasts
  `receipts_root`.
- `consensus_loop` no longer extracts `receipts_root` from
  `BlockDataBroadcast`, and the late root fallback was removed.
- `MobileVerificationBridge` first registers the consensus task with
  `expected_receipts_root = None`.
- `mobile_packet_loop` reads the receipts root from the node-local imported
  block header and sends a local `DispatchedBlockRoot` to the bridge.
- `ReceiptAggregator::register_block` can fill a missing expected root later
  without resetting already observed receipts.

The expected root therefore comes from the locally imported block, not from a
consensus wire payload. Phones still submit their independently computed
`computed_receipts_root` in the BLS receipt.

## Staking Fix

Reward requires stake. A zero reward for an unstaked phone is correct behavior.

The reward E2E now stakes first:

- The harness sends a real stake transaction to `0x0000000000000000000000000000000000000042`.
- Stake value is `32 ETH` (`32000000000000000000` wei).
- Stake input is the 48-byte BLS pubkey used by the QUIC mobile verifier.
- `StakingManager` scans the committed block from `pending_block_data` after
  `drain_leader_payload_rx`, instead of reading the already-cached
  `committed_blocks.back()` entry that could be empty on the leader.
- The scanner accepts Engine API payload shape via `payload.transactions`.

## Harness

Scenario 14 has two phases.

Reward phase:

- Starts one real `n42-node`.
- Sends a real stake transaction for the mobile verifier pubkey.
- Connects a real QUIC mobile verifier to StarHub.
- Receives stream verification packets.
- Re-executes/verifies each packet in the harness and signs real BLS mobile
  receipts.
- Waits for an EIP-4895 withdrawal to the staker EVM address.
- Asserts node logs include stake registration, mobile receipt handling,
  withdrawal injection, and local imported-header root registration.

Blob phase:

- Starts a three-node real testnet.
- Builds and signs a real EIP-4844 blob transaction using Alloy's KZG sidecar
  builder.
- Submits the transaction to the leader RPC.
- Waits for the transaction receipt and syncs all nodes.
- Asserts all nodes agree on the blob block hash.
- Asserts `blobGasUsed > 0`.
- Asserts leader blob sidecar broadcast and follower sidecar processing logs are
  present with no decode/insert errors.

## Commands

```text
cargo check --all-targets

cargo clippy -- -D warnings

cargo build --release -p n42-node-bin -p e2e-test

E2E_SCENARIO_FILTER=14 target/release/e2e-test --binary target/release/n42-node
```

## Results

`cargo check --all-targets`: PASS.

`cargo clippy -- -D warnings`: PASS.

`cargo build --release -p n42-node-bin -p e2e-test`: PASS.

`scenario 14`: PASS.

```text
test suite complete passed=1 failed=0 total=1
```

Reward evidence:

| Field | Value |
| --- | --- |
| Stake tx hash | `0xf310b052a7de24752f6e79d5b06cd40a7a1a3b2c872c1b1b9072d28210c71f90` |
| Stake block | `3` |
| Staker / withdrawal recipient | `0xb208ebDe1606a7d2E2132565dD2e5d618332B498` |
| BLS-derived reward address | `0xC6446a5A8e10c0BB36e7b014A6E1f8F5107f67Cb` |
| Stake amount | `32000000000000000000` wei |
| Mobile receipts signed | `8` |
| First withdrawal block observed by harness | `5` |
| Withdrawal index | `40` |
| Reward amount | `100000000` gwei |
| Recipient balance after reward | `24414062499999968699963649140625000` wei |

Node-side reward proof:

- `new stake registered` at block `3`.
- `registered mobile packet block root from imported header` for streamed
  blocks, proving the expected root is local block-header data.
- `verification receipt received from mobile verifier` for blocks `4..10`.
- `injecting mobile rewards as withdrawals` emitted with `count=1`.

Blob evidence:

| Field | Value |
| --- | --- |
| Blob tx hash | `0x913b82ff49915db37fc4a1bbc705a8b17a6cabb19a5e04842f1bb6a162355532` |
| Inclusion block | `6` |
| `blobGasUsed` | `131072` |
| Leader sidecar broadcasts | `1` |
| Follower sidecar process logs | `2` |

Node-side blob proof:

- Leader emitted one blob sidecar broadcast.
- Both followers emitted `processed blob sidecar broadcast` with `sidecars=1`.
- The E2E log found no blob sidecar decode or insert errors.

## Guardrails

- No `Cargo.lock` drift is included.
- `crates/n42-consensus` and consensus wire formats were not changed.
- The fix branch keeps the Caplin branch clean and records only the reward/blob
  validation fix.
