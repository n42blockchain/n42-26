# Devlog 72: Async finalize-FCU behind flag

Date: 2026-06-15
Base: `main` at `9de4eab` (`Merge pull request #3 from n42blockchain/feat/consensus-fcu-async`)
Branch: `feat/consensus-async-finalize-fcu`
Reth baseline: unchanged, `chore/merge-upstream-fc2cc1e` at `449ecfdce`

## Summary

Stage 8 moves the finalize `fork_choice_updated` call off the consensus select-loop hot path behind `N42_ASYNC_FINALIZE_FCU=1`. The default path keeps inline finalize-FCU behavior and the existing eager-import rescue path.

Implementation:

- Added `N42_ASYNC_FINALIZE_FCU` parsing to `ConsensusOrchestrator`.
- Added `finalize_done_tx/rx` channel with capacity 256.
- Extracted post-FCU Case A/B handling into `handle_finalize_done`.
- Default path awaits FCU inline, keeps the rescue retry, then calls `handle_finalize_done`.
- Flag-on path spawns finalize-FCU, sends `(view, block_hash, commit_qc, finalized)` back on the new channel, and lets the run-loop call `handle_finalize_done`.
- Added a biased-select branch after engine outputs and before network/data events for finalize completions.

## Validation

Build and unit checks:

```bash
cargo check -p n42-node
cargo test -p n42-node orchestrator -- --nocapture
cargo build --release -p n42-node-bin -p e2e-test -p n42-stress -p n42-mobile-sim
git diff --check
```

Results:

- `cargo check -p n42-node`: passed.
- `cargo test -p n42-node orchestrator -- --nocapture`: passed, 31 orchestrator tests.
- Release build: passed.
- `git diff --check`: passed.

Flag-off E2E regression:

```bash
RUST_LOG=info \
E2E_SCENARIO_FILTER=1,3,4 \
E2E_SCENARIO4_PROFILE=correctness \
target/release/e2e-test --binary target/release/n42-node
```

Results:

- Scenario 1 passed: 100 blocks, average interval 4.01s.
- Scenario 3 passed: 300/300 ERC-20 transfers, balances and total supply verified.
- Scenario 4 passed:
  - 1 node: height 12, average interval 8.0s.
  - 3 nodes: all height 13, average interval 7.6s.
  - 5 nodes: all height 16, average interval 7.7s.

Flag-on E2E:

```bash
RUST_LOG=info \
N42_ASYNC_FINALIZE_FCU=1 \
E2E_SCENARIO_FILTER=4 \
E2E_SCENARIO4_PROFILE=correctness \
target/release/e2e-test --binary target/release/n42-node
```

Results:

- Scenario 4 passed:
  - 1 node: height 13, average interval 8.0s.
  - 3 nodes: all height 13, average interval 7.8s.
  - 5 nodes: all height 15, average interval 7.8s.

## Contract-heavy 4-node comparison

Both runs used a clean 4-validator local testnet, `N42_TWIG=1`, block interval 4000ms, and the same stress parameters:

```bash
target/release/n42-stress \
  --rpc http://127.0.0.1:18000,http://127.0.0.1:18001,http://127.0.0.1:18002,http://127.0.0.1:18003 \
  --erc20-ratio 100 \
  --target-tps 200 \
  --duration 30 \
  --accounts 1000 \
  --batch-size 100 \
  --accounts-per-batch 100 \
  --concurrency 128
```

| Mode | Sent | RPC/HTTP errors | Total tx in analyzed blocks | Overall TPS | p50 TPS | p95 TPS | Max tx/block | Final height |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| flag-off | 14000 | 0/0 | 13600 | 425.0 | 500.0 | 700.0 | 2800 | `0x14` on all 4 nodes |
| flag-on | 14000 | 0/0 | 12800 | 400.0 | 400.0 | 700.0 | 2800 | `0x1b` on all 4 nodes |

Validator-0 commit-stage samples from `view committed` logs:

| Mode | Samples | p50 commit | p95 commit | Max commit |
| --- | ---: | ---: | ---: | ---: |
| flag-off | 22 | 61ms | 86ms | 97ms |
| flag-on | 54 | 71ms | 95ms | 119ms |

FCU latency in this local run was too small to show the production stall described in devlog 69:

| Mode | Samples | p50 FCU | p95 FCU | Max FCU |
| --- | ---: | ---: | ---: | ---: |
| flag-off | 22 | 0ms | 0ms | 1ms |
| flag-on | 54 | 0ms | 0ms | 2ms |

Flag-on evidence:

- `N42_FCU: async finalize fcu` appeared 96 times across the 4 validator logs.
- No `fork_choice_updated failed`, async completion receiver drop, panic, or ERROR was observed.
- Tx-bearing blocks were committed and Twig storage changed under contract-heavy load:
  - flag-on validator-0 `storage_changes`: 1000, 1500, 2000 across tx-bearing blocks.
  - flag-on storage sample: version 10 had `accounts=1002 storage_changes=2000`; version 14 had `accounts=1002 storage_changes=2000`; version 18 had `accounts=1002 storage_changes=2000`.

## Conclusion

Correctness passed for both paths. The flag-on path exercised the async completion channel under 4-node contract-heavy load and maintained consistent chain height across all validators with zero RPC/HTTP errors from stress.

This local run did not reproduce slow FCU latency; FCU p95 stayed at 0ms for both modes. Recommendation: keep `N42_ASYNC_FINALIZE_FCU` default off for now, use it as an experimental flag for longer 7-node/high-load profiling where FCU latency is actually non-trivial, and revisit default-on only after that profile shows select-loop improvement without Case A hit-rate regression.
