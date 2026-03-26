#!/usr/bin/env bash
#
# N42 Testnet — One-time Setup
#
# Builds binaries, creates Python venv, generates genesis.json,
# and writes config.env for use by the other module scripts.
#
# Usage:
#   ./scripts/testnet/setup.sh
#   ./scripts/testnet/setup.sh --nodes 4
#   ./scripts/testnet/setup.sh --nodes 3 --debug --clean
#   ./scripts/testnet/setup.sh --data-dir ~/my-testnet --nodes 7
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="setup"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
NUM_VALIDATORS=7
CLEAN=false
DEBUG_BUILD=false
DATA_DIR=""
BLOCK_INTERVAL_MS=2000

while [[ $# -gt 0 ]]; do
    case "$1" in
        --nodes)          NUM_VALIDATORS="$2"; shift 2 ;;
        --clean)          CLEAN=true; shift ;;
        --debug)          DEBUG_BUILD=true; shift ;;
        --data-dir)       DATA_DIR="$2"; shift 2 ;;
        --block-interval) BLOCK_INTERVAL_MS="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--nodes N] [--clean] [--debug] [--data-dir DIR] [--block-interval MS]"
            exit 0
            ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

[[ -z "$DATA_DIR" ]] && DATA_DIR="$(default_data_dir "$NUM_VALIDATORS")"

# ---------------------------------------------------------------------------
# Clean
# ---------------------------------------------------------------------------
if [[ "$CLEAN" == true ]]; then
    warn "Cleaning testnet data at $DATA_DIR..."
    rm -rf "$DATA_DIR"
    if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
        warn "Cleaning Blockscout Docker volumes..."
        docker compose \
            -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" \
            down -v 2>/dev/null || true
    fi
fi

mkdir -p "$DATA_DIR"
log "Data directory: $DATA_DIR"

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------
local_build_dir="release"
build_flag="--release"
if [[ "$DEBUG_BUILD" == true ]]; then
    build_flag=""
    local_build_dir="debug"
fi

log "Building n42-node and n42-mobile-sim (${local_build_dir})..."
cd "$PROJECT_DIR"
cargo build $build_flag --bin n42-node --bin n42-mobile-sim 2>&1 | tail -5

BINARY="$PROJECT_DIR/target/${local_build_dir}/n42-node"
if [[ ! -f "$BINARY" ]]; then
    err "Build failed: $BINARY not found"
    exit 1
fi
log "Binary ready: $BINARY"

MOBILE_SIM_BIN="$PROJECT_DIR/target/${local_build_dir}/n42-mobile-sim"
if [[ -f "$MOBILE_SIM_BIN" ]]; then
    log "Mobile sim binary ready: $MOBILE_SIM_BIN"
else
    warn "n42-mobile-sim not found (non-fatal)"
    MOBILE_SIM_BIN=""
fi

# ---------------------------------------------------------------------------
# Python venv
# ---------------------------------------------------------------------------
VENV_DIR="$DATA_DIR/.venv"
if [[ ! -d "$VENV_DIR" ]]; then
    log "Creating Python venv at $VENV_DIR..."
    python3 -m venv "$VENV_DIR"
fi
# shellcheck source=/dev/null
source "$VENV_DIR/bin/activate"

if ! python3 -c "import eth_account" 2>/dev/null; then
    log "Installing Python dependencies..."
    pip3 install --quiet -r "$SCRIPTS_DIR/requirements-testnet.txt"
fi
log "Python venv ready: $(python3 --version)"

# ---------------------------------------------------------------------------
# Genesis
# ---------------------------------------------------------------------------
GENESIS_FILE="$DATA_DIR/genesis.json"
ACCOUNTS_CACHE="$SCRIPTS_DIR/testnet-accounts-cache.json"

