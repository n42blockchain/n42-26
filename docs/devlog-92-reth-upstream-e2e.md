# Devlog 92 - Reth Upstream Bump E2E Validation

Date: 2026-06-22

PR under test: `#18`, branch `chore/reth-upstream-bump` at
`87cec7f05e4b`.

Baseline:

- n42 branch: `main` at `0463cb4c59c9`.
- reth ref: `../reth` at `449ecfdcef` on
  `chore/merge-upstream-fc2cc1e`.
- Dependency line: reth 2.3.0 with revm 40 / alloy-evm 0.36.

Candidate:

- n42 branch: `chore/reth-upstream-bump` at `87cec7f05e4b`.
- reth ref: `../reth` at `0655e7a9a2` on
  `chore/merge-upstream-paradigmxyz-latest`.
- Dependency line: reth 2.3.0 with revm 41 / alloy-evm 0.37 /
  reth-primitives-traits 0.5.

Artifacts:

- Baseline build: `/tmp/n42-reth-upstream-e2e/baseline/build.log`
- Baseline scenarios 1/3/4:
  `/tmp/n42-reth-upstream-e2e/baseline/e2e-1-3-4.log`
- Baseline scenario 14:
  `/tmp/n42-reth-upstream-e2e/baseline/e2e-14.log`
- Candidate build: `/tmp/n42-reth-upstream-e2e/candidate/build.log`
- Candidate scenarios 1/3/4:
  `/tmp/n42-reth-upstream-e2e/candidate/e2e-1-3-4.log`
- Candidate scenario 14:
  `/tmp/n42-reth-upstream-e2e/candidate/e2e-14.log`

## Verdict

PR #18 runtime gate verdict: **MERGE**.

The candidate reth upstream bump passes the same real-node E2E coverage as the
old reth baseline: scenarios `1`, `3`, `4`, and `14` all pass. No new panic,
span-panic, or error lines were found in the captured node logs. Commit cadence,
FCU finalize latency, eager import, follower import, reward withdrawal, and blob
propagation remain in the same range as the baseline.

## Commands

Both sides were built after switching `../reth` to the matching ref:

```text
cargo build --release -p n42-node-bin -p e2e-test
```

E2E commands:

```text
E2E_SCENARIO_FILTER=1,3,4 target/release/e2e-test --binary target/release/n42-node

E2E_SCENARIO_FILTER=14 target/release/e2e-test --binary target/release/n42-node
```

Build result:

| Check | baseline | candidate | Result |
| --- | --- | --- | --- |
| Release build | PASS | PASS | no regression |
| Existing reth warnings | present | present | same class of unused-import warnings |
| Cargo.lock drift from validation | none | none | no validation drift |

## Scenario Results

| Check | baseline | candidate | Result |
| --- | ---: | ---: | --- |
| E2E suite `1,3,4` | passed=3 failed=0 | passed=3 failed=0 | no regression |
| Scenario 1 blocks | 99 | 100 | within expected range |
| Scenario 1 average interval | 4.01s | 4.01s | identical |
| Scenario 1 balances | PASS | PASS | no regression |
| Scenario 3 ERC-20 receipts | 300/300 | 300/300 | no regression |
| Scenario 3 deployer balance | PASS | PASS | no regression |
| Scenario 3 total supply | PASS | PASS | no regression |
| Scenario 4 1-node | PASS, h=13/13, avg=8.0s | PASS, h=12/12, avg=8.0s | no regression |
| Scenario 4 3-node | PASS, h=13/13, avg=7.9s | PASS, h=13/13, avg=7.7s | no regression |
| Scenario 4 5-node | PASS, h=13/13, avg=7.8s | PASS, h=13/13, avg=7.8s | no regression |
| Scenario 4 7-node | PASS, h=13/13, avg=7.6s | PASS, h=13/13, avg=7.5s | no regression |
| Scenario 4 21-node | PASS, h=12/12, avg=8.0s | PASS, h=12/13, avg=8.0s | within tolerance; no fork |
| E2E suite `14` | passed=1 failed=0 | passed=1 failed=0 | no regression |

