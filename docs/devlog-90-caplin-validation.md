# Devlog 90 - Caplin EL-Seam Validation

Date: 2026-06-21

Branch under test: `feat/caplin-cl-stage3-6` at `2b0d65a`.

Reth baseline: `../reth` at `449ecfdcef n42: disable jit by default (Windows, no LLVM 22) + post-merge fixes`.

Artifacts:

- Main E2E log: `/tmp/n42-caplin-validation-artifacts/e2e-main.log`
- Candidate E2E log: `/tmp/n42-caplin-validation-artifacts/e2e-candidate.log`
- Async-FCU OFF run: `/tmp/n42-caplin-validation-async-off`
- Async-FCU ON run: `/tmp/n42-caplin-validation-async-on`
- Standalone dummy smoke: `/tmp/n42-caplin-validation-artifacts/standalone-dummy-smoke.log`

## Verdict

Part-1 behavior gate verdict: **MERGE**.

The candidate passes the same E2E scenarios as `main` (`1,3,4`), including
scenario 4's `1/3/5/7/21` validator profiles, with no consensus stall, fork,
or E2E-visible panic regression. Keep `N42_ASYNC_FINALIZE_FCU` opt-in; do not
flip the default from this run.

## Part 1 - Candidate vs Main E2E Gate

Builds completed from the same checkout and the same reth baseline:

```text
cargo build --release -p n42-node-bin -p e2e-test
```

Both builds emitted only the existing reth warnings seen on the baseline.

Command:

```text
E2E_SCENARIO_FILTER=1,3,4 <e2e-test> --binary <n42-node>
```

Summary:

| Check | main | candidate | Result |
| --- | ---: | ---: | --- |
| Scenario 1 suite | PASS | PASS | no regression |
| Scenario 1 blocks | 99 | 101 | within expected range |
| Scenario 1 avg interval | 4.01s | 3.97s | no regression |
| Scenario 1 balances | PASS | PASS | no regression |
| Scenario 3 ERC-20 transfer receipts | 300/300 | 300/300 | no regression |
| Scenario 3 deployer balance | PASS | PASS | no regression |
| Scenario 3 total supply | PASS | PASS | no regression |
| Scenario 4 1-node | PASS, h=12, avg=8.0s | PASS, h=12, avg=8.0s | no regression |
| Scenario 4 3-node | PASS, h=13/13, avg=7.6s | PASS, h=13/13, avg=7.7s | no regression |
| Scenario 4 5-node | PASS, h=13/13, avg=7.6s | PASS, h=13/13, avg=7.7s | no regression |
| Scenario 4 7-node | PASS, h=13/13, avg=8.0s | PASS, h=13/13, avg=7.4s | no regression |
| Scenario 4 21-node | PASS, h=12/12, avg=8.0s | PASS, h=12/12, avg=8.0s | no regression |
| E2E suite total | passed=3 failed=0 | passed=3 failed=0 | no regression |

Node-log comparison for the multi-node E2E logs:

| Metric | main | candidate | Result |
| --- | ---: | ---: | --- |
| Panic / span-panic lines | 0 | 0 | no regression |
| Error lines | 0 | 0 | no regression |
| `N42_CADENCE` samples | 493 | 429 | comparable run length |
| `inter_block_commit_ms` p50 / p95 / max | 7992 / 8036 / 8061 | 7998 / 8071 / 8272 | no material regression |
| `N42_FCU` lines | 530 | 466 | comparable run length |
| `n42_fcu_latency_ms` log p50 / p95 / max | 0 / 2 / 8ms | 0 / 2 / 12ms | no material regression |
| follower eager import accepted lines | 416 | 402 | comparable |
| follower import p50 / p95 / max | 5 / 20 / 152ms | 6 / 14 / 98ms | no regression |

State-root / chain consistency:

- Scenario 4 sampled block hashes were identical across all validators at every
  tested size (`1/3/5/7/21`) for both main and candidate.
- The block hashes are not expected to equal between separate main/candidate
  wall-clock runs because timestamps differ, but cross-node equality within
  each run proves a single canonical state/receipt/header result per workload.

Coverage notes:

- Scenario `1/3/4` did not include EIP-4844 blob transactions, so blob sidecar
  propagation is not exercised by this gate.
- Scenario `1/3/4` did not cross a mobile-reward/staking-withdrawal epoch with
  assertions. The `WithdrawalSource` port was therefore not independently
  proved by this E2E subset. Existing unit/check coverage still compiles the
  port extraction, but a reward-specific E2E remains the right follow-up if the
  merge gate is expanded.

## Part 2 - `N42_ASYNC_FINALIZE_FCU` A/B

Shared setup:

```text
export N42_INJECT_PORT=19900
export N42_TWIG=1 N42_FAST_PROPOSE=1 N42_MIN_PROPOSE_DELAY_MS=0 N42_DEFER_STATE_ROOT=1
export N42_SKIP_TX_VERIFY=1 N42_MAX_TXS_PER_BLOCK=90000 N42_INJECT_HIGH_WATER=90000
export N42_INGEST_TARGET_PENDING=90000 N42_POOL_MAX_TXS=300000 N42_GAS_LIMIT=2000000000
export N42_DISABLE_TX_FORWARD=1 N42_INGEST_EXTENDED_ACK=1
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen \
  --no-monitor --no-mobile-sim --block-interval 2000 --data-dir <base>/data
```

