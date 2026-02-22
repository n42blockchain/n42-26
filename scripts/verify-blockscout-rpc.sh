#!/usr/bin/env bash
#
# Verify that an N42 node exposes the RPC methods required by Blockscout.
#
# Usage:
#   ./scripts/verify-blockscout-rpc.sh [RPC_URL] [WS_URL]
#
# Defaults:
#   RPC_URL = http://127.0.0.1:8545
#   WS_URL  = ws://127.0.0.1:8546

set -euo pipefail

RPC_URL="${1:-http://127.0.0.1:8545}"
WS_URL="${2:-ws://127.0.0.1:8546}"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASS=0
FAIL=0
WARN=0

check_rpc() {
    local name="$1"
    local method="$2"
    local params="$3"
    local required="${4:-true}"

    local payload="{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}"
    local response

    response=$(curl -sf -X POST -H 'Content-Type: application/json' \
        --max-time 5 -d "${payload}" "${RPC_URL}" 2>/dev/null) || {
        if [ "$required" = "true" ]; then
            echo -e "  ${RED}FAIL${NC} ${name} (${method}) - connection failed"
            FAIL=$((FAIL + 1))
        else
            echo -e "  ${YELLOW}WARN${NC} ${name} (${method}) - connection failed"
            WARN=$((WARN + 1))
        fi
        return
    }

    if echo "$response" | grep -q '"error"'; then
        local error_code
        error_code=$(echo "$response" | grep -o '"code":[0-9-]*' | head -1 | cut -d: -f2)
        if [ "$error_code" = "-32601" ]; then
            if [ "$required" = "true" ]; then
                echo -e "  ${RED}FAIL${NC} ${name} (${method}) - method not found"
                FAIL=$((FAIL + 1))
            else
                echo -e "  ${YELLOW}WARN${NC} ${name} (${method}) - method not found (optional)"
                WARN=$((WARN + 1))
            fi
        else
            echo -e "  ${GREEN}PASS${NC} ${name} (${method}) - method available"
            PASS=$((PASS + 1))
        fi
    else
        echo -e "  ${GREEN}PASS${NC} ${name} (${method}) - OK"
        PASS=$((PASS + 1))
    fi
}

echo "============================================"
echo " N42 Blockscout RPC Compatibility Checker"
echo "============================================"
echo ""
echo "RPC endpoint: ${RPC_URL}"
echo "WS  endpoint: ${WS_URL}"
echo ""

echo "--- Core eth_* methods (required) ---"
check_rpc "Block number"          "eth_blockNumber"         "[]"
check_rpc "Chain ID"              "eth_chainId"             "[]"
check_rpc "Get block by number"   "eth_getBlockByNumber"    "[\"0x0\",true]"
check_rpc "Get block by hash"     "eth_getBlockByHash"      "[\"0x0000000000000000000000000000000000000000000000000000000000000000\",true]"
check_rpc "Get tx receipt"        "eth_getTransactionReceipt" "[\"0x0000000000000000000000000000000000000000000000000000000000000000\"]"
check_rpc "Get logs"              "eth_getLogs"             "[{\"fromBlock\":\"0x0\",\"toBlock\":\"0x0\"}]"
check_rpc "Get balance"           "eth_getBalance"          "[\"0x0000000000000000000000000000000000000000\",\"latest\"]"
check_rpc "Get code"              "eth_getCode"             "[\"0x0000000000000000000000000000000000000000\",\"latest\"]"
check_rpc "Gas price"             "eth_gasPrice"            "[]"
echo ""

echo "--- Debug methods (required for Blockscout internal txs) ---"
check_rpc "Trace transaction"     "debug_traceTransaction"    "[\"0x0000000000000000000000000000000000000000000000000000000000000000\",{\"tracer\":\"callTracer\"}]"
check_rpc "Trace block by number" "debug_traceBlockByNumber"  "[\"0x0\",{\"tracer\":\"callTracer\"}]"
echo ""

echo "--- Net/Web3 methods (required) ---"
check_rpc "Net version"           "net_version"             "[]"
check_rpc "Net peer count"        "net_peerCount"           "[]"
check_rpc "Web3 client version"   "web3_clientVersion"      "[]"
echo ""

echo "--- Trace methods (optional) ---"
check_rpc "Trace block"           "trace_block"             "[\"0x0\"]"                "false"
check_rpc "Trace replay block"    "trace_replayBlockTransactions" "[\"0x0\",[\"trace\"]]"  "false"
echo ""

echo "--- Txpool methods (optional) ---"
check_rpc "Txpool content"        "txpool_content"          "[]"                       "false"
check_rpc "Txpool status"         "txpool_status"           "[]"                       "false"
echo ""

echo "--- N42 custom methods ---"
check_rpc "Consensus status"      "n42_consensusStatus"     "[]"                       "false"
check_rpc "Validator set"         "n42_validatorSet"        "[]"                       "false"
echo ""

echo "--- WebSocket connectivity ---"
if command -v websocat &> /dev/null; then
    ws_response=$(echo '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | \
        websocat --no-close -t "${WS_URL}" 2>/dev/null | head -1) || true
    if [ -n "$ws_response" ] && echo "$ws_response" | grep -q '"result"'; then
        echo -e "  ${GREEN}PASS${NC} WebSocket connection - OK"
        PASS=$((PASS + 1))
    else
        echo -e "  ${RED}FAIL${NC} WebSocket connection - no valid response"
        FAIL=$((FAIL + 1))
    fi
else
    echo -e "  ${YELLOW}SKIP${NC} WebSocket test (install websocat: cargo install websocat)"
    WARN=$((WARN + 1))
fi
echo ""

echo "============================================"
TOTAL=$((PASS + FAIL + WARN))
echo -e " Results: ${GREEN}${PASS} passed${NC}, ${RED}${FAIL} failed${NC}, ${YELLOW}${WARN} warnings${NC} / ${TOTAL} total"
if [ "$FAIL" -eq 0 ]; then
    echo -e " ${GREEN}Node is Blockscout-compatible!${NC}"
else
    echo -e " ${RED}Node is missing required RPC methods for Blockscout.${NC}"
    echo -e " Ensure the node is started with: --http.api \"eth,net,web3,debug,trace,txpool\""
fi
echo "============================================"

exit "$FAIL"