Scenario 4 sampled block hashes were consistent across all validators for every
tested size (`1/3/5/7/21`) on both baseline and candidate. The block hash covers
the header execution result, including state/receipt roots, so a cross-node
JMT/Twig state-root divergence would have failed the sampled-hash consistency
checks. Separate baseline/candidate wall-clock runs are not expected to produce
identical block hashes.

## Node-Log Metrics

The following metrics are aggregated from the captured `n42-node-*.log` files
for scenarios `1`, `3`, `4`, and `14`.

| Metric | baseline | candidate | Result |
| --- | ---: | ---: | --- |
| Panic / span-panic / ERROR lines | 0 | 0 | no regression |
| `N42_CADENCE` samples | 538 | 558 | comparable run length |
| `inter_block_commit_ms` p50 / p95 / max | 7988 / 8040 / 8175ms | 7989 / 8042 / 8266ms | no material regression |
| `N42_FCU` samples | 582 | 602 | comparable run length |
| FCU status | Valid=582 | Valid=602 | no Syncing/Error regression |
| `n42_fcu_latency_ms` log p50 / p95 / max | 0 / 2 / 8ms | 0 / 2 / 8ms | no regression |
| eager import accepted lines | 582 | 602 | comparable |
| eager `new_payload` p50 / p95 / max | 3 / 13 / 30ms | 3 / 14 / 25ms | no material regression |
| follower import samples | 470 | 490 | comparable |
| follower import p50 / p95 / max | 4 / 15 / 74ms | 4 / 15 / 48ms | no regression |
| canonical import p50 / p95 / max | 2.612 / 11.578 / 29.013ms | 2.581 / 13.346 / 25.329ms | no material regression |
| tx-bearing canonical blocks | 4 | 4 | same coverage |
| blob-bearing canonical blocks | 3 | 3 | same coverage |

## Scenario 14 Reward and Blob Evidence

Reward phase:

| Field | baseline | candidate | Result |
| --- | --- | --- | --- |
| Staker | `0xb208ebDe1606a7d2E2132565dD2e5d618332B498` | `0xb208ebDe1606a7d2E2132565dD2e5d618332B498` | identical |
| Stake block | 3 | 3 | identical |
| Withdrawal block | 5 | 5 | identical |
| Withdrawal index | 40 | 40 | identical |
| Reward amount | 100000000 gwei | 100000000 gwei | identical |
| Recipient balance after reward | `24414062499999968699963649140625000` | `24414062499999968699963649140625000` | identical |
| Mobile receipts signed | 8 | 8 | identical |

The `reward_address` value is not expected to be byte-identical between the two
runs because scenario 14 generates a fresh random BLS mobile verifier key each
time. The reward proof still hit the same staking/withdrawal path and produced
the same withdrawal amount, index, block, staker, and final balance.

Blob phase:

| Field | baseline | candidate | Result |
| --- | --- | --- | --- |
| Blob tx hash | `0x913b82ff49915db37fc4a1bbc705a8b17a6cabb19a5e04842f1bb6a162355532` | `0x913b82ff49915db37fc4a1bbc705a8b17a6cabb19a5e04842f1bb6a162355532` | identical |
| Inclusion block | 6 | 6 | identical |
| `blobGasUsed` | 131072 | 131072 | identical |
| Leader sidecar broadcast logs | 1 | 1 | identical |
| Follower sidecar processed logs | 2 | 2 | identical |

The blob phase therefore exercises the `BlobStorePort` path with a real EIP-4844
transaction, KZG sidecar propagation, follower sidecar processing, and
cross-node blob-block agreement.

## Guardrails

- This validation commit records only the E2E result.
- No business behavior was changed to make the test pass.
- No `Cargo.lock` drift is included from the validation run.
- `crates/n42-consensus` and consensus wire formats were not changed.
