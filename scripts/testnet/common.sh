#!/usr/bin/env bash
#
# N42 Testnet — Shared constants and helper functions
# Source this file from other module scripts:
#   source "$(dirname "$0")/common.sh"
#

COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(dirname "$COMMON_DIR")"
PROJECT_DIR="$(dirname "$SCRIPTS_DIR")"

# ---------------------------------------------------------------------------
# Port allocation (400-slot gaps, supports up to 400 nodes)
#   HTTP:      18000 .. 18000+N-1
#   WS:        18400 .. 18400+N-1
#   AUTH:      18800 .. 18800+N-1
#   METRICS:   19200 .. 19200+N-1
#   P2P:       30303 .. 30303+N-1
#   CONSENSUS:  9000 ..  9000+N-1
#   STARHUB:    9400 ..  9400+N-1
# ---------------------------------------------------------------------------
BASE_HTTP_RPC=18000
BASE_WS=18400
BASE_AUTH=18800
BASE_METRICS=19200
BASE_P2P=30303
BASE_CONSENSUS=9000
BASE_STARHUB=9400

CHAIN_ID=4242

# ---------------------------------------------------------------------------
# Colors / logging
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${GREEN}[${LOG_PREFIX:-n42}]${NC} $*"; }
warn() { echo -e "${YELLOW}[${LOG_PREFIX:-n42}]${NC} $*"; }
err()  { echo -e "${RED}[${LOG_PREFIX:-n42}]${NC} $*"; }
info() { echo -e "${CYAN}[${LOG_PREFIX:-n42}]${NC} $*"; }

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Default data directory for N validators
default_data_dir() {
    local n="${1:-7}"
    if [[ "$n" -eq 7 ]]; then
        echo "$HOME/n42-testnet-data"
    else
        echo "$HOME/n42-testnet-data-${n}"
    fi
}

# Load config.env written by setup.sh. Exits if not found.
load_config() {
    local data_dir="$1"
    local cfg="$data_dir/config.env"
    if [[ ! -f "$cfg" ]]; then
        err "Config not found: $cfg"
        err "Run setup.sh first."
        exit 1
    fi
    # shellcheck source=/dev/null
    source "$cfg"
}

# Activate the Python venv created by setup.sh
activate_venv() {
    local data_dir="$1"
    local venv="$data_dir/.venv"
    if [[ ! -d "$venv" ]]; then
        err "Python venv not found at $venv. Run setup.sh first."
        exit 1
    fi
    # shellcheck source=/dev/null
    source "$venv/bin/activate"
}

# Raise the file descriptor limit
setup_ulimit() {
    local cur
    cur=$(ulimit -n 2>/dev/null || echo "256")
    if [[ "$cur" != "unlimited" ]] && [[ "$cur" -lt 65536 ]]; then
        ulimit -n 65536 2>/dev/null || \
            warn "Could not raise ulimit to 65536 (current: $cur)"
    fi
}

# Generate deterministic BLS keys into global array KEYS[]
generate_bls_keys() {
    local n="$1"
    KEYS=()
    for i in $(seq 0 $((n - 1))); do
        KEYS+=("$(printf '%056x%08x' 0 "$((i + 1))")")
    done
}

# Generate deterministic devp2p keys into global arrays P2P_SECRETS[] / P2P_PUBKEYS[]
# Requires Python venv to be active.
generate_p2p_keys() {
    local n="$1"
    if [[ "$n" -eq 1 ]]; then
        P2P_SECRETS=("")
        P2P_PUBKEYS=("")
        return
    fi
    P2P_SECRETS=()
    P2P_PUBKEYS=()
    for i in $(seq 0 $((n - 1))); do
        read -r secret pubkey <<< "$(python3 -c "
from eth_utils import keccak
from eth_keys import keys as eth_keys
sk = keccak(b'n42-devp2p-key-$i')
pk = eth_keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
")"
        P2P_SECRETS+=("$secret")
        P2P_PUBKEYS+=("$pubkey")
    done
}

# Compute consensus timeout parameters into global vars based on node count
compute_timeouts() {
    local n="$1"
    case "$n" in
        1)
            BASE_TIMEOUT_MS=10000; MAX_TIMEOUT_MS=30000
            STARTUP_DELAY_MS=2000; NODE_START_INTERVAL=0; MAX_BLOCK_WAIT=60
            ;;
        3)
            BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000
            STARTUP_DELAY_MS=2000; NODE_START_INTERVAL=3; MAX_BLOCK_WAIT=78
            ;;
        4)
            BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000
            STARTUP_DELAY_MS=2000; NODE_START_INTERVAL=3; MAX_BLOCK_WAIT=90
            ;;
        5)
            BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000
            STARTUP_DELAY_MS=2000; NODE_START_INTERVAL=2; MAX_BLOCK_WAIT=90
            ;;
        7)
            BASE_TIMEOUT_MS=10000; MAX_TIMEOUT_MS=30000
            STARTUP_DELAY_MS=3000; NODE_START_INTERVAL=2; MAX_BLOCK_WAIT=120
            ;;
        21)
            BASE_TIMEOUT_MS=30000; MAX_TIMEOUT_MS=90000
            STARTUP_DELAY_MS=5200; NODE_START_INTERVAL=1; MAX_BLOCK_WAIT=180
            ;;
        *)
            if [[ "$n" -ge 200 ]]; then
                NODE_START_INTERVAL=2
            else
                NODE_START_INTERVAL=1
            fi
            STARTUP_DELAY_MS=$(( (n * NODE_START_INTERVAL * 1000) + 60000 ))
            BASE_TIMEOUT_MS=$(( STARTUP_DELAY_MS + 30000 ))
            MAX_TIMEOUT_MS=$(( BASE_TIMEOUT_MS * 3 ))
            MAX_BLOCK_WAIT=$(( (n * NODE_START_INTERVAL * 3) + 180 ))
            ;;
    esac
}

# Send SIGTERM to a list of PIDs, wait up to timeout_s seconds, then SIGKILL
kill_pids_with_timeout() {
    local timeout_s="${1:-8}"; shift
    local pids=("$@")
    [[ ${#pids[@]} -eq 0 ]] && return

    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done

    local deadline=$(( $(date +%s) + timeout_s ))
    local remaining=("${pids[@]}")
    while [[ ${#remaining[@]} -gt 0 ]] && [[ $(date +%s) -lt $deadline ]]; do
        local still=()
        for pid in "${remaining[@]}"; do
            kill -0 "$pid" 2>/dev/null && still+=("$pid")
        done
        remaining=("${still[@]+${still[@]}}")
        [[ ${#remaining[@]} -gt 0 ]] && sleep 0.3
    done

    for pid in "${remaining[@]+${remaining[@]}}"; do
        kill -9 "$pid" 2>/dev/null || true
    done
}
