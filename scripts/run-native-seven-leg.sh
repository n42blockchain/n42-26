#!/usr/bin/env bash
# One isolated seven-validator H2 performance leg. Run as a foreground child
# of the shared box-claim driver; the driver owns the hardware claim.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ $# -lt 1 ]]; then
    echo "Usage: $0 TAG [--fast-transfers] [--parallel-build] [--parallel-import] [--import-single-flight]" >&2
    exit 2
fi
TAG="$1"
shift
BLOCK_CAP="${N42_BENCH_MAX_TXS_PER_BLOCK:-220000}"
if [[ ! "$BLOCK_CAP" =~ ^[1-9][0-9]*$ ]]; then
    echo "N42_BENCH_MAX_TXS_PER_BLOCK must be a positive integer" >&2
    exit 2
fi
if [[ ! "$TAG" =~ ^[a-zA-Z0-9_-]+$ ]]; then
    echo "TAG must contain only letters, digits, underscores or dashes" >&2
    exit 2
fi
for option in "$@"; do
    case "$option" in
        --fast-transfers|--parallel-build|--parallel-import|--import-single-flight) ;;
        *) echo "Unsupported fleet option: $option" >&2; exit 2 ;;
    esac
done
if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Run under the shared box-claim driver with N42_QUIET_CLAIM=1" >&2
    exit 2
fi

TEMPLATE="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924}"
CAMPAIGN="${N42_SEVEN_CAMPAIGN_DIR:-$PROJECT_DIR/.artifacts/native-seven-ab-20260924}"
PRESIGNED="${N42_SEVEN_PRESIGNED_TXS:-$TEMPLATE/presigned-24m.bin}"
RUNTIME="$CAMPAIGN/runtime-$TAG"
RESULT="$CAMPAIGN/result-$TAG"
if [[ -e "$RUNTIME" || -e "$RESULT" ]]; then
    echo "Refusing to overwrite existing leg: $TAG" >&2
    exit 2
fi
for file in manifest.json consensus.json trusted-config.json test-accounts.json recipients.json; do
    if [[ ! -f "$TEMPLATE/$file" ]]; then
        echo "Missing template file: $file" >&2
        exit 2
    fi
done
NODE_BINARY="${N42_SEVEN_NODE_BINARY:-$PROJECT_DIR/target/release/n42-node}"
STRESS_BINARY="${N42_SEVEN_STRESS_BINARY:-$PROJECT_DIR/target/release/n42-stress}"
KECCAK_BINARY="${N42_SEVEN_KECCAK_BINARY:-$PROJECT_DIR/target/release/n42-keccak}"
COMMIT_VERIFY_BINARY="${N42_SEVEN_COMMIT_VERIFY_BINARY:-$PROJECT_DIR/target/release/n42-verify-commit}"
for file in "$NODE_BINARY" "$STRESS_BINARY" "$KECCAK_BINARY" "$COMMIT_VERIFY_BINARY" "$PRESIGNED"; do
    if [[ ! -f "$file" ]]; then
        echo "Missing binary or workload: $file" >&2
        exit 2
    fi
done

mkdir -p "$CAMPAIGN" "$RUNTIME"
cp -a --reflink=auto "$TEMPLATE/artifacts" "$RUNTIME/artifacts"
cp -a --reflink=auto "$TEMPLATE"/node[0-6] "$RUNTIME/"
for file in manifest.json consensus.json trusted-config.json test-accounts.json recipients.json; do
    cp -a --reflink=auto "$TEMPLATE/$file" "$RUNTIME/$file"
done
mkdir -p "$RUNTIME/logs" "$RUNTIME/pids" "$RESULT"
printf 'tag\t%s\noptions\t%s\nmax_txs_per_block\t%s\n' "$TAG" "$*" "$BLOCK_CAP" > "$RESULT/leg.tsv"
printf 'gov5_reuse_builder_execution\t%s\n' "${N42_GOV5_REUSE_BUILDER_EXECUTION:-1}" >> "$RESULT/leg.tsv"
printf 'reth_persistence_threshold\t0\nreth_state_masking_blocks\t0\nreth_memory_block_buffer_target\t0\n' >> "$RESULT/leg.tsv"
printf 'rpc_max_response_mb\t%s\n' "${N42_BENCH_RPC_MAX_RESPONSE_MB:-160}" >> "$RESULT/leg.tsv"
if [[ -n "${N42_RETH_SOURCE_MANIFEST:-}" ]]; then
    cp "$N42_RETH_SOURCE_MANIFEST" "$RESULT/source-manifest.json"
fi
sha256sum "$NODE_BINARY" "$STRESS_BINARY" \
    "$KECCAK_BINARY" "$COMMIT_VERIFY_BINARY" \
    "$PRESIGNED" > "$RESULT/binary-workload-sha256.txt"
sha256sum "$TEMPLATE/manifest.json" "$TEMPLATE/consensus.json" \
    "$TEMPLATE/trusted-config.json" "$TEMPLATE/test-accounts.json" \
    "$TEMPLATE/recipients.json" > "$RESULT/run-config-sha256.txt"
date -Is > "$RESULT/started-at.txt"
cp /proc/buddyinfo "$RESULT/buddy-before.txt"
cp /proc/meminfo "$RESULT/meminfo-before.txt"
cat /proc/loadavg > "$RESULT/load-before.txt"

