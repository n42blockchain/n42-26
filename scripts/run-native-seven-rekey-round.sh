#!/usr/bin/env bash
# Same-binary A/B/A comparison of Gov5 builder execution reuse.
# Invoke only through the shared quiet-hardware claim driver.
set -euo pipefail

if [[ "${N42_QUIET_CLAIM:-}" != 1 ]]; then
    echo "Run under the shared quiet-hardware claim" >&2
    exit 2
fi

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PWD/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PWD/.artifacts/native-seven-rekey-20260925}"
export N42_BENCH_MAX_TXS_PER_BLOCK=100000

N42_GOV5_REUSE_BUILDER_EXECUTION=1 N42_BENCH_DURATION_SECS="${N42_REKEY_WARMUP_SECS:-10}" \
    bash scripts/run-native-seven-leg.sh rekey-warmup --parallel-build
N42_GOV5_REUSE_BUILDER_EXECUTION=0 N42_BENCH_DURATION_SECS=60 \
    bash scripts/run-native-seven-leg.sh rekey-a1 --parallel-build
N42_GOV5_REUSE_BUILDER_EXECUTION=1 N42_BENCH_DURATION_SECS=60 \
    bash scripts/run-native-seven-leg.sh rekey-b --parallel-build
N42_GOV5_REUSE_BUILDER_EXECUTION=0 N42_BENCH_DURATION_SECS=60 \
    bash scripts/run-native-seven-leg.sh rekey-a2 --parallel-build
