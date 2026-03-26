#!/usr/bin/env bash
#
# N42 Testnet — Validator Nodes
#
# Starts all validator nodes. Runs in the foreground; Ctrl+C stops all nodes.
#
# Usage:
#   ./scripts/testnet/nodes.sh
#   ./scripts/testnet/nodes.sh --data-dir ~/my-testnet
#   ./scripts/testnet/nodes.sh --block-interval 4000
#
# Requires setup.sh to have been run first.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="nodes"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
DATA_DIR=""
BLOCK_INTERVAL_OVERRIDE=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)       DATA_DIR="$2"; shift 2 ;;
        --block-interval) BLOCK_INTERVAL_OVERRIDE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--data-dir DIR] [--block-interval MS]"
            exit 0
            ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

# Auto-discover data dir
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
[[ -n "$BLOCK_INTERVAL_OVERRIDE" ]] && BLOCK_INTERVAL_MS="$BLOCK_INTERVAL_OVERRIDE"

LOG_PREFIX="${NUM_VALIDATORS}nodes"

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
NODE_PIDS=()

cleanup() {
    echo ""
    log "Stopping ${#NODE_PIDS[@]} validator node(s)..."
    kill_pids_with_timeout 8 "${NODE_PIDS[@]+${NODE_PIDS[@]}}"
    log "All nodes stopped."
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Prepare
# ---------------------------------------------------------------------------
setup_ulimit
activate_venv "$DATA_DIR"
compute_timeouts "$NUM_VALIDATORS"
generate_bls_keys "$NUM_VALIDATORS"
generate_p2p_keys "$NUM_VALIDATORS"

if [[ "${N42_LOW_MEMORY:-0}" == "1" ]]; then
    log "Starting $NUM_VALIDATORS node(s) [LOW_MEMORY mode ~200MB/node]..."
else
    log "Starting $NUM_VALIDATORS node(s) [standard mode ~5GB/node]..."
fi

# ---------------------------------------------------------------------------
# Start nodes
# ---------------------------------------------------------------------------
ENODES=()

for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    datadir="$DATA_DIR/validator-${i}"
    http_port=$(( BASE_HTTP_RPC + i ))
    ws_port=$(( BASE_WS + i ))
    auth_port=$(( BASE_AUTH + i ))
    p2p_port=$(( BASE_P2P + i ))
    consensus_port=$(( BASE_CONSENSUS + i ))
    metrics_port=$(( BASE_METRICS + i ))
    starhub_port=$(( BASE_STARHUB + i ))
    log_file="$DATA_DIR/validator-${i}.log"

    mkdir -p "$datadir"

    TRUSTED_PEERS_FLAG=""
    if [[ ${#ENODES[@]} -gt 0 ]] && [[ "$NUM_VALIDATORS" -gt 1 ]]; then
        TRUSTED_PEERS_FLAG="--trusted-peers $(IFS=,; echo "${ENODES[*]}")"
    fi

    p2p_key_flag=""
    [[ "$NUM_VALIDATORS" -gt 1 ]] && p2p_key_flag="--p2p-secret-key-hex ${P2P_SECRETS[$i]}"

    mdns_flag="false"
    [[ "$NUM_VALIDATORS" -eq 1 ]] && mdns_flag="false"

    low_mem_flags=""
    if [[ "${N42_LOW_MEMORY:-0}" == "1" ]]; then
        low_mem_flags="--engine.cross-block-cache-size 64 --engine.disable-state-cache --engine.disable-prewarming --engine.storage-worker-count 2 --engine.account-worker-count 2 --rpc.max-connections 50 --txpool.additional-validation-tasks 2"
        export TOKIO_WORKER_THREADS=2 RAYON_NUM_THREADS=2 N42_WORKER_THREADS=2
    else
        low_mem_flags="--rpc.max-connections 1000 --txpool.additional-validation-tasks 16"
        unset TOKIO_WORKER_THREADS 2>/dev/null || true
    fi

    log "  v${i}: http=:${http_port}  p2p=:${p2p_port}  consensus=:${consensus_port}"

    RUST_LOG="info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error" \
    N42_VALIDATOR_KEY="${KEYS[$i]}" \
    N42_VALIDATOR_COUNT="$NUM_VALIDATORS" \
    N42_ENABLE_MDNS="$mdns_flag" \
    N42_DATA_DIR="$datadir" \
    N42_CONSENSUS_PORT="$consensus_port" \
    N42_STARHUB_PORT="$starhub_port" \
    N42_MAX_EMPTY_SKIPS="0" \
    N42_BLOCK_INTERVAL_MS="$BLOCK_INTERVAL_MS" \
    N42_BASE_TIMEOUT_MS="$BASE_TIMEOUT_MS" \
    N42_MAX_TIMEOUT_MS="$MAX_TIMEOUT_MS" \
    N42_STARTUP_DELAY_MS="$STARTUP_DELAY_MS" \
    N42_REWARD_EPOCH_BLOCKS="50" \
    N42_DAILY_BASE_REWARD_GWEI="100000000" \
    N42_REWARD_CURVE_K="4.0" \
    N42_MIN_ATTESTATION_THRESHOLD="1" \
    N42_OPEN_VERIFICATION="1" \
    N42_MAX_TXS_PER_BLOCK="${N42_MAX_TXS_PER_BLOCK:-48000}" \
    N42_FAST_PROPOSE="${N42_FAST_PROPOSE:-0}" \
    N42_MIN_PROPOSE_DELAY_MS="${N42_MIN_PROPOSE_DELAY_MS:-0}" \
    N42_SKIP_TX_VERIFY="${N42_SKIP_TX_VERIFY:-0}" \
    N42_POOL_VALIDATION_THREADS="${N42_POOL_VALIDATION_THREADS:-2}" \
    N42_LOW_MEMORY="${N42_LOW_MEMORY:-0}" \
    N42_INJECT_PORT="${N42_INJECT_PORT:+$(( N42_INJECT_PORT + i ))}" \
    "$BINARY" node \
        --chain "$GENESIS_FILE" \
        --datadir "$datadir" \
        --http \
        --http.port "$http_port" \
        --http.api eth,net,web3,txpool,rpc \
        --http.corsdomain "*" \
        --ws \
        --ws.port "$ws_port" \
        --ws.api eth,net,web3 \
        --authrpc.port "$auth_port" \
        --port "$p2p_port" \
        --discovery.port "$p2p_port" \
        --log.file.directory "$datadir/logs" \
        --ipcdisable \
        --max-outbound-peers "$NUM_VALIDATORS" \
        --max-inbound-peers "$NUM_VALIDATORS" \
        --metrics "127.0.0.1:$metrics_port" \
        --builder.gaslimit "${N42_GAS_LIMIT:-2000000000}" \
        --builder.interval 50ms \
        --rpc.max-request-size 50 \
        --rpc.max-response-size 50 \
        --disable-tx-gossip \
        ${low_mem_flags} \
        ${p2p_key_flag} \
        ${TRUSTED_PEERS_FLAG} \
        > "$log_file" 2>&1 &

    NODE_PIDS+=($!)

    [[ "$NUM_VALIDATORS" -gt 1 ]] && \
        ENODES+=("enode://${P2P_PUBKEYS[$i]}@127.0.0.1:${p2p_port}")

    if [[ "$NODE_START_INTERVAL" -gt 0 ]] && [[ $i -lt $((NUM_VALIDATORS - 1)) ]]; then
        sleep "$NODE_START_INTERVAL"
    fi
done

# Write PIDs for monitor.sh
( IFS=,; echo "${NODE_PIDS[*]}" ) > "$DATA_DIR/node-pids.txt"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
log "===== $NUM_VALIDATORS validator node(s) running ====="
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    printf "  v%-2d  http://127.0.0.1:%d  log: %s/validator-%d.log\n" \
        "$i" "$((BASE_HTTP_RPC + i))" "$DATA_DIR" "$i"
done
echo ""
info "Check block number:"
echo "  curl -s http://127.0.0.1:$BASE_HTTP_RPC -X POST -H 'Content-Type: application/json' \\"
echo "    -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}'"
echo ""
log "Waiting for block production (up to ${MAX_BLOCK_WAIT}s)..."
log "Press Ctrl+C to stop all nodes."
echo ""

waited=0
while [[ $waited -lt $MAX_BLOCK_WAIT ]]; do
    block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:${BASE_HTTP_RPC}/" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null \
        || echo "0x0")
    block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
    if [[ $block_num -gt 0 ]]; then
        log "Chain active! Block $block_num produced."
        break
    fi
    sleep 3
    waited=$(( waited + 3 ))
    [[ $(( waited % 15 )) -eq 0 ]] && info "  Still waiting... (${waited}s / ${MAX_BLOCK_WAIT}s)"
done

if [[ $waited -ge $MAX_BLOCK_WAIT ]]; then
    warn "Timeout waiting for blocks. Check: tail -f $DATA_DIR/validator-0.log"
fi

# ---------------------------------------------------------------------------
# Watchdog
# ---------------------------------------------------------------------------
watchdog_interval=10
[[ "$NUM_VALIDATORS" -gt 10 ]] && watchdog_interval=15

while true; do
    alive=0
    for pid in "${NODE_PIDS[@]}"; do
        kill -0 "$pid" 2>/dev/null && alive=$(( alive + 1 ))
    done

    if [[ $alive -eq 0 ]]; then
        err "All nodes have exited. Check $DATA_DIR/validator-*.log"
        exit 1
    elif [[ $alive -lt $NUM_VALIDATORS ]]; then
        warn "Only $alive/$NUM_VALIDATORS nodes alive"
    fi

    sleep "$watchdog_interval"
done
