#!/usr/bin/env bash
# One continuous seven-node, nonce-continuous 60s warmup + 60s scored window.
# Run as the foreground child of the shared box-claim controller.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Requires the shared box claim on both directories" >&2
    exit 2
fi

TAG="cap220-warmed"
export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PROJECT_DIR/.artifacts/native-seven-220k-warmed-20260927}"
export N42_SEVEN_PRESIGNED_TXS="${N42_SEVEN_PRESIGNED_TXS:-$PROJECT_DIR/.artifacts/native-seven-presigned-48m-20260927.bin}"
export N42_BENCH_MAX_TXS_PER_BLOCK=220000
export N42_BENCH_RPC_MAX_RESPONSE_MB=512
export N42_GOV5_REUSE_BUILDER_EXECUTION=1
export N42_SEVEN_SCORE_MODE=1

RESULT="$N42_SEVEN_CAMPAIGN_DIR/result-$TAG"
QUALIFICATION="$RESULT/qualification"
if [[ -e "$RESULT" || -e "$N42_SEVEN_CAMPAIGN_DIR/runtime-$TAG" ]]; then
    echo "Refusing to overwrite warmed leg: $TAG" >&2
    exit 2
fi

# The sender runs 30s past the scored boundary, so boundary capture and
# post-score bookkeeping cannot shorten the measured 60 seconds. The 48M
# file is large enough even for a theoretical 220k/s across all 150 seconds.
python3 "$SCRIPT_DIR/presign-workload-preflight.py" \
    --file "$N42_SEVEN_PRESIGNED_TXS" \
    --trusted-config "$N42_SEVEN_ARTIFACT_DIR/trusted-config.json" \
    --chain-id 941007 --duration 150 --minimum-tps 220000 \
    --out "$PROJECT_DIR/.artifacts/native-seven-220k-warmed-preflight-20260927.json"

watch_boundaries() {
    local attempt
    for attempt in $(seq 1 6000); do
        if [[ -s "$QUALIFICATION/ingest-start.ns" ]]; then
            break
        fi
        sleep 0.1
    done
    if [[ ! -s "$QUALIFICATION/ingest-start.ns" ]]; then
        echo "Timed out waiting for the continuous ingest start" >&2
        return 1
    fi
    sleep 60
    python3 "$SCRIPT_DIR/h2-tps-audit.py" capture \
        --trusted-config "$QUALIFICATION/trusted-config.json" \
        --rpc "$(python3 -c 'print(",".join(f"http://127.0.0.1:{23400+i}" for i in range(7)))')" \
        --out "$QUALIFICATION/h2-score-start.json"
    python3 - "$QUALIFICATION" <<'PY'
import json
import sys
from pathlib import Path
directory = Path(sys.argv[1])
boundary = json.loads((directory / "h2-score-start.json").read_text())
(directory / "score-start.ns").write_text(str(boundary["started_ns"]) + "\n")
PY
    sleep 60
    python3 "$SCRIPT_DIR/h2-tps-audit.py" capture \
        --trusted-config "$QUALIFICATION/trusted-config.json" \
        --rpc "$(python3 -c 'print(",".join(f"http://127.0.0.1:{23400+i}" for i in range(7)))')" \
        --out "$QUALIFICATION/h2-score-end.json"
}

watch_boundaries > "$PROJECT_DIR/.artifacts/native-seven-220k-warmed-boundary-20260927.log" 2>&1 &
watcher="$!"
cleanup() {
    local code=$?
    trap - EXIT INT TERM
    if kill -0 "$watcher" 2>/dev/null; then
        kill "$watcher" 2>/dev/null || true
        wait "$watcher" 2>/dev/null || true
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

N42_BENCH_DURATION_SECS=150 bash "$SCRIPT_DIR/run-native-seven-leg.sh" "$TAG" --parallel-build
wait "$watcher"
trap - EXIT INT TERM
