#!/usr/bin/env bash
#
# N42 Testnet — Mobile Phone Simulator
#
# Simulates phones connecting to validator StarHub ports. Ctrl+C stops it.
#
# Usage:
#   ./scripts/testnet/mobile-sim.sh
#   ./scripts/testnet/mobile-sim.sh --data-dir ~/my-testnet --phones 50
#
# Requires setup.sh and nodes.sh to have been started first.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="mobile-sim"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
DATA_DIR=""
PHONE_COUNT_OVERRIDE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir) DATA_DIR="$2"; shift 2 ;;
        --phones)   PHONE_COUNT_OVERRIDE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--data-dir DIR] [--phones N]"
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

if [[ -z "$MOBILE_SIM_BIN" ]] || [[ ! -f "$MOBILE_SIM_BIN" ]]; then
    err "n42-mobile-sim binary not found. Re-run setup.sh."
    exit 1
fi

PHONE_COUNT="${PHONE_COUNT_OVERRIDE:-$NUM_VALIDATORS}"

STARHUB_PORTS=""
for _i in $(seq 0 $(( NUM_VALIDATORS - 1 ))); do
    _p=$(( BASE_STARHUB + _i ))
    STARHUB_PORTS="${STARHUB_PORTS:+${STARHUB_PORTS},}${_p}"
done

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
SIM_PID=""

cleanup() {
    echo ""
    [[ -n "$SIM_PID" ]] && kill_pids_with_timeout 5 "$SIM_PID"
    log "Mobile simulator stopped."
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Start
# ---------------------------------------------------------------------------
log "Starting mobile simulator: ${PHONE_COUNT} phones -> StarHub ports ${STARHUB_PORTS}"
log "Press Ctrl+C to stop."

N42_MIN_ATTESTATION_THRESHOLD=1 \
"$MOBILE_SIM_BIN" \
    --starhub-ports "$STARHUB_PORTS" \
    --phone-count "$PHONE_COUNT" &

SIM_PID=$!
log "PID: $SIM_PID"

wait "$SIM_PID"
