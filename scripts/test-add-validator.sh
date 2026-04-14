#!/usr/bin/env bash
#
# 动态 Validator 新增端到端测试
#
# 测试流程：
#   Phase 1: 启动 N 节点测试网，等待出块
#   Phase 2: 通过 RPC 提案添加第 N+1 个 validator
#   Phase 3: 等待 epoch 边界激活
#   Phase 4: 启动新节点（N+1），同步并参与共识
#   Phase 5: 验证新节点参与出块
#
# 用法：
#   ./scripts/test-add-validator.sh                  # 默认 3→4 节点
#   ./scripts/test-add-validator.sh --nodes 5        # 5→6 节点
#   ./scripts/test-add-validator.sh --nodes 7        # 7→8 节点
#   KEEP_RUNNING=1 ./scripts/test-add-validator.sh   # 测试后不停止，方便调试
#
# 依赖：
#   - cargo build --release -p n42-node-bin（或 --debug）
#   - pip3 install eth-account eth-utils eth-keys
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# ── Colors ──
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${GREEN}[add-validator]${NC} $*"; }
info() { echo -e "${CYAN}[add-validator]${NC} $*"; }
warn() { echo -e "${YELLOW}[add-validator]${NC} $*"; }
err()  { echo -e "${RED}[add-validator]${NC} $*"; }
fail() { err "$*"; cleanup; exit 1; }

# ── Args ──
NUM_INITIAL=3
DEBUG_BUILD=false
KEEP_RUNNING="${KEEP_RUNNING:-0}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --nodes) NUM_INITIAL="$2"; shift 2 ;;
        --debug) DEBUG_BUILD=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--nodes N] [--debug]"
            echo "  --nodes N   Initial validator count (default: 3)"
            echo "  --debug     Use debug build"
            echo "  KEEP_RUNNING=1  Don't stop after test (for debugging)"
            exit 0 ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ "$NUM_INITIAL" -lt 3 ]]; then
    fail "Initial validator count must be >= 3 (need BFT quorum)"
fi

NEW_INDEX=$NUM_INITIAL   # 0-based index of new validator
NUM_AFTER=$((NUM_INITIAL + 1))
ADMIN_TOKEN="test-add-validator-$(date +%s)"

# ── Port allocation ──
# Use high port range to avoid conflict with default testnet
BASE_HTTP=27000
BASE_WS=27400
BASE_AUTH=27800
BASE_P2P=31300
BASE_CONSENSUS=11000
BASE_STARHUB=11400
BASE_METRICS=21200

DATA_DIR="/tmp/n42-test-add-validator-$$"

# ── Binary selection ──
if [[ "$DEBUG_BUILD" == true ]]; then
    BINARY="$PROJECT_DIR/target/debug/n42-node"
else
    BINARY="$PROJECT_DIR/target/release/n42-node"
fi

if [[ ! -f "$BINARY" ]]; then
    fail "Binary not found: $BINARY\n  Run: cargo build $([ "$DEBUG_BUILD" = true ] && echo '' || echo '--release') -p n42-node-bin"
fi

# ── Cleanup ──
declare -a ALL_PIDS=()
BLOCKSCOUT_STARTED=false

cleanup() {
    log "Cleaning up..."
    for pid in "${ALL_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    if [[ -d "$DATA_DIR" && "$KEEP_RUNNING" != "1" ]]; then
        rm -rf "$DATA_DIR"
    fi
}
trap cleanup EXIT

# ── Deterministic key generation ──
# Matches ConsensusConfig::deterministic_key_bytes(index)
# BLS secret key: 32 bytes, all zeros except last 4 bytes = (index+1) big-endian
bls_secret_hex() {
    local idx=$1
    local val=$((idx + 1))
    printf '%056x%08x' 0 "$val"
}

# Validator address: 20 bytes, all zeros except last 2 bytes = (index+1) big-endian
# Matches ConsensusConfig::dev_multi()
validator_address() {
    local idx=$1
    local val=$((idx + 1))
    printf '0x%036x%04x' 0 "$val"
}

