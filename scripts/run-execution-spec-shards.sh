#!/usr/bin/env bash
set -euo pipefail

# Parallel runner for execution-spec/Hive shards.
#
# Prerequisites:
# - `hive` binary is installed and runnable
# - ethereum/hive repository is checked out locally
# - client image is already prepared and addressable as `reth`
#
# Typical usage:
#   HIVE_TESTS_DIR="$HOME/src/hive" scripts/run-execution-spec-shards.sh
#   SHARDS="paris+shanghai,cancun" scripts/run-execution-spec-shards.sh
#
# This script intentionally targets the upstream `hive --client reth` contract
# so it can reuse the same simulator expectations and fork filters as the
# upstream execution-spec lane.

HIVE_BIN="${HIVE_BIN:-hive}"
HIVE_TESTS_DIR="${HIVE_TESTS_DIR:-$PWD/hivetests}"
HIVE_CLIENT="${HIVE_CLIENT:-reth}"
HIVE_PARALLELISM="${HIVE_PARALLELISM:-4}"
LOG_DIR="${LOG_DIR:-$PWD/.artifacts/execution-spec-shards-$(date +%Y%m%d-%H%M%S)}"
SHARDS="${SHARDS:-paris+shanghai,cancun,prague,osaka,rlp}"

mkdir -p "$LOG_DIR"

if ! command -v "$HIVE_BIN" >/dev/null 2>&1; then
    echo "error: hive binary not found: $HIVE_BIN" >&2
    exit 1
fi

if [[ ! -d "$HIVE_TESTS_DIR" ]]; then
    echo "error: HIVE_TESTS_DIR does not exist: $HIVE_TESTS_DIR" >&2
    exit 1
fi

run_shard() {
    local shard_name=$1
    local sim=$2
    local limit=$3
    local approx_tests=$4
    local target_minutes=$5
    local log_file="$LOG_DIR/${shard_name}.log"

    echo "=== shard=${shard_name} sim=${sim} sim_limit=${limit} approx_tests=${approx_tests} target_minutes=${target_minutes} ==="
    (
        cd "$HIVE_TESTS_DIR"
        "$HIVE_BIN" \
            --sim "$sim" \
            --sim.limit "$limit" \
            --sim.parallelism "$HIVE_PARALLELISM" \
            --client "$HIVE_CLIENT" \
            2>&1 | tee "$log_file"
    )
}

start_named_shard() {
    local shard_name=$1
    case "$shard_name" in
        paris+shanghai)
            run_shard "$shard_name" "ethereum/eels/consume-engine" '.*/.*fork_(Paris|Shanghai)' "~2600" "12" &
            ;;
        cancun)
            run_shard "$shard_name" "ethereum/eels/consume-engine" '.*/.*fork_Cancun' "~17250" "59" &
            ;;
        prague)
            run_shard "$shard_name" "ethereum/eels/consume-engine" '.*/.*fork_Prague' "~20500" "71" &
            ;;
        osaka)
            run_shard "$shard_name" "ethereum/eels/consume-engine" '.*/.*fork_Osaka' "~21000" "74" &
            ;;
        rlp)
            run_shard "$shard_name" "ethereum/eels/consume-rlp" '.*eip2930_access_list.*' "unchanged" "19" &
            ;;
        *)
            echo "error: unknown shard '$shard_name'" >&2
            return 1
            ;;
    esac
    pids+=($!)
    names+=("$shard_name")
}

IFS=',' read -r -a shard_list <<< "$SHARDS"
pids=()
names=()

echo "log_dir=$LOG_DIR"
echo "hive_tests_dir=$HIVE_TESTS_DIR"
echo "client=$HIVE_CLIENT"
echo "parallelism=$HIVE_PARALLELISM"
echo "shards=${SHARDS}"

for shard in "${shard_list[@]}"; do
    start_named_shard "$shard"
done

exit_code=0
for i in "${!pids[@]}"; do
    pid="${pids[$i]}"
    name="${names[$i]}"
    if wait "$pid"; then
        echo "PASS shard=${name}"
    else
        echo "FAIL shard=${name}" >&2
        exit_code=1
    fi
done

echo "all_logs=$LOG_DIR"
exit "$exit_code"
