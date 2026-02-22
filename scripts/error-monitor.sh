#!/usr/bin/env bash
#
# N42 Testnet Error Monitor
#
# Monitors all validator node logs for errors, checks node health periodically,
# detects crashes (PID disappearance), and alerts on block height divergence.
#
# Usage:
#   ./scripts/error-monitor.sh --data-dir DIR --pids PID1,PID2,... --nodes 7
#
# Outputs:
#   $DATA_DIR/testnet-errors.log   — all error events
#   $DATA_DIR/testnet-crashes.log  — crash events with context
#

set -uo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

HEALTH_CHECK_INTERVAL=30    # seconds between health checks
MAX_HEIGHT_DIVERGENCE=10    # blocks — trigger alert if nodes diverge more than this
BASE_HTTP_RPC=18545         # must match testnet-7node.sh

# Colors
RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

DATA_DIR=""
PIDS_CSV=""
NUM_NODES=7

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)  DATA_DIR="$2";  shift 2 ;;
        --pids)      PIDS_CSV="$2";  shift 2 ;;
        --nodes)     NUM_NODES="$2"; shift 2 ;;
        *)           echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$DATA_DIR" ]]; then
    echo "Error: --data-dir is required"
    exit 1
fi

ERROR_LOG="$DATA_DIR/testnet-errors.log"
CRASH_LOG="$DATA_DIR/testnet-crashes.log"

# Parse PIDs into array
IFS=',' read -ra NODE_PIDS <<< "$PIDS_CSV"

# ---------------------------------------------------------------------------
# Logging helpers
# ---------------------------------------------------------------------------

ts() { date '+%Y-%m-%d %H:%M:%S'; }

log_error() {
    local msg="[$(ts)] ERROR: $*"
    echo -e "${RED}${msg}${NC}"
    echo "$msg" >> "$ERROR_LOG"
}

log_crash() {
    local msg="[$(ts)] CRASH: $*"
    echo -e "${RED}${msg}${NC}"
    echo "$msg" >> "$CRASH_LOG"
}

log_warn() {
    local msg="[$(ts)] WARN: $*"
    echo -e "${YELLOW}${msg}${NC}"
    echo "$msg" >> "$ERROR_LOG"
}

log_info() {
    echo -e "${GREEN}[$(ts)]${NC} $*"
}

# ---------------------------------------------------------------------------
# Initialize log files
# ---------------------------------------------------------------------------

echo "=== N42 Testnet Error Monitor started at $(ts) ===" >> "$ERROR_LOG"
echo "=== N42 Testnet Error Monitor started at $(ts) ===" >> "$CRASH_LOG"
log_info "Error monitor started — watching $NUM_NODES nodes"
log_info "Error log:  $ERROR_LOG"
log_info "Crash log:  $CRASH_LOG"

# ---------------------------------------------------------------------------
# Background: tail all node logs and filter for error patterns
# ---------------------------------------------------------------------------

ERROR_PATTERNS='ERROR|FATAL|panic|SIGSEGV|SIGABRT|out of memory|OOM|consensus.*timeout|fork.*detected|database.*corrupt'