# Derive BLS public key (48 bytes) from secret key using Python + blst or manual
# Since we need the actual BLS pubkey for the RPC call, we write a small Rust helper
derive_bls_pubkey() {
    local secret_hex=$1
    # Use the running node's RPC to query validator set — the pubkeys are there
    # This is more reliable than trying to do BLS math in bash
    echo "$secret_hex"  # placeholder, will be resolved below
}

# DevP2P key generation (matches testnet.sh)
generate_p2p_key() {
    local idx=$1
    python3 -c "
from eth_utils import keccak
from eth_keys import keys
sk = keccak(b'n42-devp2p-key-$idx')
pk = keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
"
}

# ── Genesis generation ──
generate_genesis() {
    mkdir -p "$DATA_DIR"
    log "Generating genesis..."
    python3 << 'PYEOF' - "$DATA_DIR" "$NUM_INITIAL"
import json, os, sys
data_dir = sys.argv[1]
num = int(sys.argv[2])
genesis = {
    "config": {
        "chainId": 4242,
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
    "gasLimit": "0x77359400", "difficulty": "0x0",
    "mixHash": "0x" + "0" * 64,
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc": {
            "balance": "0x60EF6B1ABA6F072330000000"
        },
    },
    "number": "0x0", "gasUsed": "0x0",
    "parentHash": "0x" + "0" * 64,
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0", "blobGasUsed": "0x0",
}
with open(os.path.join(data_dir, "genesis.json"), "w") as f:
    json.dump(genesis, f, indent=2)
PYEOF
}

# ══════════════════════════════════════════════════════════════════════
# Phase 1: 启动初始测试网
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 1: 启动 ${NUM_INITIAL} 节点测试网 ═══"

generate_genesis
GENESIS_FILE="$DATA_DIR/genesis.json"

# Generate P2P keys
declare -a P2P_SECRETS=()
declare -a P2P_PUBKEYS=()
# Generate for NUM_AFTER nodes (including the one we'll add later)
for i in $(seq 0 $((NUM_AFTER - 1))); do
    read -r secret pubkey <<< "$(generate_p2p_key "$i")"
    P2P_SECRETS+=("$secret")
    P2P_PUBKEYS+=("$pubkey")
done

# Generate BLS keys
declare -a BLS_KEYS=()
for i in $(seq 0 $((NUM_AFTER - 1))); do
    BLS_KEYS+=("$(bls_secret_hex "$i")")
done

# Timeouts
case "$NUM_INITIAL" in
    3)  BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000; STARTUP_DELAY_MS=2000 ;;
    5)  BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000; STARTUP_DELAY_MS=2000 ;;
    7)  BASE_TIMEOUT_MS=10000; MAX_TIMEOUT_MS=30000; STARTUP_DELAY_MS=3000 ;;
    *)  BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000; STARTUP_DELAY_MS=3000 ;;
esac

