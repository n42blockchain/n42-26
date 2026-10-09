#!/usr/bin/env bash
# Same-binary warmed A/B/A for the measured follower-import bottleneck.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Requires the shared box claim on both directories" >&2
    exit 2
fi

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PROJECT_DIR/.artifacts/native-seven-220k-import-ab-20261001}"
export N42_SEVEN_PRESIGNED_TXS="${N42_SEVEN_PRESIGNED_TXS:-$PROJECT_DIR/.artifacts/native-seven-presigned-48m-20260927.bin}"
export N42_BENCH_MAX_TXS_PER_BLOCK=220000
export N42_BENCH_RPC_MAX_RESPONSE_MB=512
export N42_GOV5_REUSE_BUILDER_EXECUTION=1

if [[ -e "$N42_SEVEN_CAMPAIGN_DIR" ]]; then
    echo "Refusing to overwrite A/B/A campaign: $N42_SEVEN_CAMPAIGN_DIR" >&2
    exit 2
fi

N42_SEVEN_LEG_TAG=import-a1 N42_SEVEN_LEG_OPTIONS="--parallel-build" \
    bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"
N42_SEVEN_LEG_TAG=import-b N42_SEVEN_LEG_OPTIONS="--parallel-build --parallel-import" \
    bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"
N42_SEVEN_LEG_TAG=import-a2 N42_SEVEN_LEG_OPTIONS="--parallel-build" \
    bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"

python3 "$SCRIPT_DIR/summarize_native_seven_import_ab.py" "$N42_SEVEN_CAMPAIGN_DIR"
