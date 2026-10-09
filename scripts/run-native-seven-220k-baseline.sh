#!/usr/bin/env bash
# Full-validation 220k-cap native seven-node baseline. Invoke only as the
# foreground child of the shared quiet-hardware box-claim controller.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Requires the shared box claim on both directories" >&2
    exit 2
fi

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PROJECT_DIR/.artifacts/native-seven-220k-20260926}"
export N42_BENCH_MAX_TXS_PER_BLOCK=220000
export N42_BENCH_RPC_MAX_RESPONSE_MB=512
export N42_GOV5_REUSE_BUILDER_EXECUTION=1

N42_BENCH_DURATION_SECS=60 bash "$SCRIPT_DIR/run-native-seven-leg.sh" cap220-warmup --parallel-build
N42_BENCH_DURATION_SECS=60 bash "$SCRIPT_DIR/run-native-seven-leg.sh" cap220-baseline --parallel-build
