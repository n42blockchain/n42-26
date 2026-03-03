#!/usr/bin/env bash
#
# 77-node 10-minute stress test with 8s block interval
# Automatically collects results and shuts down
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
NUM_VALIDATORS=77
BLOCK_INTERVAL_MS=8000
TEST_DURATION=600  # 10 minutes
DATA_DIR="$HOME/n42-testnet-data-77"
BASE_HTTP_RPC=18545

# Sampling nodes for progress reporting
SAMPLE_MID=$((NUM_VALIDATORS / 2))
SAMPLE_LAST=$((NUM_VALIDATORS - 1))

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${GREEN}[77node-test]${NC} $*"; }
warn() { echo -e "${YELLOW}[77node-test]${NC} $*"; }
err()  { echo -e "${RED}[77node-test]${NC} $*"; }
info() { echo -e "${CYAN}[77node-test]${NC} $*"; }

# Raise fd limit — 77 nodes need many fds
ulimit -n 65536 2>/dev/null || true

# Clean previous data
log "Cleaning previous test data..."
rm -rf "$DATA_DIR"

TESTNET_LOG="$HOME/testnet-77node.log"

# Start testnet in background
log "Starting 77-node testnet (8s block interval)..."
bash "$SCRIPT_DIR/testnet.sh" \
    --nodes 77 \
    --block-interval 8000 \
    --clean \
    --no-explorer \
    --no-tx-gen \
    --no-mobile-sim \
    --no-monitor \
    > "$TESTNET_LOG" 2>&1 &

TESTNET_PID=$!
log "Testnet launcher PID: $TESTNET_PID"

cleanup() {
    log "Stopping testnet..."
    kill -- -$TESTNET_PID 2>/dev/null || kill $TESTNET_PID 2>/dev/null || true
    pkill -f "n42-testnet-data-77" 2>/dev/null || true
    wait 2>/dev/null || true
    log "Cleanup done."
}
trap cleanup EXIT

# Wait for testnet to be ready (blocks produced)
# 77 nodes: startup_delay = 77*1000+60000 = 137s, plus node launch time (~77s)
log "Waiting for first block..."
STARTUP_WAIT=0
MAX_STARTUP_WAIT=600  # 10 minutes max for 77 nodes to start
while [[ $STARTUP_WAIT -lt $MAX_STARTUP_WAIT ]]; do
    if ! kill -0 $TESTNET_PID 2>/dev/null; then
        err "Testnet process died during startup! Check: tail -100 $TESTNET_LOG"
        tail -50 "$TESTNET_LOG" 2>/dev/null || true
        exit 1
    fi

    block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:$BASE_HTTP_RPC/" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

    block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
    if [[ $block_num -gt 0 ]]; then
        log "First block produced! Block #$block_num"
        break
    fi

    sleep 5
    STARTUP_WAIT=$((STARTUP_WAIT + 5))
    [[ $((STARTUP_WAIT % 30)) -eq 0 ]] && info "  Still waiting for blocks... (${STARTUP_WAIT}s / ${MAX_STARTUP_WAIT}s)"
done

if [[ $STARTUP_WAIT -ge $MAX_STARTUP_WAIT ]]; then
    err "Timeout waiting for first block after ${MAX_STARTUP_WAIT}s"
    err "Check logs: tail -100 $DATA_DIR/validator-0.log"
    tail -30 "$DATA_DIR/validator-0.log" 2>/dev/null || true
    exit 1
fi

# Record initial block numbers (sample a few to be fast)
log "Recording initial state..."
INITIAL_V0=$(curl -s --max-time 3 \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://127.0.0.1:$BASE_HTTP_RPC/" 2>/dev/null | \
    python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "0")
log "Initial block: v0=$INITIAL_V0"

# Run for TEST_DURATION seconds, reporting progress every 30s
log "=== 10-minute test started ==="
START_TIME=$(date +%s)
ELAPSED=0
LAST_REPORT=0
REPORT_INTERVAL=30

while [[ $ELAPSED -lt $TEST_DURATION ]]; do
    sleep 10
    ELAPSED=$(( $(date +%s) - START_TIME ))

    if ! kill -0 $TESTNET_PID 2>/dev/null; then
        err "Testnet process died during test at ${ELAPSED}s!"
        break
    fi

    if [[ $((ELAPSED - LAST_REPORT)) -ge $REPORT_INTERVAL ]]; then
        LAST_REPORT=$ELAPSED

        blocks_v0=$(curl -s --max-time 3 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
            "http://127.0.0.1:$BASE_HTTP_RPC/" 2>/dev/null | \
            python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "?")

        blocks_mid=$(curl -s --max-time 3 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
            "http://127.0.0.1:$((BASE_HTTP_RPC + SAMPLE_MID))/" 2>/dev/null | \
            python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "?")

        blocks_last=$(curl -s --max-time 3 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
            "http://127.0.0.1:$((BASE_HTTP_RPC + SAMPLE_LAST))/" 2>/dev/null | \
            python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "?")

        remaining=$((TEST_DURATION - ELAPSED))
        info "[${ELAPSED}s/${TEST_DURATION}s] Blocks: v0=$blocks_v0 v${SAMPLE_MID}=$blocks_mid v${SAMPLE_LAST}=$blocks_last (${remaining}s remaining)"
    fi