start_log_watcher() {
    local log_files=()
    for i in $(seq 0 $((NUM_NODES - 1))); do
        local lf="$DATA_DIR/validator-${i}.log"
        [[ -f "$lf" ]] && log_files+=("$lf")
    done

    if [[ ${#log_files[@]} -eq 0 ]]; then
        log_warn "No log files found yet, will retry..."
        return 1
    fi

    tail -F "${log_files[@]}" 2>/dev/null | while IFS= read -r line; do
        if echo "$line" | grep -qEi "$ERROR_PATTERNS"; then
            log_error "$line"
        fi
    done &
    TAIL_PID=$!
    log_info "Log watcher started (PID $TAIL_PID) for ${#log_files[@]} files"
    return 0
}

# Try to start the log watcher, retrying until files appear
TAIL_PID=""
for attempt in $(seq 1 30); do
    if start_log_watcher; then
        break
    fi
    sleep 2
done

# ---------------------------------------------------------------------------
# RPC helper
# ---------------------------------------------------------------------------

rpc_call() {
    local port=$1
    local method=$2
    local params="${3:-[]}"
    curl -s --max-time 5 \
        -X POST \
        -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}" \
        "http://127.0.0.1:${port}/" 2>/dev/null
}

get_block_number() {
    local port=$1
    local result
    result=$(rpc_call "$port" "eth_blockNumber")
    if [[ -n "$result" ]]; then
        local hex
        hex=$(echo "$result" | python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null)
        printf '%d' "$hex" 2>/dev/null || echo "-1"
    else
        echo "-1"
    fi
}

# ---------------------------------------------------------------------------
# Cleanup on exit
# ---------------------------------------------------------------------------

cleanup() {
    log_info "Error monitor shutting down..."
    [[ -n "$TAIL_PID" ]] && kill "$TAIL_PID" 2>/dev/null
    wait 2>/dev/null
    log_info "Error monitor stopped"
}

trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Main health check loop
# ---------------------------------------------------------------------------

log_info "Starting health check loop (every ${HEALTH_CHECK_INTERVAL}s)"

while true; do
    sleep "$HEALTH_CHECK_INTERVAL"

    # Check PIDs for crashes
    for i in $(seq 0 $((NUM_NODES - 1))); do
        if [[ $i -lt ${#NODE_PIDS[@]} ]]; then
            local_pid="${NODE_PIDS[$i]}"
            if [[ -n "$local_pid" ]] && ! kill -0 "$local_pid" 2>/dev/null; then
                log_crash "Validator $i (PID $local_pid) has CRASHED!"

                local crash_log_file="$DATA_DIR/validator-${i}.log"
                if [[ -f "$crash_log_file" ]]; then
                    echo "--- Last 50 lines of validator $i log at $(ts) ---" >> "$CRASH_LOG"
                    tail -50 "$crash_log_file" >> "$CRASH_LOG" 2>/dev/null
                    echo "--- End of crash context ---" >> "$CRASH_LOG"
                fi

                echo "--- Block height snapshot at crash time ---" >> "$CRASH_LOG"
                for j in $(seq 0 $((NUM_NODES - 1))); do
                    local port=$((BASE_HTTP_RPC + j))
                    local height
                    height=$(get_block_number "$port")
                    echo "  Validator $j (port $port): block $height" >> "$CRASH_LOG"
                done
                echo "--- End snapshot ---" >> "$CRASH_LOG"

                NODE_PIDS[$i]=""
            fi
        fi
    done

    # Query block heights from all nodes
    declare -a HEIGHTS=()
    local alive_count=0
    local max_height=0
    local min_height=999999999

    for i in $(seq 0 $((NUM_NODES - 1))); do
        local port=$((BASE_HTTP_RPC + i))
        local height
        height=$(get_block_number "$port")
        HEIGHTS+=("$height")

        if [[ "$height" -ge 0 ]]; then
            alive_count=$((alive_count + 1))
            [[ "$height" -gt "$max_height" ]] && max_height=$height
            [[ "$height" -lt "$min_height" ]] && min_height=$height
        fi
    done

    # Report status
    local height_summary=""
    for i in $(seq 0 $((NUM_NODES - 1))); do
        height_summary+="v${i}=${HEIGHTS[$i]:-?} "
    done
    log_info "Health: ${alive_count}/${NUM_NODES} alive  blocks: ${height_summary}"

    # Check for block height divergence
    if [[ $alive_count -ge 2 && $min_height -ge 0 ]]; then
        local divergence=$((max_height - min_height))
        if [[ $divergence -gt $MAX_HEIGHT_DIVERGENCE ]]; then
            log_warn "Block height divergence: $divergence blocks (max=$max_height min=$min_height)"
        fi
    fi

    # Check for stalled chain (no block progress)
    if [[ -n "${LAST_MAX_HEIGHT:-}" && "$max_height" -le "$LAST_MAX_HEIGHT" && $alive_count -gt 0 ]]; then
        log_warn "Chain may be stalled: block height unchanged at $max_height"
    fi
    LAST_MAX_HEIGHT=$max_height

    # Check consensus status on node 0
    local consensus_result
    consensus_result=$(rpc_call "$BASE_HTTP_RPC" "n42_consensusStatus")
    if [[ -n "$consensus_result" ]] && echo "$consensus_result" | grep -q '"error"'; then
        log_warn "Consensus status error on node 0: $consensus_result"
    fi

    unset HEIGHTS
done
