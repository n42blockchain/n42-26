#!/usr/bin/env bash
# Round 2: independent A-B-A comparisons for parallel build and import.
# Run only as a child of the shared quiet-hardware box-claim driver.
set -euo pipefail

if [[ "${N42_QUIET_CLAIM:-}" != 1 ]]; then
    echo "Run under the shared quiet-hardware claim" >&2
    exit 2
fi

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PWD/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PWD/.artifacts/native-seven-ab-v2-20260924}"
export N42_BENCH_MAX_TXS_PER_BLOCK=100000

N42_BENCH_DURATION_SECS=10 bash scripts/run-native-seven-leg.sh warmup11 > .artifacts/native-seven-warmup11.log 2>&1

N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-build-a1 > .artifacts/native-seven-r2c-build-a1.log 2>&1
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-build-b --parallel-build > .artifacts/native-seven-r2c-build-b.log 2>&1
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-build-a2 > .artifacts/native-seven-r2c-build-a2.log 2>&1

N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-import-a1 > .artifacts/native-seven-r2c-import-a1.log 2>&1
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-import-b --parallel-import > .artifacts/native-seven-r2c-import-b.log 2>&1
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2c-import-a2 > .artifacts/native-seven-r2c-import-a2.log 2>&1