supervisor=""
cleanup() {
    local code=$?
    trap - EXIT INT TERM
    if [[ -n "$supervisor" ]] && kill -0 "$supervisor" 2>/dev/null; then
        kill -INT "$supervisor" 2>/dev/null || true
        wait "$supervisor" 2>/dev/null || true
    fi
    python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$RUNTIME" stop > "$RESULT/stop.log" 2>&1 || true
    cp /proc/buddyinfo "$RESULT/buddy-after.txt" || true
    cp /proc/meminfo "$RESULT/meminfo-after.txt" || true
    cat /proc/loadavg > "$RESULT/load-after.txt" || true
    date -Is > "$RESULT/ended-at.txt" || true
    exit "$code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$RUNTIME" start \
    --foreground --binary "$NODE_BINARY" \
    --disable-tx-forward --qmdb-reads only --max-txs-per-block "$BLOCK_CAP" \
    --rpc-max-response-mb "${N42_BENCH_RPC_MAX_RESPONSE_MB:-160}" "$@" \
    > "$RESULT/supervisor.log" 2>&1 &
supervisor="$!"

ready=0
for _ in $(seq 1 180); do
    if ! kill -0 "$supervisor" 2>/dev/null; then
        echo "Fleet supervisor exited before RPC readiness" >&2
        exit 1
    fi
    if curl -fsS --max-time 1 -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        http://127.0.0.1:23400/ > "$RESULT/rpc-ready.json" 2>/dev/null; then
        ready=1
        break
    fi
    sleep 1
done
if [[ "$ready" != 1 ]]; then
    echo "Fleet RPC did not become ready within 180 seconds" >&2
    exit 1
fi

# RPC can answer eth_blockNumber before all seven validators publish their
# first CommitQC. The rotation check below must start from a committed fleet.
committed_ready=0
for _ in $(seq 1 60); do
    if ! kill -0 "$supervisor" 2>/dev/null; then
        echo "Fleet supervisor exited before seven-node CommitQC readiness" >&2
        exit 1
    fi
    if python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$RUNTIME" status \
        > "$RESULT/status-ready.json" 2> "$RESULT/status-wait.log"; then
        committed_ready=1
        break
    fi
    sleep 1
done
if [[ "$committed_ready" != 1 ]]; then
    echo "Seven-node fleet did not reach CommitQC readiness within 60 seconds" >&2
    exit 1
fi

python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$RUNTIME" verify \
    --seconds 30 --min-blocks 2 > "$RESULT/verify-before.log" 2>&1
python3 - "$RUNTIME" <<'PY'
import json
import sys
from pathlib import Path
runtime = Path(sys.argv[1])
pids = [str(json.loads((runtime / f"pids/node{i}.json").read_text())["pid"]) for i in range(7)]
(runtime / "node-pids.txt").write_text(",".join(pids) + "\n")
PY

N42_BENCH_NODES=7 \
N42_CHAIN_ID=941007 \
N42_BENCH_DURATION_SECS="${N42_BENCH_DURATION_SECS:-60}" \
N42_BENCH_TARGET_TPS="${N42_BENCH_TARGET_TPS:-400000}" \
N42_BENCH_MIN_COMMITTED_TPS=1 \
N42_BENCH_WAVE_TXS=220000 \
N42_MAX_TXS_PER_BLOCK="$BLOCK_CAP" \
N42_BENCH_RPC_BASE=23400 \
N42_BENCH_INGEST_BASE=34400 \
N42_BENCH_METRICS_BASE=23600 \
N42_BENCH_TRUSTED_CONFIG="$RUNTIME/trusted-config.json" \
N42_PRESIGNED_TXS="$PRESIGNED" \
N42_BENCH_DATA_DIR="$RUNTIME" \
N42_BENCH_ARTIFACT_DIR="$RESULT/qualification" \
N42_BENCH_VARIANT="$TAG" \
N42_STRESS_BIN="$STRESS_BINARY" \
N42_KECCAK_BIN="$KECCAK_BINARY" \
N42_COMMIT_VERIFY_BIN="$COMMIT_VERIFY_BINARY" \
bash "$SCRIPT_DIR/qualify-1m-tps.sh" > "$RESULT/qualification.log" 2>&1

if [[ "${N42_SEVEN_SCORE_MODE:-0}" == 1 ]]; then
    for file in h2-score-start.json h2-score-end.json score-start.ns; do
        if [[ ! -s "$RESULT/qualification/$file" ]]; then
            echo "Missing warmed score boundary: $file" >&2
            exit 1
        fi
    done
    python3 "$SCRIPT_DIR/h2-tps-audit.py" audit \
        --trusted-config "$RESULT/qualification/trusted-config.json" \
        --start "$RESULT/qualification/h2-score-start.json" \
        --end "$RESULT/qualification/h2-score-end.json" \
        --out "$RESULT/qualification/h2-score-audit.json" --min-tps 1 \
        > "$RESULT/score-audit.log" 2>&1
fi

python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$RUNTIME" verify \
    --seconds 20 --min-blocks 1 --no-require-rotation > "$RESULT/verify-after.log" 2>&1
python3 "$SCRIPT_DIR/native_seven_timeline.py" "$CAMPAIGN" "$TAG" \
    > "$RESULT/timeline.log" 2>&1
if [[ "${N42_SEVEN_SCORE_MODE:-0}" == 1 ]]; then
    python3 "$SCRIPT_DIR/native_seven_timeline.py" "$CAMPAIGN" "$TAG" --score \
        > "$RESULT/timeline-score.log" 2>&1
fi
cat "$RESULT/qualification/summary.tsv"
