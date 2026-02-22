#!/usr/bin/env bash
#
# N42 7-Node Testnet — All-in-One Launcher
#
# Starts a 7-validator HotStuff-2 testnet on a single machine with:
#   - Release-optimized n42-node binaries
#   - Custom genesis (10 pre-funded test accounts, chain ID 4242)
#   - Blockscout block explorer (http://localhost:3000)
#   - Continuous transaction load generator
#   - Error monitor daemon
#
# Usage:
#   ./scripts/testnet-7node.sh                         # full stack
#   ./scripts/testnet-7node.sh --clean                 # wipe data first
#   ./scripts/testnet-7node.sh --no-explorer --no-tx-gen  # nodes only
#
# Ctrl+C to gracefully stop everything.
#

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

NUM_VALIDATORS=7
DATA_DIR="$HOME/n42-testnet-data"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Port allocation — no conflicts between 7 nodes:
#   Node i: HTTP=18545+i  WS=18645+i  Auth=18551+i  P2P=30303+i  Consensus=9400+i  Metrics=19001+i
BASE_HTTP_RPC=18545
BASE_WS=18645
BASE_AUTH=18751
BASE_P2P=30303
BASE_CONSENSUS=9400
BASE_METRICS=19001
BASE_STARHUB=9500

# Chain configuration
CHAIN_ID=4242

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

CLEAN=false
NO_EXPLORER=false
NO_TX_GEN=false
NO_MONITOR=false

for arg in "$@"; do
    case "$arg" in
        --clean)        CLEAN=true ;;
        --no-explorer)  NO_EXPLORER=true ;;
        --no-tx-gen)    NO_TX_GEN=true ;;
        --no-monitor)   NO_MONITOR=true ;;
        -h|--help)
            echo "Usage: $0 [--clean] [--no-explorer] [--no-tx-gen] [--no-monitor]"
            echo ""
            echo "Options:"
            echo "  --clean         Wipe all testnet data before starting"
            echo "  --no-explorer   Don't start Blockscout"
            echo "  --no-tx-gen     Don't start transaction generator"
            echo "  --no-monitor    Don't start error monitor"
            exit 0
            ;;
        *)
            echo -e "${RED}Unknown option: $arg${NC}"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Logging helpers
# ---------------------------------------------------------------------------

log()  { echo -e "${GREEN}[testnet]${NC} $*"; }
warn() { echo -e "${YELLOW}[testnet]${NC} $*"; }
err()  { echo -e "${RED}[testnet]${NC} $*"; }
info() { echo -e "${CYAN}[testnet]${NC} $*"; }

# ---------------------------------------------------------------------------
# File descriptor limit — critical for multi-day operation with 7 nodes
# ---------------------------------------------------------------------------

current_ulimit=$(ulimit -n 2>/dev/null || echo "256")
if [[ "$current_ulimit" != "unlimited" ]] && [[ "$current_ulimit" -lt 65536 ]]; then
    ulimit -n 65536 2>/dev/null || warn "Could not raise ulimit to 65536 (current: $current_ulimit). May cause issues."
fi

# ---------------------------------------------------------------------------
# Cleanup handler
# ---------------------------------------------------------------------------

NODE_PIDS=()
TX_GEN_PID=""
MONITOR_PID=""
BLOCKSCOUT_STARTED=false

