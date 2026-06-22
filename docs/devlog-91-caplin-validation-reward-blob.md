# Devlog 91 - Caplin Reward and Blob Port Validation

Date: 2026-06-22

Branch under test: `feat/caplin-cl-stage3-6` at `4cf2b57` plus the two focused
test-only regressions described below.

Reth baseline: `../reth` at `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`.

Artifacts:

- Main reward E2E log: `/tmp/n42-caplin-validation-artifacts/e2e-reward-main.log`
- Candidate reward E2E log: `/tmp/n42-caplin-validation-artifacts/e2e-reward-candidate.log`
- Main reward node logs: `/tmp/n42-caplin-validation-artifacts/reward-main-node-logs`
- Candidate reward node logs: `/tmp/n42-caplin-validation-artifacts/reward-candidate-node-logs`

## Verdict

Updated Caplin validation verdict: **MERGE**.

The reward and blob gaps from devlog-90 are now covered by the strongest checks
available in this tree without changing production behavior:

- Reward scenario 13 is identical between main and candidate, but it is only an
  observability E2E in the current harness and did not emit real reward
  withdrawals in either branch.
- The candidate now has a focused `NodeWithdrawalSource` regression that
  exercises the moved reward + staking-return adapter path and verifies
  withdrawal address resolution, amount, and index output.
- There is no existing E2E 4844/blob transaction injector in `tests/e2e` or the
  current stress harness. The candidate now has a focused `DiskBlobStorePort`
  regression that exercises the moved RLP decode -> `DiskFileBlobStore` insert
  -> `get_all_encoded` -> RLP decode path.

No production code, consensus wire type, `crates/n42-consensus`, or `Cargo.lock`
was changed for this follow-up.

## Reward - Scenario 13 E2E

Commands:

```text
E2E_SCENARIO_FILTER=13 /tmp/n42-caplin-validation-artifacts/e2e-test.main \
  --binary /tmp/n42-caplin-validation-artifacts/n42-node.main

E2E_SCENARIO_FILTER=13 /tmp/n42-caplin-validation-artifacts/e2e-test.candidate \
  --binary /tmp/n42-caplin-validation-artifacts/n42-node.candidate
```

Result:

| Check | main | candidate | Result |
| --- | ---: | ---: | --- |
| Scenario 13 suite | passed=1 failed=0 | passed=1 failed=0 | no regression |
| Final height | 30 | 30 | identical |
| Miner count | 3 | 3 | identical |
| Miner block split | 10 / 10 / 10 | 10 / 10 / 10 | identical |
| Reward total observed by scenario | 0 | 0 | identical |
| Scenario warning | no reward detected | no reward detected | identical |

Interpretation:

- Scenario 13 confirms the candidate did not regress the existing reward
  observability E2E.
- Scenario 13 does not close the real `WithdrawalSource` funds-flow gap by
  itself because neither branch produced reward withdrawals in this run.

## Reward - Focused Adapter Regression

Added candidate-only test:

```text
cargo test -p n42-node withdrawal_source_combines_reward_resolution_and_staking_returns
```

Result: PASS.

Coverage:

- Creates one active staker with a reward-address mapping and one unstaking
  staker whose cooldown has expired.
- Records ten mobile attestations so `MobileRewardManager` emits one reward
  withdrawal at the epoch boundary.
- Calls `NodeWithdrawalSource::withdrawals_for_block`.
- Asserts two withdrawals:
  - reward withdrawal resolves from reward address to the staker EVM address;
  - reward amount is `100_000_000`;
  - reward index is `block_number * max_withdrawals_per_payload`;
  - staking return amount is `32_000_000_000`;
  - staking return index is the persisted `next_withdrawal_index`.

This directly covers the stage-3 port extraction that moved the old inline
mobile reward + staking return logic behind `WithdrawalSource`.

## Blob - E2E Availability

Search result:

- `tests/e2e` has no 4844/blob scenario.
- `n42-stress` has no blob transaction injection mode in this branch.
- The default E2E genesis currently sets `blobGasUsed` to `0x0`.

Therefore no real two-node EIP-4844 transaction propagation E2E can be run from
the existing harness without adding a new blob transaction generator. That is a
test-harness gap, not an observed candidate regression.

## Blob - Focused Adapter Regression

Added candidate-only test:

```text
cargo test -p n42-node disk_blob_store_port_roundtrips_rlp_sidecar
```

Result: PASS.

Coverage:

- Builds an RLP-encoded EIP-4844 `BlobTransactionSidecarVariant`.
- Calls `DiskBlobStorePort::insert_rlp`, which decodes the RLP and inserts into
  reth's `DiskFileBlobStore`.
- Calls `DiskBlobStorePort::get_all_encoded`, which reads the sidecar back and
  re-encodes it as RLP bytes.
- Decodes the returned bytes and asserts exact sidecar equality and byte
  equality with the original RLP payload.

This directly covers the stage-6a-3 port extraction that moved blob sidecar RLP
decode/encode out of the orchestrator and into the node-side `BlobStorePort`
adapter. `BlobSidecarBroadcast` remains the same bincode shape:
`(block_hash, view, Vec<(tx_hash, sidecar_rlp)>)`.

## Guardrail Check

- Production behavior changes: none.
- Consensus wire changes: none.
- `crates/n42-consensus` changes: none.
- `Cargo.lock` changes: none.
- Existing full scenario `1/3/4` gate remains devlog-90's candidate PASS.
- Reward/blob follow-up tests are narrow regressions for the two ports that
  scenario `1/3/4` did not exercise.
