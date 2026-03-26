#!/usr/bin/env bash
#
# N42 Testnet — Transaction Load Generator
#
# Sends continuous transactions to the testnet. Ctrl+C stops it.
#
# Usage:
#   ./scripts/testnet/tx-gen.sh
#   ./scripts/testnet/tx-gen.sh --data-dir ~/my-testnet --rate 10
#
# Requires setup.sh and nodes.sh to have been started first.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="tx-gen"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
DATA_DIR=""
RATE=2

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir) DATA_DIR="$2"; shift 2 ;;
        --rate)     RATE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--data-dir DIR] [--rate TPS]"
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
activate_venv "$DATA_DIR"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
TX_GEN_PID=""

cleanup() {
    echo ""
    [[ -n "$TX_GEN_PID" ]] && kill_pids_with_timeout 5 "$TX_GEN_PID"
    log "TX generator stopped."
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Start
# ---------------------------------------------------------------------------
log "Starting TX generator: ${RATE} tx/s -> http://127.0.0.1:${BASE_HTTP_RPC}"
log "Press Ctrl+C to stop."

python3 "$SCRIPTS_DIR/tx-load-generator.py" \
    --rpc "http://127.0.0.1:$BASE_HTTP_RPC" \
    --rate "$RATE" &

TX_GEN_PID=$!
log "PID: $TX_GEN_PID"

wait "$TX_GEN_PID"
