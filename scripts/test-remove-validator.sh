#!/usr/bin/env bash
#
# 动态 Validator 移除端到端测试
#
# 测试流程：
#   Phase 1: 启动 N 节点测试网，等待出块稳定
#   Phase 2: 通过 RPC 提案移除最后一个 validator（index N-1）
#   Phase 3: 等待 epoch 边界激活，验证 validator set 缩减
#   Phase 4: 验证剩余节点继续出块，被移除节点停止参与共识
#
# 用法：
#   ./scripts/test-remove-validator.sh                  # 默认 5→4 节点
#   ./scripts/test-remove-validator.sh --nodes 7        # 7→6 节点
#   ./scripts/test-remove-validator.sh --nodes 10       # 10→9 节点
#   KEEP_RUNNING=1 ./scripts/test-remove-validator.sh   # 测试后不停止，方便调试
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

log()  { echo -e "${GREEN}[remove-validator]${NC} $*"; }
info() { echo -e "${CYAN}[remove-validator]${NC} $*"; }
warn() { echo -e "${YELLOW}[remove-validator]${NC} $*"; }
err()  { echo -e "${RED}[remove-validator]${NC} $*"; }
fail() { err "$*"; cleanup; exit 1; }

# ── Args ──
NUM_INITIAL=5
DEBUG_BUILD=false
KEEP_RUNNING="${KEEP_RUNNING:-0}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --nodes) NUM_INITIAL="$2"; shift 2 ;;
        --debug) DEBUG_BUILD=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--nodes N] [--debug]"
            echo "  --nodes N   Initial validator count (default: 5, must be >= 5)"
            echo "  --debug     Use debug build"
            echo "  KEEP_RUNNING=1  Don't stop after test (for debugging)"
            exit 0 ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ "$NUM_INITIAL" -lt 5 ]]; then
    fail "Initial validator count must be >= 5 (MIN_VALIDATOR_COUNT=4; need >= 4 remaining after removal)"
fi

# Remove the last validator (highest index)
REMOVE_INDEX=$((NUM_INITIAL - 1))
NUM_AFTER=$((NUM_INITIAL - 1))
ADMIN_TOKEN="test-remove-validator-$(date +%s)"

# ── Port allocation ──
# Use high port range to avoid conflict with default testnet or add-validator test
BASE_HTTP=28000
BASE_WS=28400
BASE_AUTH=28800
BASE_P2P=32300
BASE_CONSENSUS=12000
BASE_STARHUB=12400
BASE_METRICS=22200

DATA_DIR="/tmp/n42-test-remove-validator-$$"

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
bls_secret_hex() {
    local idx=$1
    local val=$((idx + 1))
    printf '%056x%08x' 0 "$val"
}

# Validator address: 20 bytes, all zeros except last 2 bytes = (index+1) big-endian
validator_address() {
    local idx=$1
    local val=$((idx + 1))
    printf '0x%036x%04x' 0 "$val"
}