if [[ ! -f "$GENESIS_FILE" ]]; then
    log "Generating genesis.json..."
    DATA_DIR="$DATA_DIR" ACCOUNTS_CACHE="$ACCOUNTS_CACHE" \
    N42_GAS_LIMIT="${N42_GAS_LIMIT:-2000000000}" \
    python3 << 'PYEOF'
import json, os, time

NUM_ACCOUNTS = 5000
CHAIN_ID = 4242
INITIAL_BALANCE = "0x4B3B4CA85A86C47A098A224000000"

data_dir = os.environ["DATA_DIR"]
cache_file = os.environ.get("ACCOUNTS_CACHE", "")

if cache_file and os.path.exists(cache_file):
    t0 = time.time()
    with open(cache_file) as f:
        accounts = json.load(f)
    print(f"  Loaded {len(accounts)} accounts from cache ({time.time()-t0:.2f}s)")
else:
    from eth_account import Account
    from eth_utils import keccak
    t0 = time.time()
    accounts = []
    for i in range(NUM_ACCOUNTS):
        private_key = keccak(f"n42-test-key-{i}".encode())
        acct = Account.from_key(private_key)
        accounts.append({"key": private_key.hex(), "address": acct.address})
        if i % 500 == 0:
            print(f"  Account {i}/{NUM_ACCOUNTS}...")
    print(f"  Generated {len(accounts)} accounts ({time.time()-t0:.2f}s)")
    if cache_file:
        with open(cache_file, "w") as f:
            json.dump(accounts, f, indent=2)

genesis = {
    "config": {
        "chainId": CHAIN_ID,
        "homesteadBlock": 0, "eip150Block": 0, "eip155Block": 0,
        "eip158Block": 0, "byzantiumBlock": 0, "constantinopleBlock": 0,
        "petersburgBlock": 0, "istanbulBlock": 0, "muirGlacierBlock": 0,
        "berlinBlock": 0, "londonBlock": 0, "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0, "mergeNetsplitBlock": 0,
        "shanghaiTime": 0, "cancunTime": 0,
        "terminalTotalDifficulty": "0x0",
        "terminalTotalDifficultyPassed": True,
    },
    "nonce": "0x0", "timestamp": "0x0", "extraData": "0x",
    "gasLimit": hex(int(os.environ.get("N42_GAS_LIMIT", "2000000000"))),
    "difficulty": "0x0",
    "mixHash": "0x" + "0" * 64,
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc": {"balance": "0x60EF6B1ABA6F072330000000"},
        **{a["address"]: {"balance": INITIAL_BALANCE} for a in accounts},
    },
    "number": "0x0", "gasUsed": "0x0",
    "parentHash": "0x" + "0" * 64,
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0", "blobGasUsed": "0x0",
}

with open(os.path.join(data_dir, "genesis.json"), "w") as f:
    json.dump(genesis, f, indent=2)
with open(os.path.join(data_dir, "test-accounts.json"), "w") as f:
    json.dump(accounts, f, indent=2)
print("  Genesis written.")
PYEOF

    if [[ $? -ne 0 ]]; then
        err "Genesis generation failed"
        exit 1
    fi
else
    log "Using existing genesis: $GENESIS_FILE"
fi

# ---------------------------------------------------------------------------
# Write config.env
# ---------------------------------------------------------------------------
cat > "$DATA_DIR/config.env" << EOF
# Auto-generated by setup.sh — do not edit manually
NUM_VALIDATORS=${NUM_VALIDATORS}
BINARY=${BINARY}
MOBILE_SIM_BIN=${MOBILE_SIM_BIN}
GENESIS_FILE=${GENESIS_FILE}
BLOCK_INTERVAL_MS=${BLOCK_INTERVAL_MS}
BUILD_DIR=${local_build_dir}
EOF

log "Config written: $DATA_DIR/config.env"
log ""
log "Setup complete. Next steps:"
log "  ./scripts/testnet/nodes.sh"
log "  ./scripts/testnet/blockscout.sh   (optional)"
log "  ./scripts/testnet/monitor.sh      (optional)"
