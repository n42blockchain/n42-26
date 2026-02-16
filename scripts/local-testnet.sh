#!/usr/bin/env bash
#
# Launch a local N42 testnet with multiple validator nodes.
#
# Usage:
#   ./scripts/local-testnet.sh [NUM_VALIDATORS]
#
# Defaults to 3 validators. Each node gets unique ports and data directory.
# Press Ctrl+C to stop all nodes.

set -euo pipefail

NUM_VALIDATORS="${1:-3}"
BASE_DIR="/tmp/n42-testnet"
BASE_RPC_PORT=8545
BASE_P2P_PORT=30303
BASE_CONSENSUS_PORT=9400
BASE_STARHUB_PORT=9500

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log() { echo -e "${GREEN}[testnet]${NC} $*"; }
warn() { echo -e "${YELLOW}[testnet]${NC} $*"; }
error() { echo -e "${RED}[testnet]${NC} $*"; }

cleanup() {
    log "Stopping all validator nodes..."
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done
    wait 2>/dev/null || true
    log "All nodes stopped."
}

trap cleanup EXIT INT TERM

# Generate deterministic BLS secret keys for validators.
# Uses simple sequential bytes (sufficient for local testing).
# Must match ConsensusConfig::deterministic_key_bytes() in n42-chainspec.
generate_key() {
    local index=$1
    # Generate a 32-byte key: all zeros except last 4 bytes = index+1 (big-endian)
    local val=$((index + 1))
    printf '%056x%08x' 0 "$val"
}

log "Starting N42 local testnet with ${NUM_VALIDATORS} validators"
log "Base directory: ${BASE_DIR}"

# Clean previous data
rm -rf "${BASE_DIR}"
mkdir -p "${BASE_DIR}"

# Build the binary first
log "Building n42-node..."
cargo build --bin n42-node 2>&1 | tail -5
BINARY="./target/debug/n42-node"

if [ ! -f "$BINARY" ]; then
    error "Failed to build n42-node binary"
    exit 1
fi

log "Binary built: ${BINARY}"

# Generate keys and display validator info
log "Validator configuration:"
KEYS=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    key=$(generate_key "$i")
    KEYS+=("$key")
    echo -e "  ${BLUE}Validator ${i}${NC}: key=${key:0:16}..."
done

# Start each validator node
PIDS=()
ENODES=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    datadir="${BASE_DIR}/validator-${i}"
    rpc_port=$((BASE_RPC_PORT + i))
    p2p_port=$((BASE_P2P_PORT + i))
    consensus_port=$((BASE_CONSENSUS_PORT + i))
    log_file="${BASE_DIR}/validator-${i}.log"

    mkdir -p "${datadir}"

    log "Starting validator ${i} (rpc=:${rpc_port}, p2p=:${p2p_port}, consensus=:${consensus_port})"

    # Build bootnode list: connect to all previously started nodes via reth devp2p.
    BOOTNODES_ARG=""
    if [ ${#ENODES[@]} -gt 0 ]; then
        # Use reth's --peers.connect to connect to previously started nodes.
        for enode in "${ENODES[@]}"; do
            BOOTNODES_ARG="${BOOTNODES_ARG} --peers.connect ${enode}"
        done
    fi

    N42_VALIDATOR_KEY="${KEYS[$i]}" \
    N42_VALIDATOR_COUNT="${NUM_VALIDATORS}" \
    "${BINARY}" node \
        --chain dev \
        --datadir "${datadir}" \
        --http \
        --http.port "${rpc_port}" \
        --http.api "eth,net,web3,n42" \
        --port "${p2p_port}" \
        --discovery.port "${p2p_port}" \
        --log.file.directory "${datadir}/logs" \
        ${BOOTNODES_ARG} \
        > "${log_file}" 2>&1 &

    PIDS+=($!)
    log "  PID: ${PIDS[$i]}, log: ${log_file}"

    # Record this node's enode for subsequent nodes to connect to.
    # reth devp2p uses TCP for connections.
    ENODES+=("enode://0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000${i}@127.0.0.1:${p2p_port}")

    # Brief pause to let the node initialize before starting the next.
    sleep 1
done

log ""
log "All ${NUM_VALIDATORS} validators started."
log ""
log "RPC endpoints:"
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    rpc_port=$((BASE_RPC_PORT + i))
    echo -e "  Validator ${i}: ${BLUE}http://127.0.0.1:${rpc_port}${NC}"
done
log ""
log "Test commands:"
echo -e "  ${YELLOW}# Check consensus status:${NC}"
echo -e "  ${YELLOW}curl -s http://127.0.0.1:${BASE_RPC_PORT} -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"n42_consensusStatus\",\"params\":[],\"id\":1}'${NC}"
echo -e ""
echo -e "  ${YELLOW}# Check validator set:${NC}"
echo -e "  ${YELLOW}curl -s http://127.0.0.1:${BASE_RPC_PORT} -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"n42_validatorSet\",\"params\":[],\"id\":1}'${NC}"
echo -e ""
echo -e "  ${YELLOW}# Check block number:${NC}"
echo -e "  ${YELLOW}curl -s http://127.0.0.1:${BASE_RPC_PORT} -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}'${NC}"
echo -e ""
echo -e "  ${YELLOW}# Send a test transaction (requires funded account):${NC}"
echo -e "  ${YELLOW}curl -s http://127.0.0.1:${BASE_RPC_PORT} -X POST -H 'Content-Type: application/json' -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"0x0000000000000000000000000000000000000001\",\"to\":\"0x0000000000000000000000000000000000000002\",\"value\":\"0x1\"}],\"id\":1}'${NC}"
log ""
log "Tailing logs from validator 0 (Ctrl+C to stop all):"
log ""

# Monitor node processes and tail leader log
while true; do
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        if ! kill -0 "${PIDS[$i]}" 2>/dev/null; then
            error "Validator ${i} (PID ${PIDS[$i]}) has exited!"
            error "Check log: ${BASE_DIR}/validator-${i}.log"
            # Show last 20 lines of the failed node's log
            tail -20 "${BASE_DIR}/validator-${i}.log" 2>/dev/null || true
        fi
    done
    sleep 5
done
