#!/usr/bin/env bash
#
# N42 Testnet — Error Monitor
#
# Watches node logs and processes for errors, reports chain health.
# Ctrl+C stops the monitor.
#
# Usage:
#   ./scripts/testnet/monitor.sh
#   ./scripts/testnet/monitor.sh --data-dir ~/my-testnet
#
# Requires setup.sh and nodes.sh to have been started first.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="monitor"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
DATA_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir) DATA_DIR="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--data-dir DIR]"
            exit 0
            ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$DATA_DIR" ]]; then
    for candidate in "$HOME/n42-testnet-data" "$HOME/n42-testnet-data-"*; do
        if [[ -f "$candidate/config.env" ]]; then
            DATA_DIR="$candidate"
            break
        fi
    done
fi

if [[ -z "$DATA_DIR" ]]; then
    err "Could not find testnet data dir. Run setup.sh first, or pass --data-dir."
    exit 1
fi

load_config "$DATA_DIR"

PIDS_FILE="$DATA_DIR/node-pids.txt"
if [[ ! -f "$PIDS_FILE" ]]; then
    err "node-pids.txt not found. Start nodes.sh first."
    exit 1
fi
PIDS_CSV="$(cat "$PIDS_FILE")"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
MONITOR_PID=""

cleanup() {
    echo ""
    [[ -n "$MONITOR_PID" ]] && kill_pids_with_timeout 5 "$MONITOR_PID"
    log "Monitor stopped."
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Start
# ---------------------------------------------------------------------------
log "Starting error monitor (${NUM_VALIDATORS} nodes, PIDs: ${PIDS_CSV})..."
log "Press Ctrl+C to stop."

bash "$SCRIPTS_DIR/error-monitor.sh" \
    --data-dir "$DATA_DIR" \
    --pids "$PIDS_CSV" \
    --nodes "$NUM_VALIDATORS" \
    --base-http-rpc "$BASE_HTTP_RPC" &

MONITOR_PID=$!
log "PID: $MONITOR_PID"

wait "$MONITOR_PID"
