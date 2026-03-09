#!/bin/bash
# Step stress test: gradually increase load to find TPS ceiling.
# Each step runs for STEP_DURATION seconds, then ramps up.

RPC=${RPC:-http://127.0.0.1:18545}
STEP_DURATION=${STEP_DURATION:-60}
STRESS_BIN=${STRESS_BIN:-target/release/n42-stress}
LOG_DIR="/Users/jieliu/n42-testnet-data"

echo "=== N42 Step Stress Test ==="
echo "RPC: $RPC"
echo "Step duration: ${STEP_DURATION}s"
echo ""

# Steps: (accounts, batch_size, concurrency, prefill, label)
STEPS=(
    "200 50 128 5000 step1_low"
    "400 100 256 10000 step2_medium"
    "600 100 512 20000 step3_high"
    "800 200 768 30000 step4_very_high"
    "1000 200 1024 40000 step5_max"
)

get_block_number() {
    curl -s -X POST "$RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        | python3 -c "import sys,json; print(int(json.load(sys.stdin)['result'],16))" 2>/dev/null
}

get_pool_size() {
    curl -s -X POST "$RPC" -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"txpool_status","params":[],"id":1}' \
        | python3 -c "import sys,json; r=json.load(sys.stdin)['result']; print(int(r['pending'],16), int(r['queued'],16))" 2>/dev/null
}

for step_params in "${STEPS[@]}"; do
    read -r accounts batch concurrency prefill label <<< "$step_params"

    echo "=============================================="
    echo "STEP: $label"
    echo "  accounts=$accounts batch=$batch concurrency=$concurrency prefill=$prefill"
    echo "=============================================="

    START_BLOCK=$(get_block_number)
    START_TIME=$(date +%s)

    echo "[$(date +%H:%M:%S)] Starting stress (block $START_BLOCK)..."

    $STRESS_BIN \
        --step --duration "$STEP_DURATION" \
        --accounts "$accounts" --batch-size "$batch" \
        --concurrency "$concurrency" --prefill "$prefill" \
        --rpc "$RPC" 2>&1 &
    STRESS_PID=$!

    # Monitor during stress
    while kill -0 $STRESS_PID 2>/dev/null; do
        sleep 10
        CURRENT_BLOCK=$(get_block_number)
        POOL=$(get_pool_size)
        echo "[$(date +%H:%M:%S)] block=$CURRENT_BLOCK pool=($POOL)"
    done

    wait $STRESS_PID 2>/dev/null

    END_BLOCK=$(get_block_number)
    END_TIME=$(date +%s)
    ELAPSED=$((END_TIME - START_TIME))
    BLOCKS=$((END_BLOCK - START_BLOCK))

    echo "[$(date +%H:%M:%S)] Step complete: blocks=$BLOCKS elapsed=${ELAPSED}s"

    # Extract peak TPS from logs
    echo ""
    echo "--- Slot commits during this step ---"
    cat "$LOG_DIR"/validator-0/logs/4242/reth.log* 2>/dev/null \
        | grep "slot_notifier.*elapsed_ms" \
        | tail -5

    echo ""
    echo "--- Largest blocks ---"
    cat "$LOG_DIR"/validator-0/logs/4242/reth.log* 2>/dev/null \
        | grep "N42_PAYLOAD_PACK" \
        | sort -t= -k3 -n \
        | tail -3

    echo ""

    # Drain pool between steps
    echo "Draining pool for 15s..."
    sleep 15
    POOL=$(get_pool_size)
    echo "Pool after drain: ($POOL)"
    echo ""
done

echo "=== Step stress test complete ==="

# Final summary: extract all DIRECT_PUSH and DECOMPRESS for latency analysis
echo ""
echo "=== Network Latency Summary ==="
echo "Leader DIRECT_PUSH:"
cat "$LOG_DIR"/validator-0/logs/4242/reth.log* 2>/dev/null | grep "N42_DIRECT_PUSH" | tail -10
echo ""
echo "Follower DECOMPRESS:"
cat "$LOG_DIR"/validator-1/logs/4242/reth.log* 2>/dev/null | grep "N42_DECOMPRESS" | tail -10