cleanup() {
    echo ""
    log "Shutting down testnet..."

    if [[ -n "$TX_GEN_PID" ]] && kill -0 "$TX_GEN_PID" 2>/dev/null; then
        log "Stopping transaction generator (PID $TX_GEN_PID)..."
        kill "$TX_GEN_PID" 2>/dev/null || true
    fi

    if [[ -n "$MONITOR_PID" ]] && kill -0 "$MONITOR_PID" 2>/dev/null; then
        log "Stopping error monitor (PID $MONITOR_PID)..."
        kill "$MONITOR_PID" 2>/dev/null || true
    fi

    if [[ "$BLOCKSCOUT_STARTED" == true ]]; then
        log "Stopping Blockscout..."
        docker compose -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -p n42-testnet-blockscout down 2>/dev/null || true
    fi

    if [[ ${#NODE_PIDS[@]} -gt 0 ]]; then
        log "Stopping ${#NODE_PIDS[@]} validator nodes..."
        for pid in "${NODE_PIDS[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
            fi
        done
    fi

    wait 2>/dev/null || true
    log "All services stopped."
}

trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Step 0: Clean data if requested
# ---------------------------------------------------------------------------

if [[ "$CLEAN" == true ]]; then
    warn "Cleaning testnet data at $DATA_DIR..."
    rm -rf "$DATA_DIR"
fi

mkdir -p "$DATA_DIR"

# ---------------------------------------------------------------------------
# Step 0.5: Setup Python virtual environment
# ---------------------------------------------------------------------------

VENV_DIR="$DATA_DIR/.venv"
if [[ ! -d "$VENV_DIR" ]]; then
    log "Creating Python virtual environment at $VENV_DIR..."
    python3 -m venv "$VENV_DIR"
fi
# Activate venv — all subsequent python3/pip3 calls use it
source "$VENV_DIR/bin/activate"

# Install dependencies if not already present
if ! python3 -c "import eth_account" 2>/dev/null; then
    log "Installing Python dependencies..."
    pip3 install --quiet -r "$SCRIPT_DIR/requirements-testnet.txt"
fi
log "Python venv ready: $(python3 --version)"

# ---------------------------------------------------------------------------
# Step 1: Build release binary
# ---------------------------------------------------------------------------

log "Building n42-node (release) - this may take a few minutes..."
cd "$PROJECT_DIR"
cargo build --release --bin n42-node 2>&1 | tail -5

BINARY="$PROJECT_DIR/target/release/n42-node"
if [[ ! -f "$BINARY" ]]; then
    err "Failed to build n42-node binary"
    exit 1
fi
log "Binary ready: $BINARY"

# ---------------------------------------------------------------------------
# Step 2: Generate genesis.json with Python
# ---------------------------------------------------------------------------

GENESIS_FILE="$DATA_DIR/genesis.json"

if [[ ! -f "$GENESIS_FILE" ]] || [[ "$CLEAN" == true ]]; then
    log "Generating genesis.json with pre-funded test accounts..."

    DATA_DIR="$DATA_DIR" python3 << 'PYEOF'
import json
import os
import sys

# CRITICAL: Use eth_utils.keccak (Keccak-256), NOT hashlib.sha3_256 (NIST SHA3)
from eth_account import Account
from eth_utils import keccak

NUM_ACCOUNTS = 10
CHAIN_ID = 4242
# 100,000,000 * 10^18 = 100M N42 per account
INITIAL_BALANCE = "0x4B3B4CA85A86C47A098A224000000"

data_dir = os.environ["DATA_DIR"]
output_path = os.path.join(data_dir, "genesis.json")

# Generate deterministic test accounts (must match tests/e2e/src/genesis.rs)
accounts = []
for i in range(NUM_ACCOUNTS):
    seed = f"n42-test-key-{i}".encode()
    private_key = keccak(seed)
    acct = Account.from_key(private_key)
    accounts.append({
        "key": private_key.hex(),
        "address": acct.address,
    })
    print(f"  Account {i}: {acct.address}  key={private_key.hex()[:16]}...")

# Build genesis allocation
alloc = {}
for acct in accounts:
    # Use checksummed address (standard format)
    alloc[acct["address"]] = {"balance": INITIAL_BALANCE}

genesis = {
    "config": {
        "chainId": CHAIN_ID,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "shanghaiTime": 0,
        "cancunTime": 0,
        "terminalTotalDifficulty": "0x0",
        "terminalTotalDifficultyPassed": True,
    },
    "nonce": "0x0",
    "timestamp": "0x0",
    "extraData": "0x",
    "gasLimit": "0x1C9C380",
    "difficulty": "0x0",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": alloc,
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0",
    "blobGasUsed": "0x0",
}

with open(output_path, "w") as f:
    json.dump(genesis, f, indent=2)

# Also write account keys to a file for reference
keys_path = os.path.join(data_dir, "test-accounts.json")
with open(keys_path, "w") as f:
    json.dump(accounts, f, indent=2)

print(f"  Genesis written to {output_path}")
print(f"  Account keys written to {keys_path}")
PYEOF

    if [[ $? -ne 0 ]]; then
        err "Genesis generation failed. Are eth-account and eth-utils installed?"
        err "Run: pip3 install -r scripts/requirements-testnet.txt"
        exit 1
    fi
else
    log "Using existing genesis at $GENESIS_FILE"
fi

# ---------------------------------------------------------------------------
# Step 3: Generate BLS validator keys
# ---------------------------------------------------------------------------

# Deterministic BLS keys matching local-testnet.sh / ConsensusConfig::deterministic_key_bytes()
# Format: 32 bytes, all zeros except last 4 bytes = (index+1) big-endian
generate_key() {
    local index=$1
    local val=$((index + 1))
    printf '%056x%08x' 0 "$val"
}

log "Validator BLS keys:"
KEYS=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    key=$(generate_key "$i")
    KEYS+=("$key")
    echo -e "  ${BLUE}Validator ${i}${NC}: key=${key:0:16}..."
done

# ---------------------------------------------------------------------------
# Step 4: Start validator nodes
# ---------------------------------------------------------------------------

log "Starting $NUM_VALIDATORS validator nodes..."

ENODES=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    datadir="$DATA_DIR/validator-${i}"
    http_port=$((BASE_HTTP_RPC + i))
    ws_port=$((BASE_WS + i))
    auth_port=$((BASE_AUTH + i))
    p2p_port=$((BASE_P2P + i))
    consensus_port=$((BASE_CONSENSUS + i))
    metrics_port=$((BASE_METRICS + i))
    starhub_port=$((BASE_STARHUB + i))
    log_file="$DATA_DIR/validator-${i}.log"

    mkdir -p "$datadir"

    TRUSTED_PEERS_FLAG=""
    if [[ ${#ENODES[@]} -gt 0 ]]; then
        TRUSTED_PEERS_FLAG="--trusted-peers $(IFS=,; echo "${ENODES[*]}")"
    fi

    log "  Validator $i: http=:$http_port ws=:$ws_port p2p=:$p2p_port consensus=:$consensus_port"

    N42_VALIDATOR_KEY="${KEYS[$i]}" \
    N42_VALIDATOR_COUNT="$NUM_VALIDATORS" \
    N42_ENABLE_MDNS="true" \
    N42_DATA_DIR="$datadir" \
    N42_CONSENSUS_PORT="$consensus_port" \
    N42_STARHUB_PORT="$starhub_port" \
    N42_MAX_EMPTY_SKIPS="0" \
    N42_BLOCK_INTERVAL_MS="4000" \
    N42_BASE_TIMEOUT_MS="20000" \
    N42_MAX_TIMEOUT_MS="60000" \
    N42_STARTUP_DELAY_MS="3000" \
    "$BINARY" node \
        --chain "$GENESIS_FILE" \
        --datadir "$datadir" \
        --http \
        --http.port "$http_port" \
        --http.api eth,net,web3,debug,trace,txpool,rpc \
        --http.corsdomain "*" \
        --ws \
        --ws.port "$ws_port" \
        --ws.api eth,net,web3 \
        --authrpc.port "$auth_port" \
        --port "$p2p_port" \
        --discovery.port "$p2p_port" \
        --log.file.directory "$datadir/logs" \
        --metrics "127.0.0.1:$metrics_port" \
        ${TRUSTED_PEERS_FLAG} \
        > "$log_file" 2>&1 &

    NODE_PIDS+=($!)
    log "    PID: ${NODE_PIDS[$i]}, log: $log_file"

    # Record enode for subsequent nodes
    ENODES+=("enode://0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000${i}@127.0.0.1:${p2p_port}")

    # Brief pause for initialization
    sleep 2
done

# Write PIDs to file for error monitor
PIDS_CSV=$(IFS=,; echo "${NODE_PIDS[*]}")
echo "$PIDS_CSV" > "$DATA_DIR/node-pids.txt"

# ---------------------------------------------------------------------------
# Step 5: Wait for block production
# ---------------------------------------------------------------------------

log "Waiting for block production..."
MAX_WAIT=120
waited=0
while [[ $waited -lt $MAX_WAIT ]]; do
    block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:$BASE_HTTP_RPC/" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

    block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
    if [[ $block_num -gt 0 ]]; then
        log "Chain active! Block $block_num produced."
        break
    fi

    sleep 3
    waited=$((waited + 3))
    [[ $((waited % 15)) -eq 0 ]] && info "Waiting for blocks... (${waited}s / ${MAX_WAIT}s)"
done

if [[ $waited -ge $MAX_WAIT ]]; then
    warn "Timeout waiting for blocks (nodes may still be syncing)"
    warn "Check logs: $DATA_DIR/validator-*.log"
fi

# ---------------------------------------------------------------------------
# Step 6: Start Blockscout (optional)
# ---------------------------------------------------------------------------

if [[ "$NO_EXPLORER" == false ]]; then
    log "Starting Blockscout block explorer..."

    if ! command -v docker &>/dev/null; then
        warn "Docker not found - skipping Blockscout"
    elif ! docker info &>/dev/null 2>&1; then
        warn "Docker daemon not running - skipping Blockscout"
    else
        cat > "$DATA_DIR/docker-compose.blockscout-override.yml" << YAMLEOF
# Auto-generated override for 7-node testnet
# Points Blockscout at node 0's RPC (port $BASE_HTTP_RPC)
services:
  backend:
    environment:
      ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_TRACE_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:${BASE_WS}
  frontend:
    environment:
      NEXT_PUBLIC_APP_HOST: localhost
      NEXT_PUBLIC_APP_PROTOCOL: http
YAMLEOF

        docker compose \
            -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -f "$DATA_DIR/docker-compose.blockscout-override.yml" \
            -p n42-testnet-blockscout \
            up -d 2>&1 | tail -5

        BLOCKSCOUT_STARTED=true
        log "Blockscout starting: http://localhost:3000 (frontend), http://localhost:4000 (API)"
    fi
fi

# ---------------------------------------------------------------------------
# Step 7: Start transaction generator (optional)
# ---------------------------------------------------------------------------

if [[ "$NO_TX_GEN" == false ]]; then
    log "Starting transaction load generator (2 tx/sec)..."

    python3 "$SCRIPT_DIR/tx-load-generator.py" \
        --rpc "http://127.0.0.1:$BASE_HTTP_RPC" \
        --rate 2 \
        > "$DATA_DIR/tx-generator.log" 2>&1 &

    TX_GEN_PID=$!
    log "  TX generator PID: $TX_GEN_PID, log: $DATA_DIR/tx-generator.log"
fi

# ---------------------------------------------------------------------------
# Step 8: Start error monitor (optional)
# ---------------------------------------------------------------------------

if [[ "$NO_MONITOR" == false ]]; then
    log "Starting error monitor..."

    bash "$SCRIPT_DIR/error-monitor.sh" \
        --data-dir "$DATA_DIR" \
        --pids "$PIDS_CSV" \
        --nodes "$NUM_VALIDATORS" \
        > "$DATA_DIR/monitor.log" 2>&1 &

    MONITOR_PID=$!
    log "  Error monitor PID: $MONITOR_PID, log: $DATA_DIR/monitor.log"
fi

# ---------------------------------------------------------------------------
# Step 9: Open browser
# ---------------------------------------------------------------------------

if [[ "$NO_EXPLORER" == false && "$BLOCKSCOUT_STARTED" == true ]]; then
    sleep 3
    if command -v open &>/dev/null; then
        open "http://localhost:3000" 2>/dev/null || true
    fi
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
log "===== N42 7-Node Testnet Running ====="
echo ""
echo -e "  ${CYAN}Data directory:${NC}  $DATA_DIR"
echo -e "  ${CYAN}Genesis file:${NC}    $GENESIS_FILE"
echo ""
echo -e "  ${CYAN}Validator nodes:${NC}"
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    http_port=$((BASE_HTTP_RPC + i))
    echo -e "    Validator $i: ${BLUE}http://127.0.0.1:${http_port}${NC}  PID=${NODE_PIDS[$i]}"
done
echo ""
if [[ "$BLOCKSCOUT_STARTED" == true ]]; then
    echo -e "  ${CYAN}Blockscout:${NC}      ${BLUE}http://localhost:3000${NC}"
fi
if [[ -n "$TX_GEN_PID" ]]; then
    echo -e "  ${CYAN}TX Generator:${NC}    PID=$TX_GEN_PID  (2 tx/sec)"
fi
if [[ -n "$MONITOR_PID" ]]; then
    echo -e "  ${CYAN}Error Monitor:${NC}   PID=$MONITOR_PID"
fi
echo ""
echo -e "  ${CYAN}Logs:${NC}"
echo "    Node logs:     $DATA_DIR/validator-*.log"
echo "    TX generator:  $DATA_DIR/tx-generator.log"
echo "    Error events:  $DATA_DIR/testnet-errors.log"
echo "    Crash events:  $DATA_DIR/testnet-crashes.log"
echo ""
echo -e "  ${YELLOW}Quick commands:${NC}"
echo "    # Check block number:"
echo "    curl -s http://127.0.0.1:$BASE_HTTP_RPC -X POST -H 'Content-Type: application/json' \\"
echo "      -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}'"
echo ""
echo "    # Check consensus status:"
echo "    curl -s http://127.0.0.1:$BASE_HTTP_RPC -X POST -H 'Content-Type: application/json' \\"
echo "      -d '{\"jsonrpc\":\"2.0\",\"method\":\"n42_consensusStatus\",\"params\":[],\"id\":1}'"
echo ""
echo "    # Tail TX generator output:"
echo "    tail -f $DATA_DIR/tx-generator.log"
echo ""
log "Press Ctrl+C to stop all services"
echo ""

# ---------------------------------------------------------------------------
# Keep alive — monitor node processes
# ---------------------------------------------------------------------------

while true; do
    alive=0
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        if kill -0 "${NODE_PIDS[$i]}" 2>/dev/null; then
            alive=$((alive + 1))
        fi
    done

    if [[ $alive -eq 0 ]]; then
        err "All validator nodes have exited!"
        err "Check logs at $DATA_DIR/validator-*.log"
        exit 1
    fi

    sleep 10
done