done

log "=== Test complete! Collecting results... ==="

# Collect final block numbers from ALL nodes
echo ""
log "Final block numbers for all $NUM_VALIDATORS nodes:"
FINAL_BLOCKS=()
RESPONSIVE=0
MIN_BLOCK=999999
MAX_BLOCK=0
TOTAL_BLOCK=0

for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    port=$((BASE_HTTP_RPC + i))
    b=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:$port/" 2>/dev/null | \
        python3 -c "import sys,json; print(int(json.load(sys.stdin).get('result','0x0'),16))" 2>/dev/null || echo "-1")

    FINAL_BLOCKS+=("$b")

    if [[ "$b" != "-1" ]]; then
        RESPONSIVE=$((RESPONSIVE + 1))
        [[ $b -lt $MIN_BLOCK ]] && MIN_BLOCK=$b
        [[ $b -gt $MAX_BLOCK ]] && MAX_BLOCK=$b
        TOTAL_BLOCK=$((TOTAL_BLOCK + b))
    fi

    # Print in columns of 7
    col=$((i % 7))
    if [[ $col -eq 0 ]]; then
        printf "\n  "
    fi
    if [[ "$b" == "-1" ]]; then
        printf "v%-2d:${RED}DOWN${NC}  " "$i"
    else
        printf "v%-2d:%-5d " "$i" "$b"
    fi
done
echo ""
echo ""

# Check for error patterns in logs
log "Checking logs for errors..."
ERROR_COUNT=0
PANIC_COUNT=0
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    logfile="$DATA_DIR/validator-${i}.log"
    if [[ -f "$logfile" ]]; then
        panics=$(grep -ci "panic\|fatal" "$logfile" 2>/dev/null || echo "0")
        PANIC_COUNT=$((PANIC_COUNT + panics))
    fi
done

# Check node process health
ALIVE=0
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    b=$(curl -s --max-time 2 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:$((BASE_HTTP_RPC + i))/" 2>/dev/null | \
        python3 -c "import sys,json; r=json.load(sys.stdin).get('result'); print('1' if r else '0')" 2>/dev/null || echo "0")
    [[ "$b" == "1" ]] && ALIVE=$((ALIVE + 1))
done

# Summary
echo ""
log "========================================="
log "  77-NODE TEST RESULTS (10 minutes, 8s slots)"
log "========================================="
echo ""

if [[ $RESPONSIVE -gt 0 ]]; then
    AVG_BLOCK=$((TOTAL_BLOCK / RESPONSIVE))
    BLOCK_DIFF=$((MAX_BLOCK - MIN_BLOCK))
    EXPECTED_BLOCKS=$((TEST_DURATION / (BLOCK_INTERVAL_MS / 1000)))

    info "  Nodes responsive:    $RESPONSIVE / $NUM_VALIDATORS"
    info "  Nodes alive:         $ALIVE / $NUM_VALIDATORS"
    info "  Block range:         $MIN_BLOCK - $MAX_BLOCK (diff: $BLOCK_DIFF)"
    info "  Average block:       $AVG_BLOCK"
    info "  Expected blocks:     ~$EXPECTED_BLOCKS (with 8s interval)"
    info "  Panic/fatal entries: $PANIC_COUNT"
    echo ""

    PASS=true

    if [[ $ALIVE -lt $NUM_VALIDATORS ]]; then
        err "FAIL: Only $ALIVE/$NUM_VALIDATORS nodes alive"
        PASS=false
    fi

    if [[ $MAX_BLOCK -eq 0 ]]; then
        err "FAIL: No blocks produced!"
        PASS=false
    fi

    if [[ $BLOCK_DIFF -gt 3 ]]; then
        warn "WARN: Block height divergence > 3 ($BLOCK_DIFF)"
    fi

    if [[ $PANIC_COUNT -gt 0 ]]; then
        err "FAIL: $PANIC_COUNT panic/fatal entries in logs"
        PASS=false
    fi

    HALF_EXPECTED=$((EXPECTED_BLOCKS / 2))
    if [[ $MAX_BLOCK -lt $HALF_EXPECTED ]]; then
        err "FAIL: Block production too slow ($MAX_BLOCK < $HALF_EXPECTED expected minimum)"
        PASS=false
    fi

    echo ""
    if [[ "$PASS" == true ]]; then
        log "${GREEN}===== TEST PASSED =====${NC}"
    else
        err "===== TEST FAILED ====="
    fi
else
    err "FAIL: No nodes responded!"
fi

echo ""
log "Logs directory: $DATA_DIR/"
log "Full testnet log: $TESTNET_LOG"
