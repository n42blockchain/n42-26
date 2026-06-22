# Devlog 91 - Caplin Reward and Blob True E2E

Date: 2026-06-22

Branch under test: `feat/caplin-cl-stage3-6` at `c2ef42d` plus this follow-up.

Reth baseline: `../reth` at `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`.

## Verdict

Updated Caplin validation verdict: **MERGE**.

The two devlog-90 coverage gaps are now closed by a real E2E harness:

- `WithdrawalSource` / mobile reward funds flow: PASS.
- `BlobStorePort` / EIP-4844 sidecar injection and propagation: PASS.

The new E2E is `scenario14_reward_blob` and is wired into `e2e-test` as scenario
`14`.

## Why Code Changed

The first true reward run exposed a real production-chain gap, not a harness
problem:

- `SharedConsensusState::notify_block_committed` only broadcast `(block_hash,
  block_number)`.
- `MobileVerificationBridge` therefore registered dispatched blocks with
  `expected_receipts_root = None`.
- Mobile stream receipts still reached the threshold, but no
  `AttestationBuilder` was created, so the BLS aggregate had zero reward
  participants and `MobileRewardManager` never emitted a withdrawal.

The fix is narrow:

- `VerificationTask` now carries `receipts_root: Option<B256>`.
- The consensus loop extracts `receipts_root` from the leader
  `BlockDataBroadcast` execution payload after `drain_leader_payload_rx`.
- `notify_block_committed` is sent after the local leader payload cache is
  visible, so the mobile bridge registers the block with its expected receipts
  root before phones submit receipts.
- A late root fallback remains in `handle_leader_payload_feedback` for the race
  where commit beats leader payload feedback.

No consensus wire type was changed. `crates/n42-consensus` was not changed.

## Harness Added

Scenario 14 has two phases.

Reward phase:

- Starts a single real `n42-node`.
- Connects a real QUIC mobile verifier to StarHub.
- Receives stream verification packets.
- Re-executes/verifies each packet in the harness.
- Signs and sends real BLS mobile receipts.
- Waits for an EIP-4895 withdrawal to the derived mobile reward address.
- Asserts reward logs include mobile receipt handling and withdrawal injection.

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
cargo check -p n42-consensus-service -p n42-node -p e2e-test --all-targets

cargo build --release -p n42-node-bin -p e2e-test

E2E_SCENARIO_FILTER=14 target/release/e2e-test --binary target/release/n42-node
```

## Results

`cargo check`: PASS.

`cargo build --release`: PASS.

`scenario 14`: PASS.

```text
test suite complete passed=1 failed=0 total=1
```

Reward evidence:

| Field | Value |
| --- | --- |
| Mobile receipts signed | 8 |
| First rewarded block observed by harness | 4 |
| Reward recipient | `0x994b6a31b2aC368B2C45126945f00AF4e39c707b` |
| Reward amount | `100000000` gwei |
| Recipient balance after rewards | `700000000000000000` wei |

Node-side reward proof:

- `BLS aggregate attestation built` for blocks `3..9`, each with
  `participant_count=1`.
- `reward pubkeys prepared for finalized attestation` with `rewarded=1`.
- `reward pubkeys sent for finalized attestation` with `rewarded=1`.
- `injecting mobile rewards as withdrawals` emitted for subsequent blocks.

Blob evidence:

| Field | Value |
| --- | --- |
| Blob tx hash | `0x913b82ff49915db37fc4a1bbc705a8b17a6cabb19a5e04842f1bb6a162355532` |
| Inclusion block | 6 |
| `blobGasUsed` | `131072` |
| Leader sidecar broadcasts | 1 |
| Follower sidecar process logs | 2 |

Node-side blob proof:

- Leader emitted `broadcasting blob sidecars` with `blob_count=1`.
- Both followers emitted `processed blob sidecar broadcast` with `sidecars=1`.
- The E2E log found no blob sidecar decode or insert errors.

## Guardrails

- `Cargo.lock` change is limited to adding `alloy-rpc-types-eth` to
  `e2e-test` dependencies; no version drift or downgrade.
- `tests/e2e` enables Alloy `kzg` only for the blob transaction builder.
- Existing full scenario `1/3/4` gate remains devlog-90's candidate PASS.
- This follow-up closes the reward/blob dead spots with real node E2E coverage,
  not only unit regressions.