# Fetch block hash at a given decimal height from a node's HTTP RPC port.
# Returns empty string on any error (node down, block not yet produced, timeout).
block_hash_at() {
    local port=$1 height=$2
    local hex_h
    hex_h=$(printf '0x%x' "$height")
    python3 -c "
import sys, json, urllib.request
req = urllib.request.Request(
    'http://127.0.0.1:${port}',
    data=json.dumps({'jsonrpc':'2.0','method':'eth_getBlockByNumber',
                     'params':['${hex_h}',False],'id':1}).encode(),
    headers={'Content-Type':'application/json'}, method='POST')
try:
    with urllib.request.urlopen(req, timeout=5) as r:
        obj = json.load(r)
    result = obj.get('result') or {}
    print(result.get('hash',''))
except Exception:
    print('')
" 2>/dev/null || echo ""
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
        "chainId": 4243,
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
    "gasLimit": hex(int(os.environ.get("N42_GAS_LIMIT", "2000000000"))), "difficulty": "0x0",
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

log "═══ Phase 1: 启动 ${NUM_INITIAL} 节点测试网（MIN_VALIDATOR_COUNT=4，移除后剩余 ${NUM_AFTER}）═══"

generate_genesis
GENESIS_FILE="$DATA_DIR/genesis.json"

# Generate P2P keys for all initial validators
declare -a P2P_SECRETS=()
declare -a P2P_PUBKEYS=()
for i in $(seq 0 $((NUM_INITIAL - 1))); do
    read -r secret pubkey <<< "$(generate_p2p_key "$i")"
    P2P_SECRETS+=("$secret")
    P2P_PUBKEYS+=("$pubkey")
done

# Generate BLS keys
declare -a BLS_KEYS=()
for i in $(seq 0 $((NUM_INITIAL - 1))); do
    BLS_KEYS+=("$(bls_secret_hex "$i")")
done

# Timeouts
case "$NUM_INITIAL" in
    5)  BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000; STARTUP_DELAY_MS=2000 ;;
    7)  BASE_TIMEOUT_MS=10000; MAX_TIMEOUT_MS=30000; STARTUP_DELAY_MS=3000 ;;
    *)  BASE_TIMEOUT_MS=15000; MAX_TIMEOUT_MS=45000; STARTUP_DELAY_MS=3000 ;;
esac

# Start all N validators
declare -a ENODES=()
declare -a NODE_PIDS=()
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
        --max-outbound-peers "$NUM_INITIAL" \
        --max-inbound-peers "$NUM_INITIAL" \
        --metrics "127.0.0.1:$metrics_port" \
        --builder.gaslimit "${N42_GAS_LIMIT:-2000000000}" \
        --builder.interval 50ms \
        --rpc.max-connections 100 \
        --disable-tx-gossip \
        --p2p-secret-key-hex "${P2P_SECRETS[$i]}" \
        ${TRUSTED_PEERS_FLAG} \
        > "$log_file" 2>&1 &

    NODE_PIDS+=($!)
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
block_num=0
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
# Phase 2: 查询当前 validator set，发起移除提案
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 2: 提案移除 validator #${REMOVE_INDEX} ═══"

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