Before each stress run, all ingest ports `19900..19906` were checked with `nc -z`
and all seven were reachable.

Stress command:

```text
target/release/n42-stress \
  --rpc http://127.0.0.1:18000,...,http://127.0.0.1:18006 \
  --ingest 127.0.0.1:19900,...,127.0.0.1:19906 \
  --sync-ingest-mode per-node-continuous \
  --presign-load /tmp/n42-devlog79-5m-7rpc.bin \
  --wave 90000 --batch-size 500 --target-tps 0 --erc20-ratio 0 --duration 90
```

Results, filtered to the stress window:

| Metric | OFF (`N42_ASYNC_FINALIZE_FCU=0`) | ON (`N42_ASYNC_FINALIZE_FCU=1`) |
| --- | ---: | ---: |
| Stress duration | 90.5s | 90.4s |
| TCP injected / errors | 1,268,744 / 0 | 1,838,452 / 0 |
| Stress injection TPS | 14,012 | 20,346 |
| Block-analysis blocks | 10 | 11 |
| Block-analysis total tx | 660,036 | 588,244 |
| Active-block overall TPS | 73,337 | 58,824 |
| Max tx/block | 90,000 | 82,688 |
| `inter_block_commit_ms` samples | 70 | 133 |
| `inter_block_commit_ms` p50 / p95 / max | 3214 / 61723 / 62515ms | 3262 / 11667 / 12731ms |
| FCU samples | 70 inline | 133 async |
| FCU latency p50 / p95 / max | 7 / 201 / 795ms | 0 / 41 / 892ms |
| FCU status Valid / Syncing | 45 / 25 | 62 / 71 |
| FCU retry samples | 0 | 0 |
| Build start -> broadcast p50 / p95 / max | 1611 / 2362 / 2362ms | 1900 / 2761 / 2761ms |
| Follower import p50 / p95 / max | 98 / 870 / 1492ms | 102 / 537 / 1141ms |
| Pool pending p50 / p95 / max | 90k / 90k / 90k | 90k / 90k / 90k |
| `finalize_done_pending` observed | no diagnostic lines | no diagnostic lines |
| finalize channel full/drop lines | 0 | 0 |
| Error lines | 0 | 0 |
| Non-fatal span-panic lines | 70 | 38 |

Interpretation:

- ON moved finalize FCU off the consensus loop and lowered the observed
  `inter_block_commit_ms` tail in this run.
- ON was not a clear throughput win: active-block TPS and max tx/block were lower
  than OFF, and ON had a higher Syncing share in FCU results.
- Both runs were polluted by the known tracing-subscriber span panic under high
  load (`deferred-trie` / `payload-convert`, `sharded.rs:306`). The node kept
  running and stress completed, but this prevents treating the A/B as a clean
  default-flip proof.

Recommendation: keep `N42_ASYNC_FINALIZE_FCU` default off. Leave it opt-in until
the span propagation issue and EL head-lag / Syncing behavior are fixed and the
same A/B shows a throughput win without finalization ambiguity.

## Part 3 - Standalone Consensus / Engine API

Build:

```text
cargo build --release -p n42-consensus-standalone
```

Result: PASS.

CLI smoke:

```text
target/release/n42-consensus-standalone --help
```

Result: PASS; the binary exposes `--engine-url` and `--jwt-secret` with env
fallbacks.

Dummy endpoint smoke:

```text
printf '0000000000000000000000000000000000000000000000000000000000000000\n' \
  > /tmp/n42-caplin-standalone.jwt
target/release/n42-consensus-standalone \
  --engine-url http://127.0.0.1:65535 \
  --jwt-secret /tmp/n42-caplin-standalone.jwt
```

Result: EXIT=0 with:

```text
standalone consensus: remote Engine-API ExecutionLayer ready
standalone consensus skeleton: ConsensusService constructed against the remote EL;
full swarm bring-up + service.run() is the post-datc cross-process E2E step
```

Status: **blocked / not a real Engine API E2E yet**.

The binary currently constructs `EngineApiRpcExecutionLayer` and
`ConsensusService`, then drops the service. It does not bring up a libp2p swarm,
load a real validator set/chainspec, call `service.run().await`, or invoke
`engine_newPayloadV4`, `engine_forkchoiceUpdatedV3`, or `engine_getPayloadV4`.
The dummy smoke succeeds against a closed port, which proves there is no live
Engine API I/O in the current skeleton.

Minimal next work for a real cross-process E2E:

1. Load BLS key, validator set, genesis/chainspec, head hash, and fee recipient
   from config instead of dev single-validator constants.
2. Start the libp2p `NetworkService`, wire its command receivers and event
   senders to `ConsensusService`.
3. Call `service.run().await` and drive a live EL at `--engine-url` with JWT auth.
4. Add an assertion harness around observed `engine_newPayloadV4`,
   `engine_forkchoiceUpdatedV3`, and `engine_getPayloadV4` calls.
5. Fill `blob_tx_hashes` in standalone `resolve_payload` if blob rebroadcast is
   required for standalone mode.

## Guardrail Check

- No behavior code was changed for this validation.
- `git status` was clean before writing this devlog.
- No Cargo.lock drift was introduced.
- No dependency downgrade was performed.
- No commit trailer / AI co-author trailer is used.
