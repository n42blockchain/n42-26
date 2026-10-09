#!/usr/bin/env bash
# Repeat the parallel-import A-B-A after per-leg load has settled.
# Run only under the shared quiet-hardware box-claim driver.
set -euo pipefail

if [[ "${N42_QUIET_CLAIM:-}" != 1 ]]; then
    echo "Run under the shared quiet-hardware claim" >&2
    exit 2
fi

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PWD/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PWD/.artifacts/native-seven-ab-v2-20260924}"
export N42_BENCH_MAX_TXS_PER_BLOCK=100000

check_external_soak() {
    if ps -eo comm,args | awk '$1 == "rbtcd-soak-star" && /--network bitcoin/ { found = 1 } END { exit !found }'; then
        echo "External Bitcoin public soak is running; quiet-hardware measurement is invalid" >&2
        exit 2
    fi
}

wait_quiet() {
    local samples=0
    local load1
    while (( samples < 2 )); do
        check_external_soak
        read -r load1 _ < /proc/loadavg
        if awk -v measured_load="$load1" 'BEGIN { exit !(measured_load < 8) }'; then
            samples=$((samples + 1))
        else
            samples=0
        fi
        printf '%s load1=%s quiet_samples=%s\n' "$(date -Is)" "$load1" "$samples"
        if (( samples < 2 )); then sleep 10; fi
    done
}

check_external_soak
N42_BENCH_DURATION_SECS=10 bash scripts/run-native-seven-leg.sh warmup14 > .artifacts/native-seven-warmup14.log 2>&1

wait_quiet
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2d-import-a1 > .artifacts/native-seven-r2d-import-a1.log 2>&1
wait_quiet
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2d-import-b --parallel-import > .artifacts/native-seven-r2d-import-b.log 2>&1
wait_quiet
N42_BENCH_DURATION_SECS=60 bash scripts/run-native-seven-leg.sh r2d-import-a2 > .artifacts/native-seven-r2d-import-a2.log 2>&1