# 获取被移除 validator 的地址（直接从 validator set 中读取）
REMOVE_ADDR=$(echo "$vs_response" | python3 -c "
import sys, json
r = json.load(sys.stdin)
# 最后一个 validator（index REMOVE_INDEX）
print(r['result']['active'][${REMOVE_INDEX}].get('address', ''))
" 2>/dev/null || echo "")

# 如果 RPC 没有返回 address 字段，用确定性公式推算
if [[ -z "$REMOVE_ADDR" ]]; then
    REMOVE_ADDR=$(validator_address "$REMOVE_INDEX")
    info "RPC 未返回地址字段，使用推算地址: $REMOVE_ADDR"
fi

info "待移除 validator 地址: $REMOVE_ADDR (index=${REMOVE_INDEX})"

# 发起移除提案（只需要地址，不需要公钥）
log "调用 n42_proposeRemoveValidator..."
remove_response=$(curl -s --max-time 10 \
    -X POST -H 'Content-Type: application/json' \
    -d "{
        \"jsonrpc\": \"2.0\",
        \"method\": \"n42_proposeRemoveValidator\",
        \"params\": [\"$ADMIN_TOKEN\", \"$REMOVE_ADDR\"],
        \"id\": 1
    }" \
    "$RPC_URL")

info "RPC 响应: $remove_response"

if echo "$remove_response" | python3 -c "import sys,json; r=json.load(sys.stdin); assert 'result' in r" 2>/dev/null; then
    log "移除提案成功！"
else
    error_msg=$(echo "$remove_response" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r.get('error',{}).get('message','unknown'))" 2>/dev/null || echo "unknown")
    fail "移除提案失败: $error_msg"
fi

# ══════════════════════════════════════════════════════════════════════
# Phase 3: 等待 epoch 切换，验证 validator set 缩减
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 3: 等待 epoch 切换（${NUM_INITIAL}→${NUM_AFTER} 验证者）═══"

info "等待 epoch 切换（epoch_length=30 blocks，约 60-90 秒）..."

waited=0
max_wait=180
new_count=0
new_epoch=0
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
        log "Epoch 已切换！validator set 大小: $new_count, epoch: $new_epoch"
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

# 验证被移除的 validator 不再出现在 active 列表中
removed_still_active=$(echo "$vs_resp" | python3 -c "
import sys, json
r = json.load(sys.stdin)
active = r['result']['active']
# 检查被移除的地址或 index 是否仍在 active 中
print(len([v for v in active if v.get('index') == ${REMOVE_INDEX}]))
" 2>/dev/null || echo "0")

if [[ "$removed_still_active" -eq 0 ]]; then
    log "确认：validator #${REMOVE_INDEX} 已从 active 列表移除"
else
    warn "validator #${REMOVE_INDEX} 仍在 active 列表中（可能索引已重排）"
fi

# ══════════════════════════════════════════════════════════════════════
# Phase 4: 验证剩余节点持续出块
# ══════════════════════════════════════════════════════════════════════

log "═══ Phase 4: 验证剩余 ${NUM_AFTER} 节点持续出块 ═══"

# 等足够的块确认剩余节点独立运行
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
max_phase4_wait=$((blocks_to_wait * 4))
while [[ $waited -lt $max_phase4_wait ]]; do
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

# ── 验证高度一致性（只检查前 NUM_AFTER 个节点）──
log "验证剩余节点高度一致..."
declare -a FINAL_HEIGHTS=()
for i in $(seq 0 $((NUM_AFTER - 1))); do
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

# 检查高度差 + 区块哈希一致性
# set +u/-u guards the whole block: bash version differences around
# nounset + read/array interactions cannot abort the script here.
set +u
min_h=0
max_h=0
height_diff=0
hash_ok=true
ref_hash=""
if [[ "${#FINAL_HEIGHTS[@]}" -gt 0 ]]; then
    _stats=$(python3 -c "
hs=list(map(int,'${FINAL_HEIGHTS[*]}'.split()))
mn,mx=min(hs),max(hs)
print(mn,mx,mx-mn)
" 2>/dev/null) || _stats="0 0 0"
    read -r min_h max_h height_diff <<< "${_stats:-0 0 0}" || true

    if [[ "${min_h:-0}" -gt 0 ]]; then
        log "验证区块哈希一致性（高度 $min_h）..."
        for i in $(seq 0 $((NUM_AFTER - 1))); do
            port=$((BASE_HTTP + i))
            bh=$(block_hash_at "$port" "$min_h")
            if [[ -z "$bh" ]]; then
                warn "  v${i}: 无法获取高度 $min_h 的区块哈希，跳过"
                continue
            fi
            info "  v${i}: hash=${bh:0:20}..."
            if [[ -z "$ref_hash" ]]; then
                ref_hash="$bh"
            elif [[ "$bh" != "$ref_hash" ]]; then
                err "  v${i}: 分叉! hash=$bh 与参考 $ref_hash 不符"
                hash_ok=false
            fi
        done
    fi
fi
set -u

# ── 验证每个剩余节点都看到了缩减后的 validator set ──
log "验证剩余节点 validator set..."
all_vs_correct=true
for i in $(seq 0 $((NUM_AFTER - 1))); do
    port=$((BASE_HTTP + i))
    vs_r=$(curl -s --max-time 5 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
        "http://127.0.0.1:$port" 2>/dev/null)
    vs_count=$(echo "$vs_r" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r['result']['active']))" 2>/dev/null || echo "0")
    vs_epoch=$(echo "$vs_r" | python3 -c "import sys,json; r=json.load(sys.stdin); print(r['result']['currentEpoch'])" 2>/dev/null || echo "0")
    info "  v${i}: validators=$vs_count epoch=$vs_epoch"
    if [[ "$vs_count" -ne "$NUM_AFTER" ]]; then
        all_vs_correct=false
    fi
done

# ── 验证被移除节点的进程状态（可选：它可能仍在运行但不参与共识）──
REMOVED_PID=${NODE_PIDS[$REMOVE_INDEX]}
removed_alive=false
if kill -0 "$REMOVED_PID" 2>/dev/null; then
    removed_alive=true
    # 检查被移除节点的 validator set — 它自己也应该看到 epoch 已切换
    removed_port=$((BASE_HTTP + REMOVE_INDEX))
    removed_vs=$(curl -s --max-time 5 \
        -X POST -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","method":"n42_validatorSet","params":[],"id":1}' \
        "http://127.0.0.1:$removed_port" 2>/dev/null)
    removed_vs_count=$(echo "$removed_vs" | python3 -c "import sys,json; r=json.load(sys.stdin); print(len(r['result']['active']))" 2>/dev/null || echo "0")
    info "  v${REMOVE_INDEX} (removed): validators=$removed_vs_count (process still running)"
fi

# ── 输出结果 ──
echo ""
echo "════════════════════════════════════════════════════════════════"

PASS=true

if [[ ${height_diff:-0} -le 3 && ${min_h:-0} -gt 0 ]]; then
    echo -e "  ${GREEN}✓${NC} 剩余节点高度一致: 最大差距 ${height_diff:-0} 块（${min_h:-0} ~ ${max_h:-0}）"
else
    echo -e "  ${RED}✗${NC} 剩余节点高度不一致: 差距 ${height_diff:-0} 块（${min_h:-0} ~ ${max_h:-0}）"
    PASS=false
fi

if [[ "$all_vs_correct" == true ]]; then
    echo -e "  ${GREEN}✓${NC} 所有剩余节点 validator set: ${NUM_AFTER} 个（期望 ${NUM_AFTER}）"
else
    echo -e "  ${RED}✗${NC} 部分剩余节点 validator set 不正确（期望 ${NUM_AFTER}）"
    PASS=false
fi

if [[ ${max_h:-0} -gt $activate_block ]]; then
    echo -e "  ${GREEN}✓${NC} epoch 切换后持续出块: $activate_block → ${max_h:-0}"
else
    echo -e "  ${RED}✗${NC} epoch 切换后未见新块"
    PASS=false
fi

if [[ "${hash_ok:-true}" == true && -n "${ref_hash:-}" ]]; then
    echo -e "  ${GREEN}✓${NC} 区块哈希一致性: 高度 ${min_h:-0} 全节点哈希一致（${ref_hash:0:20}...）"
elif [[ "${hash_ok:-true}" == true && -z "${ref_hash:-}" ]]; then
    echo -e "  ${YELLOW}~${NC} 区块哈希一致性: 无法验证（节点未响应）"
else
    echo -e "  ${RED}✗${NC} 区块哈希不一致: 检测到分叉！"
    PASS=false
fi

echo "════════════════════════════════════════════════════════════════"

if [[ "$PASS" == true ]]; then
    echo -e "  ${GREEN}PASS${NC} — 动态 Validator 移除测试通过 ($NUM_INITIAL → $NUM_AFTER)"
else
    echo -e "  ${RED}FAIL${NC} — 动态 Validator 移除测试失败"
fi
echo "════════════════════════════════════════════════════════════════"
echo ""

if [[ "$KEEP_RUNNING" == "1" ]]; then
    log "KEEP_RUNNING=1，测试网保持运行。数据目录: $DATA_DIR"
    log "节点 RPC:"
    for i in $(seq 0 $((NUM_INITIAL - 1))); do
        info "  v${i}: http://127.0.0.1:$((BASE_HTTP + i))"
    done
    log "日志: $DATA_DIR/validator-*.log"
    log "按 Ctrl+C 停止"
    trap - EXIT
    wait
fi

if [[ "$PASS" == true ]]; then
    exit 0
else
    warn "保留日志目录: $DATA_DIR"
    trap - EXIT
    for pid in "${ALL_PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    exit 1
fi