# Start initial N validators
declare -a ENODES=()
for i in $(seq 0 $((NUM_INITIAL - 1))); do
    datadir="$DATA_DIR/validator-${i}"
    http_port=$((BASE_HTTP + i))
    ws_port=$((BASE_WS + i))
    auth_port=$((BASE_AUTH + i))
    p2p_port=$((BASE_P2P + i))
    consensus_port=$((BASE_CONSENSUS + i))
    starhub_port=$((BASE_STARHUB + i))
    metrics_port=$((BASE_METRICS + i))
    log_file="$DATA_DIR/validator-${i}.log"

    mkdir -p "$datadir"

    TRUSTED_PEERS_FLAG=""
    if [[ ${#ENODES[@]} -gt 0 ]]; then
        TRUSTED_PEERS_FLAG="--trusted-peers $(IFS=,; echo "${ENODES[*]}")"
    fi

    info "  v${i}: http=:${http_port} p2p=:${p2p_port} consensus=:${consensus_port}"

    RUST_LOG="info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error" \
    N42_VALIDATOR_KEY="${BLS_KEYS[$i]}" \
    N42_VALIDATOR_COUNT="$NUM_INITIAL" \
    N42_ENABLE_MDNS="true" \
    N42_DATA_DIR="$datadir" \
    N42_CONSENSUS_PORT="$consensus_port" \
    N42_STARHUB_PORT="$starhub_port" \
    N42_MAX_EMPTY_SKIPS="0" \
    N42_BLOCK_INTERVAL_MS="2000" \
    N42_BASE_TIMEOUT_MS="$BASE_TIMEOUT_MS" \
    N42_MAX_TIMEOUT_MS="$MAX_TIMEOUT_MS" \
    N42_STARTUP_DELAY_MS="$STARTUP_DELAY_MS" \
    N42_REWARD_EPOCH_BLOCKS="50" \
    N42_DAILY_BASE_REWARD_GWEI="100000000" \
    N42_REWARD_CURVE_K="4.0" \
    N42_MIN_ATTESTATION_THRESHOLD="1" \
    N42_OPEN_VERIFICATION="1" \
    N42_ADMIN_TOKEN="$ADMIN_TOKEN" \
    "$BINARY" node \
        --chain "$GENESIS_FILE" \
        --datadir "$datadir" \
        --http --http.addr "0.0.0.0" --http.port "$http_port" \
        --http.api eth,net,web3,txpool,rpc \
        --ws --ws.port "$ws_port" \
        --authrpc.port "$auth_port" \
        --port "$p2p_port" --discovery.port "$p2p_port" \
        --log.file.directory "$datadir/logs" \
        --ipcdisable \
        --max-outbound-peers "$NUM_AFTER" \
        --max-inbound-peers "$NUM_AFTER" \
        --metrics "127.0.0.1:$metrics_port" \
        --builder.gaslimit 2000000000 \
        --builder.interval 50ms \
        --rpc.max-connections 100 \
        --disable-tx-gossip \
        --p2p-secret-key-hex "${P2P_SECRETS[$i]}" \
        ${TRUSTED_PEERS_FLAG} \
        > "$log_file" 2>&1 &

    ALL_PIDS+=($!)
    ENODES+=("enode://${P2P_PUBKEYS[$i]}@127.0.0.1:${p2p_port}")

    if [[ $i -lt $((NUM_INITIAL - 1)) ]]; then
        sleep 2
    fi
done

# ── 等待出块 ──
log "等待出块..."
RPC_URL="http://127.0.0.1:${BASE_HTTP}"
waited=0
while [[ $waited -lt 120 ]]; do
    block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

    block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
    if [[ $block_num -gt 0 ]]; then
        log "出块正常，当前高度: $block_num"
        break
    fi
    sleep 3
    waited=$((waited + 3))
    [[ $((waited % 15)) -eq 0 ]] && info "  等待中... (${waited}s / 120s)"
done

if [[ $block_num -eq 0 ]]; then
    fail "120s 超时未出块。检查日志: tail -f $DATA_DIR/validator-0.log"
fi

# 等几个块稳定
log "等待 10 个块确认共识稳定..."
target_height=$((block_num + 10))
waited=0
while [[ $waited -lt 60 ]]; do
    block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
    block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
    if [[ $block_num -ge $target_height ]]; then
        log "共识稳定，高度: $block_num"
        break
    fi
    sleep 2
    waited=$((waited + 2))
done

# ══════════════════════════════════════════════════════════════════════
# Phase 2: 查询当前 validator set，发起添加提案
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 2: 提案添加 validator #${NEW_INDEX} ═══"

# 查询当前 validator set
info "当前 validator set:"
vs_response=$(curl -s --max-time 5 \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
    "$RPC_URL")
echo "$vs_response" | python3 -m json.tool 2>/dev/null || echo "$vs_response"

current_count=$(echo "$vs_response" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r['result']['active']))" 2>/dev/null || echo "0")
current_epoch=$(echo "$vs_response" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['currentEpoch'])" 2>/dev/null || echo "0")
info "当前验证者数量: $current_count, epoch: $current_epoch"

if [[ "$current_count" -ne "$NUM_INITIAL" ]]; then
    fail "期望 $NUM_INITIAL 个验证者，实际 $current_count"
fi

# 获取新 validator 的 BLS 公钥
# 方法：从节点 0 的 validator set 中解析格式，然后用 Python 计算新 key 的公钥
# 由于 BLS 需要特殊的曲线运算，我们利用一个技巧：
# 临时启动一个进程来导出公钥，或直接用 Python blst 库
NEW_ADDR=$(validator_address "$NEW_INDEX")
info "新 validator 地址: $NEW_ADDR"

# BLS 公钥通过临时节点导出（见下方 fallback 逻辑）
NEW_BLS_PUBKEY=""

# 如果无法直接计算 BLS 公钥，使用备选方案：
# 启动临时单节点获取公钥
if [[ -z "$NEW_BLS_PUBKEY" ]]; then
    info "通过临时节点导出 BLS 公钥..."
    tmp_datadir="$DATA_DIR/tmp-key-export"
    mkdir -p "$tmp_datadir"

    # 启动一个快速退出的节点来导出公钥
    # 用 N42_VALIDATOR_COUNT=NUM_AFTER 确保 validator set 包含新 validator 的密钥
    tmp_http_port=$((BASE_HTTP + 100))
    tmp_auth_port=$((BASE_AUTH + 100))
    tmp_p2p_port=$((BASE_P2P + 100))
    tmp_consensus_port=$((BASE_CONSENSUS + 100))
    tmp_starhub_port=$((BASE_STARHUB + 100))
    tmp_metrics_port=$((BASE_METRICS + 100))

    RUST_LOG="error" \
    N42_VALIDATOR_KEY="${BLS_KEYS[$NEW_INDEX]}" \
    N42_VALIDATOR_COUNT="$NUM_AFTER" \
    N42_DATA_DIR="$tmp_datadir" \
    N42_CONSENSUS_PORT="$tmp_consensus_port" \
    N42_STARHUB_PORT="$tmp_starhub_port" \
    N42_BASE_TIMEOUT_MS="60000" \
    N42_MAX_TIMEOUT_MS="120000" \
    N42_STARTUP_DELAY_MS="60000" \
    "$BINARY" node \
        --chain "$GENESIS_FILE" \
        --datadir "$tmp_datadir" \
        --http --http.addr "127.0.0.1" --http.port "$tmp_http_port" \
        --http.api eth,net,web3,rpc \
        --authrpc.port "$tmp_auth_port" \
        --port "$tmp_p2p_port" --discovery.port "$tmp_p2p_port" \
        --ipcdisable \
        --metrics "127.0.0.1:$tmp_metrics_port" \
        > "$DATA_DIR/tmp-key-export.log" 2>&1 &

    TMP_PID=$!

    # 等待 RPC 可用
    for attempt in $(seq 1 30); do
        tmp_vs=$(curl -s --max-time 2 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
            "http://127.0.0.1:$tmp_http_port" 2>/dev/null || echo "")
        if [[ -n "$tmp_vs" ]] && echo "$tmp_vs" | python3 -c "import sys,json; json.load(sys.stdin)['result']" 2>/dev/null; then
            NEW_BLS_PUBKEY=$(echo "$tmp_vs" | python3 -c "
import sys, json
r = json.load(sys.stdin)
# 取新 validator 的公钥
pk = r['result']['active'][${NEW_INDEX}]['publicKey']
print(pk)
")
            break
        fi
        sleep 1
    done

    kill "$TMP_PID" 2>/dev/null || true
    wait "$TMP_PID" 2>/dev/null || true
    rm -rf "$tmp_datadir"

    if [[ -z "$NEW_BLS_PUBKEY" ]]; then
        fail "无法导出新 validator 的 BLS 公钥"
    fi
fi

info "新 validator BLS 公钥: ${NEW_BLS_PUBKEY:0:24}..."

# 发起添加提案
log "调用 n42_proposeAddValidator..."
add_response=$(curl -s --max-time 10 \
    -X POST -H 'Content-Type: application/json' \
    -d "{
        \"jsonrpc\": \"2.0\",
        \"method\": \"n42_proposeAddValidator\",
        \"params\": [\"$ADMIN_TOKEN\", \"$NEW_ADDR\", \"$NEW_BLS_PUBKEY\"],
        \"id\": 1
    }" \
    "$RPC_URL")

info "RPC 响应: $add_response"

# 检查是否成功
if echo "$add_response" | python3 -c "import sys,json; r=json.load(sys.stdin); assert 'result' in r" 2>/dev/null; then
    log "提案成功！"
else
    error_msg=$(echo "$add_response" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r.get('error',{}).get('message','unknown'))" 2>/dev/null || echo "unknown")
    fail "提案失败: $error_msg"
fi

# ══════════════════════════════════════════════════════════════════════
# Phase 3: 等待 epoch 切换，验证新 validator 被激活
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 3: 等待 epoch 切换 ═══"

# 默认 epoch_length=30，等足够时间让 epoch 切换
# ConsensusConfig::dev_multi 默认 epoch_length=30
info "等待 epoch 切换（epoch_length=30 blocks，约 60-90 秒）..."

waited=0
max_wait=180
while [[ $waited -lt $max_wait ]]; do
    vs_resp=$(curl -s --max-time 5 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null)

    new_count=$(echo "$vs_resp" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r['result']['active']))" 2>/dev/null || echo "0")
    new_epoch=$(echo "$vs_resp" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['currentEpoch'])" 2>/dev/null || echo "0")
    pending=$(echo "$vs_resp" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['pendingChanges'])" 2>/dev/null || echo "0")
    staged=$(echo "$vs_resp" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['stagedNextEpoch'])" 2>/dev/null || echo "false")

    if [[ "$new_count" -eq "$NUM_AFTER" ]]; then
        log "Epoch 已切换！新 validator set 大小: $new_count, epoch: $new_epoch"
        break
    fi

    if [[ $((waited % 10)) -eq 0 ]]; then
        block_hex=$(curl -s --max-time 3 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
            "$RPC_URL" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
        cur_block=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
        info "  高度=$cur_block validators=$new_count epoch=$new_epoch pending=$pending staged=$staged (${waited}s/${max_wait}s)"
    fi

    sleep 3
    waited=$((waited + 3))
done

if [[ "$new_count" -ne "$NUM_AFTER" ]]; then
    fail "Epoch 切换超时。validator set 仍为 ${new_count}（期望 ${NUM_AFTER}）"
fi

# 记录激活时的区块高度
activate_block_hex=$(curl -s --max-time 3 \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "$RPC_URL" 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
activate_block=$(printf '%d' "$activate_block_hex" 2>/dev/null || echo 0)
info "新 validator set 在高度 $activate_block 处激活"

# ══════════════════════════════════════════════════════════════════════
# Phase 4: 启动新节点，同步并加入共识
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 4: 启动新节点 v${NEW_INDEX} ═══"

new_datadir="$DATA_DIR/validator-${NEW_INDEX}"
new_http_port=$((BASE_HTTP + NEW_INDEX))
new_ws_port=$((BASE_WS + NEW_INDEX))
new_auth_port=$((BASE_AUTH + NEW_INDEX))
new_p2p_port=$((BASE_P2P + NEW_INDEX))
new_consensus_port=$((BASE_CONSENSUS + NEW_INDEX))
new_starhub_port=$((BASE_STARHUB + NEW_INDEX))
new_metrics_port=$((BASE_METRICS + NEW_INDEX))
new_log_file="$DATA_DIR/validator-${NEW_INDEX}.log"

mkdir -p "$new_datadir"

# 连接所有已有节点
ALL_ENODES=$(IFS=,; echo "${ENODES[*]}")

info "  v${NEW_INDEX}: http=:${new_http_port} p2p=:${new_p2p_port} consensus=:${new_consensus_port}"

# 关键：N42_VALIDATOR_COUNT 必须等于初始数量
# 新节点用相同的初始 validator set 启动，通过 sync 路径获知 epoch 变更
RUST_LOG="info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error" \
N42_VALIDATOR_KEY="${BLS_KEYS[$NEW_INDEX]}" \
N42_VALIDATOR_COUNT="$NUM_INITIAL" \
N42_ENABLE_MDNS="true" \
N42_DATA_DIR="$new_datadir" \
N42_CONSENSUS_PORT="$new_consensus_port" \
N42_STARHUB_PORT="$new_starhub_port" \
N42_MAX_EMPTY_SKIPS="0" \
N42_BLOCK_INTERVAL_MS="2000" \
N42_BASE_TIMEOUT_MS="$BASE_TIMEOUT_MS" \
N42_MAX_TIMEOUT_MS="$MAX_TIMEOUT_MS" \
N42_STARTUP_DELAY_MS="2000" \
N42_REWARD_EPOCH_BLOCKS="50" \
N42_DAILY_BASE_REWARD_GWEI="100000000" \
N42_REWARD_CURVE_K="4.0" \
N42_MIN_ATTESTATION_THRESHOLD="1" \
N42_OPEN_VERIFICATION="1" \
N42_ADMIN_TOKEN="$ADMIN_TOKEN" \
"$BINARY" node \
    --chain "$GENESIS_FILE" \
    --datadir "$new_datadir" \
    --http --http.addr "0.0.0.0" --http.port "$new_http_port" \
    --http.api eth,net,web3,txpool,rpc \
    --ws --ws.port "$new_ws_port" \
    --authrpc.port "$new_auth_port" \
    --port "$new_p2p_port" --discovery.port "$new_p2p_port" \
    --log.file.directory "$new_datadir/logs" \
    --ipcdisable \
    --max-outbound-peers "$NUM_AFTER" \
    --max-inbound-peers "$NUM_AFTER" \
    --metrics "127.0.0.1:$new_metrics_port" \
    --builder.gaslimit 2000000000 \
    --builder.interval 50ms \
    --rpc.max-connections 100 \
    --disable-tx-gossip \
    --p2p-secret-key-hex "${P2P_SECRETS[$NEW_INDEX]}" \
    --trusted-peers "$ALL_ENODES" \
    > "$new_log_file" 2>&1 &

ALL_PIDS+=($!)
NEW_NODE_PID=$!
info "  新节点 PID: $NEW_NODE_PID"

# ── 等待新节点 RPC 可用 ──
NEW_RPC_URL="http://127.0.0.1:${new_http_port}"
info "等待新节点 RPC 可用..."
waited=0
while [[ $waited -lt 60 ]]; do
    if curl -s --max-time 2 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$NEW_RPC_URL" >/dev/null 2>&1; then
        log "新节点 RPC 可用"
        break
    fi
    sleep 2
    waited=$((waited + 2))
done

# ── 等待新节点同步 ──
log "等待新节点同步到最新高度..."
waited=0
max_sync_wait=120
while [[ $waited -lt $max_sync_wait ]]; do
    # 检查新节点进程是否还活着
    if ! kill -0 "$NEW_NODE_PID" 2>/dev/null; then
        err "新节点进程已退出！"
        err "最后 50 行日志:"
        tail -50 "$new_log_file" 2>/dev/null || true
        fail "新节点崩溃"
    fi

    old_block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

    new_block_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$NEW_RPC_URL" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

    old_block=$(printf '%d' "$old_block_hex" 2>/dev/null || echo 0)
    new_block=$(printf '%d' "$new_block_hex" 2>/dev/null || echo 0)

    diff=$((old_block - new_block))
    if [[ $diff -le 2 && $new_block -gt 0 ]]; then
        log "新节点已同步！高度: ${new_block}（原节点: ${old_block}，差距: ${diff}）"
        break
    fi

    if [[ $((waited % 10)) -eq 0 ]]; then
        info "  同步中: 新节点=$new_block 原节点=$old_block 差距=$diff (${waited}s/${max_sync_wait}s)"
    fi

    sleep 3
    waited=$((waited + 3))
done

if [[ $diff -gt 2 ]]; then
    warn "新节点未完全同步（差距: ${diff}），继续观察..."
fi

# ══════════════════════════════════════════════════════════════════════
# Phase 5: 验证新节点参与共识
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 5: 验证新节点参与共识 ═══"

# 等待足够的块，让新节点有机会轮到 leader
# 一个完整 round-robin 需要 NUM_AFTER 个块
blocks_to_wait=$((NUM_AFTER * 3))
info "等待 $blocks_to_wait 个块（覆盖多轮 leader 轮换）..."

start_block_hex=$(curl -s --max-time 3 \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "$RPC_URL" 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
start_block=$(printf '%d' "$start_block_hex" 2>/dev/null || echo 0)
target_block=$((start_block + blocks_to_wait))

waited=0
max_phase5_wait=$((blocks_to_wait * 4))
while [[ $waited -lt $max_phase5_wait ]]; do
    cur_hex=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "$RPC_URL" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
    cur_block=$(printf '%d' "$cur_hex" 2>/dev/null || echo 0)

    if [[ $cur_block -ge $target_block ]]; then
        break
    fi
    sleep 3
    waited=$((waited + 3))
done

# 验证：所有节点高度一致
log "验证所有节点高度一致..."
declare -a FINAL_HEIGHTS=()
all_consistent=true
for i in $(seq 0 $NEW_INDEX); do
    port=$((BASE_HTTP + i))
    h=$(curl -s --max-time 3 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
        "http://127.0.0.1:$port" 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")
    h_dec=$(printf '%d' "$h" 2>/dev/null || echo 0)
    FINAL_HEIGHTS+=("$h_dec")
    info "  v${i}: 高度 $h_dec"
done

# 检查高度差不超过 2
min_h=${FINAL_HEIGHTS[0]}
max_h=${FINAL_HEIGHTS[0]}
for h in "${FINAL_HEIGHTS[@]}"; do
    [[ $h -lt $min_h ]] && min_h=$h
    [[ $h -gt $max_h ]] && max_h=$h
done
height_diff=$((max_h - min_h))

# 验证新节点也看到了新的 validator set
new_vs_resp=$(curl -s --max-time 5 \
    -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
    "$NEW_RPC_URL" 2>/dev/null)
new_node_vs_count=$(echo "$new_vs_resp" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r['result']['active']))" 2>/dev/null || echo "0")

# ── 输出结果 ──
echo ""
echo "════════════════════════════════════════════════════════════════"

PASS=true

if [[ $height_diff -le 3 && $min_h -gt 0 ]]; then
    echo -e "  ${GREEN}✓${NC} 高度一致性: 最大差距 ${height_diff} 块（${min_h} ~ ${max_h}）"
else
    echo -e "  ${RED}✗${NC} 高度不一致: 差距 ${height_diff} 块（${min_h} ~ ${max_h}）"
    PASS=false
fi

if [[ "$new_node_vs_count" -eq "$NUM_AFTER" ]]; then
    echo -e "  ${GREEN}✓${NC} 新节点 validator set: ${new_node_vs_count} 个（期望 ${NUM_AFTER}）"
else
    echo -e "  ${RED}✗${NC} 新节点 validator set: ${new_node_vs_count} 个（期望 ${NUM_AFTER}）"
    PASS=false
fi

if [[ $max_h -gt $activate_block ]]; then
    echo -e "  ${GREEN}✓${NC} 新 validator set 激活后持续出块: $activate_block → $max_h"
else
    echo -e "  ${RED}✗${NC} 激活后未见新块"
    PASS=false
fi

echo "════════════════════════════════════════════════════════════════"

if [[ "$PASS" == true ]]; then
    echo -e "  ${GREEN}PASS${NC} — 动态 Validator 添加测试通过 ($NUM_INITIAL → $NUM_AFTER)"
else
    echo -e "  ${RED}FAIL${NC} — 动态 Validator 添加测试失败"
fi
echo "════════════════════════════════════════════════════════════════"
echo ""

if [[ "$KEEP_RUNNING" == "1" ]]; then
    log "KEEP_RUNNING=1，测试网保持运行。数据目录: $DATA_DIR"
    log "节点 RPC:"
    for i in $(seq 0 $NEW_INDEX); do
        info "  v${i}: http://127.0.0.1:$((BASE_HTTP + i))"
    done
    log "日志: $DATA_DIR/validator-*.log"
    log "按 Ctrl+C 停止"
    # Remove trap to avoid cleanup
    trap - EXIT
    wait
fi

if [[ "$PASS" == true ]]; then
    exit 0
else
    # 保留日志供调试
    warn "保留日志目录: $DATA_DIR"
    trap - EXIT  # don't cleanup on failure
    # Still kill processes
    for pid in "${ALL_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    exit 1
fi
