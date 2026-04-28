#!/usr/bin/env bash
#
# Symmetric network-partition test
#
# 4 validators, HotStuff-2, quorum = 3-of-4.
# Partition: {V0,V1} vs {V2,V3}.  Neither half has quorum → block production
# must stop.  After a hold period the partition heals → consensus must resume.
#
# Implementation: SIGKILL V2+V3 (rather than SIGSTOP/SIGCONT).
#
# Why not SIGSTOP/SIGCONT?
#   SIGSTOP freezes the Tokio async runtime.  Wall-clock time keeps advancing,
#   so when the process resumes, all timers that expired during the freeze fire
#   at once.  This floods the consensus engine with stale timeout events for
#   old views and the pacemaker can no longer re-sync — the nodes stay alive
#   but stop committing blocks indefinitely.
#
# Why not iptables/pfctl?
#   All nodes share 127.0.0.1; IP-level blocking can't distinguish
#   intra-partition traffic from inter-partition traffic.
#
# SIGKILL + restart from persisted MDBX state gives each node a clean async
# runtime on rejoin.  The same mechanism is used and proven in test-rejoin.sh.
# The safety property (no new blocks when < quorum nodes are up) is identical.
#
# What this exercises:
#   - Safety:   no block commits when < quorum nodes are reachable
#   - Liveness: consensus resumes after full connectivity is restored
#   - No-fork:  all nodes agree on the canonical chain after heal
#   - View/height ratio: detects "stale majority" (many timeouts, slow blocks)
#
# Usage:
#   ./scripts/test-partition.sh
#   ./scripts/test-partition.sh --hold 60 --recovery 180
#   KEEP_RUNNING=1 ./scripts/test-partition.sh
#
# Requirements:
#   cargo build --release -p n42-node-bin
#   pip3 install eth-account eth-utils eth-keys
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[partition]${NC} $*"; }
info() { echo -e "${CYAN}[partition]${NC} $*"; }
warn() { echo -e "${YELLOW}[partition]${NC} $*"; }
err()  { echo -e "${RED}[partition]${NC} $*"; }
fail() { err "$*"; exit 1; }

# ── Config ────────────────────────────────────────────────────────────────────
NUM_VALIDATORS=4
PARTITION_HOLD_S=30     # seconds to hold the partition before healing
RECOVERY_WINDOW_S=180   # seconds to wait for post-heal block production
KEEP_RUNNING="${KEEP_RUNNING:-0}"
DEBUG_BUILD=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)        DEBUG_BUILD=true; shift ;;
        --hold)         PARTITION_HOLD_S="$2"; shift 2 ;;
        --recovery)     RECOVERY_WINDOW_S="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--debug] [--hold SECONDS] [--recovery SECONDS]"
            echo "  --hold N       Partition hold duration in seconds (default: 30)"
            echo "  --recovery N   Recovery window in seconds (default: 180)"
            exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done

# ── Ports (distinct range: avoids conflicts with other test scripts) ──────────
# add-validator: HTTP 27000, P2P 31300, CONSENSUS 11000
# remove-validator: HTTP 28000, P2P 32300, CONSENSUS 12000
# rejoin: HTTP 26000, P2P 30600, CONSENSUS 16000
# partition: HTTP 31000, P2P 35300, CONSENSUS 19000
BASE_HTTP=31000
BASE_WS=31400
BASE_AUTH=31800
BASE_P2P=35300
BASE_CONSENSUS=19000
BASE_STARHUB=19400
BASE_METRICS=30800

DATA_DIR="/tmp/n42-test-partition-$$"

if [[ "$DEBUG_BUILD" == true ]]; then
    BINARY="$PROJECT_DIR/target/debug/n42-node"
else
    BINARY="$PROJECT_DIR/target/release/n42-node"
fi
[[ -f "$BINARY" ]] || fail "Binary not found: $BINARY\n  Run: cargo build --release -p n42-node-bin"

# ── Pre-declare all phase state variables (bash 3.2 nounset safety) ───────────
pre_partition_block=0
height_at_partition_start=0
height_after_hold=0
height_stalled=true
recovery_block=0
recovery_elapsed=0
common_h=0
hash_ok=true
ref_hash=""
liveness_ok=false

# ── Cleanup ───────────────────────────────────────────────────────────────────
declare -a ALL_PIDS=()
cleanup() {
    log "Cleaning up..."
    for pid in "${ALL_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    [[ "$KEEP_RUNNING" == "1" ]] || rm -rf "$DATA_DIR"
}
trap cleanup EXIT

# ── RPC helpers ───────────────────────────────────────────────────────────────
rpc_call() {
    curl -s --max-time 5 \
        -X POST -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$2\",\"params\":$3,\"id\":1}" \
        "http://127.0.0.1:$1" 2>/dev/null || true
}

get_block_number() {
    local raw
    raw=$(rpc_call "$1" eth_blockNumber '[]')
    python3 -c "
import json, sys
try: print(int(json.loads('''$raw''').get('result','0x0'), 16))
except: print(0)
" 2>/dev/null || echo 0
}

get_block_hash_at() {
    local hex_h
    hex_h=$(printf '0x%x' "$2")
    local raw
    raw=$(rpc_call "$1" eth_getBlockByNumber "[\"$hex_h\",false]")
    python3 -c "
import json
try: print((json.loads('''$raw''').get('result') or {}).get('hash',''))
except: print('')
" 2>/dev/null || echo ""
}

# get_consensus_view PORT → integer view number, or -1 on error
get_consensus_view() {
    local raw
    raw=$(rpc_call "$1" n42_consensusStatus '[]')
    python3 -c "
import json, sys
try:
    d = json.loads('''$raw''')
    v = (d.get('result') or {}).get('latestCommittedView')
    print(int(v) if v is not None else -1)
except:
    print(-1)
" 2>/dev/null || echo -1
}

# check_view_height_ratio PORT HEIGHT LABEL
# Warns when view/height > 2.0, errors (returns 1) when view >= height*3
check_view_height_ratio() {
    local port=$1 height=$2 label=$3
    [[ "$height" -le 0 ]] && return 0
    local view
    view=$(get_consensus_view "$port")
    if [[ "$view" -lt 0 ]]; then
        info "  $label: view unavailable (RPC may be starting)"
        return 0
    fi
    local ratio_x10
    ratio_x10=$(( view * 10 / height ))
    if [[ "$ratio_x10" -ge 30 ]]; then
        err "  $label: view/height critical — view=$view height=$height ($(( ratio_x10 / 10 )).$(( ratio_x10 % 10 ))x >= 3.0x, excessive timeouts)"
        return 1
    elif [[ "$ratio_x10" -gt 20 ]]; then
        warn "  $label: view/height elevated — view=$view height=$height ($(( ratio_x10 / 10 )).$(( ratio_x10 % 10 ))x > 2.0x)"
    else
        info "  $label: view=$view height=$height ratio=$(( ratio_x10 / 10 )).$(( ratio_x10 % 10 ))x  OK"
    fi
}

wait_for_height() {
    local port=$1 target=$2 timeout=${3:-120} label=${4:-node}
    local waited=0
    while (( waited < timeout )); do
        local h
        h=$(get_block_number "$port")
        (( h >= target )) && return 0
        (( waited % 15 == 0 )) && info "  $label: height=$h → waiting for $target  (${waited}s/${timeout}s)"
        sleep 3; (( waited += 3 ))
    done
    return 1
}

wait_for_rpc() {
    local port=$1 label=${2:-node} timeout=${3:-60}
    local waited=0
    while (( waited < timeout )); do
        rpc_call "$port" eth_blockNumber '[]' \
            | python3 -c "import sys,json; json.load(sys.stdin)" >/dev/null 2>&1 && return 0
        sleep 2; (( waited += 2 ))
    done
    return 1
}

# ── Key generation ─────────────────────────────────────────────────────────────
bls_secret_hex() { printf '%056x%08x' 0 $(($1 + 1)); }

generate_p2p_key() {
    python3 -c "
from eth_utils import keccak; from eth_keys import keys
sk = keccak(b'n42-partition-key-$1')
pk = keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
"
}

# ── Genesis ────────────────────────────────────────────────────────────────────
mkdir -p "$DATA_DIR"
log "Generating genesis (chainId=4245)..."
python3 - "$DATA_DIR" <<'PYEOF'
import json, os, sys
data_dir = sys.argv[1]
genesis = {
    "config": {
        "chainId": 4245,
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
GENESIS_FILE="$DATA_DIR/genesis.json"

# ── Pre-generate keys ──────────────────────────────────────────────────────────
log "Generating keys for $NUM_VALIDATORS validators..."
declare -a BLS_KEYS=() P2P_SECRETS=() P2P_PUBKEYS=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    BLS_KEYS+=("$(bls_secret_hex "$i")")
    read -r secret pubkey <<< "$(generate_p2p_key "$i")"
    P2P_SECRETS+=("$secret")
    P2P_PUBKEYS+=("$pubkey")
done

declare -a ENODES=()
declare -a NODE_PIDS=()

start_validator() {
    local idx=$1
    local datadir="$DATA_DIR/validator-$idx"
    local log_file="$DATA_DIR/validator-$idx.log"
    mkdir -p "$datadir"

    local trusted=""
    (( ${#ENODES[@]} > 0 )) && trusted="--trusted-peers $(IFS=,; echo "${ENODES[*]}")"

    RUST_LOG="info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error" \
    N42_VALIDATOR_KEY="${BLS_KEYS[$idx]}" \
    N42_VALIDATOR_COUNT="$NUM_VALIDATORS" \
    N42_ENABLE_MDNS="true" \
    N42_DATA_DIR="$datadir" \
    N42_CONSENSUS_PORT="$((BASE_CONSENSUS + idx))" \
    N42_STARHUB_PORT="$((BASE_STARHUB + idx))" \
    N42_MAX_EMPTY_SKIPS="0" \
    N42_BLOCK_INTERVAL_MS="2000" \
    N42_BASE_TIMEOUT_MS="15000" \
    N42_MAX_TIMEOUT_MS="45000" \
    N42_STARTUP_DELAY_MS="2000" \
    N42_REWARD_EPOCH_BLOCKS="50" \
    N42_OPEN_VERIFICATION="1" \
    "$BINARY" node \
        --chain "$GENESIS_FILE" \
        --datadir "$datadir" \
        --http --http.addr "0.0.0.0" --http.port "$((BASE_HTTP + idx))" \
        --http.api eth,net,web3,txpool,rpc \
        --ws --ws.port "$((BASE_WS + idx))" \
        --authrpc.port "$((BASE_AUTH + idx))" \
        --port "$((BASE_P2P + idx))" \
        --discovery.port "$((BASE_P2P + idx))" \
        --log.file.directory "$datadir/logs" \
        --ipcdisable \
        --max-outbound-peers "$((NUM_VALIDATORS + 2))" \
        --max-inbound-peers "$((NUM_VALIDATORS + 2))" \
        --metrics "127.0.0.1:$((BASE_METRICS + idx))" \
        --builder.gaslimit 2000000000 \
        --builder.interval 50ms \
        --rpc.max-connections 100 \
        --disable-tx-gossip \
        --p2p-secret-key-hex "${P2P_SECRETS[$idx]}" \
        $trusted \
        >> "$log_file" 2>&1 &
    ENODES+=("enode://${P2P_PUBKEYS[$idx]}@127.0.0.1:$((BASE_P2P + idx))")
}

# ══════════════════════════════════════════════════════════════════════════════
# Phase 1 — Start all 4 validators, wait for stable block production
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 1: Starting $NUM_VALIDATORS validators ═══"

for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    start_validator "$i"
    pid=$!
    NODE_PIDS+=("$pid")
    ALL_PIDS+=("$pid")
    info "  v$i started (pid=$pid) http=:$((BASE_HTTP+i))"
    [[ $i -lt $((NUM_VALIDATORS - 1)) ]] && sleep 2
done

RPC_V0=$BASE_HTTP   # V0 is our stable RPC throughout (stays in the live half)

log "Waiting for first block..."
wait_for_height "$RPC_V0" 1 120 "v0" || fail "No blocks in 120s — check $DATA_DIR/validator-0.log"

log "Waiting for 5 more blocks to confirm stable liveness..."
set +u
_cur=$(get_block_number "$RPC_V0")
_stable_target=$(( _cur + 5 ))
set -u
wait_for_height "$RPC_V0" "$_stable_target" 60 "v0" || warn "Stability wait timed out, continuing"

pre_partition_block=$(get_block_number "$RPC_V0")
log "Network stable at height $pre_partition_block"

check_view_height_ratio "$RPC_V0" "$pre_partition_block" "v0(pre-partition)" || true

# ══════════════════════════════════════════════════════════════════════════════
# Phase 2 — Partition: SIGKILL V2 and V3
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 2: Applying partition — killing V2 and V3 ═══"
info "  Neither {V0,V1} nor {V2,V3} has quorum (need 3-of-4) → blocks must stop"

for idx in 2 3; do
    kill -KILL "${NODE_PIDS[$idx]}" 2>/dev/null || warn "  Could not kill v$idx"
    wait "${NODE_PIDS[$idx]}" 2>/dev/null || true
    info "  v$idx (pid=${NODE_PIDS[$idx]}) killed"
    # Remove from ALL_PIDS so cleanup doesn't re-kill
    ALL_PIDS=("${ALL_PIDS[@]/${NODE_PIDS[$idx]}/}")
done

height_at_partition_start=$(get_block_number "$RPC_V0")
info "  Height at partition start: $height_at_partition_start"

# ══════════════════════════════════════════════════════════════════════════════
# Phase 3 — Hold, verify no new blocks (safety)
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 3: Holding partition for ${PARTITION_HOLD_S}s — checking safety ═══"

# Allow +1 for any in-flight block that committed just as we killed V2/V3
_max_safe_h=$(( height_at_partition_start + 1 ))
info "  Monitoring V0 — height must not exceed $_max_safe_h"

waited_stall=0
height_stalled=true
while (( waited_stall < PARTITION_HOLD_S )); do
    set +u
    _tmp_h=$(get_block_number "$RPC_V0" 2>/dev/null || echo 0)
    cur_h="${_tmp_h:-0}"
    set -u
    if (( cur_h > _max_safe_h )); then
        height_stalled=false
        warn "  !! V0 advanced to $cur_h (max allowed=$_max_safe_h) during partition"
    fi
    (( waited_stall % 10 == 0 )) && info "  ${waited_stall}s: V0 height=$cur_h (started=$height_at_partition_start)"
    sleep 5; (( waited_stall += 5 ))
done

height_after_hold=$(get_block_number "$RPC_V0")
info "  After ${PARTITION_HOLD_S}s hold: V0 height=$height_after_hold"

# ══════════════════════════════════════════════════════════════════════════════
# Phase 4 — Heal: restart V2 and V3 from persisted MDBX state
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 4: Healing partition — restarting V2 and V3 from persisted state ═══"

# Remove stale enode entries for V2, V3 so start_validator re-appends fresh ones
ENODES=("${ENODES[@]:0:2}")

for idx in 2 3; do
    start_validator "$idx"
    new_pid=$!
    NODE_PIDS[$idx]=$new_pid
    ALL_PIDS+=("$new_pid")
    info "  v$idx restarted (pid=$new_pid)"
done

log "  Waiting for V2 and V3 RPC to come up..."
for idx in 2 3; do
    wait_for_rpc "$((BASE_HTTP + idx))" "v$idx" 60 \
        && info "  v$idx RPC ready" \
        || warn "  v$idx RPC not responding after 60s"
done

# ══════════════════════════════════════════════════════════════════════════════
# Phase 5 — Recovery: wait for ≥2 new blocks (liveness)
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 5: Verifying liveness recovery (${RECOVERY_WINDOW_S}s window) ═══"

_target_h=$(( height_after_hold + 2 ))
info "  Waiting for V0 to reach height $_target_h (held at $height_after_hold)"

recovery_elapsed=0
recovery_block=0
liveness_ok=false
while (( recovery_elapsed < RECOVERY_WINDOW_S )); do
    set +u
    _tmp_blk=$(get_block_number "$RPC_V0" 2>/dev/null || echo 0)
    cur="${_tmp_blk:-0}"
    set -u
    if (( cur >= _target_h )); then
        liveness_ok=true
        recovery_block="$cur"
        break
    fi
    (( recovery_elapsed % 15 == 0 )) && info "  ${recovery_elapsed}s: V0 height=$cur (target=$_target_h)"
    sleep 3; (( recovery_elapsed += 3 ))
done

if [[ "$liveness_ok" == false ]]; then
    recovery_block=$(get_block_number "$RPC_V0")
fi

info "  liveness_ok=$liveness_ok  height=$recovery_block  target=$_target_h  elapsed=${recovery_elapsed}s"

check_view_height_ratio "$RPC_V0" "$recovery_block" "v0(post-heal)" || true

# ══════════════════════════════════════════════════════════════════════════════
# Phase 6 — No-fork: all 4 nodes agree on hash at common height
# ══════════════════════════════════════════════════════════════════════════════
log "═══ Phase 6: No-fork check across all $NUM_VALIDATORS nodes ═══"

# Allow V2/V3 time to sync before comparing
log "  Waiting for V2/V3 to sync (up to 60s)..."
set +u
_sync_target="${recovery_block:-$height_after_hold}"
set -u
for idx in 2 3; do
    wait_for_height "$((BASE_HTTP + idx))" "$_sync_target" 60 "v$idx" || \
        warn "  v$idx did not reach height $_sync_target within 60s"
done

declare -a FINAL_H=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    set +u
    _h=$(get_block_number "$((BASE_HTTP + i))" 2>/dev/null || echo 0)
    set -u
    FINAL_H+=("${_h:-0}")
    info "  v$i: height=${_h:-0}"
done

common_h="${FINAL_H[0]}"
for h in "${FINAL_H[@]}"; do
    (( h < common_h )) && common_h="$h"
done

# View/height ratio check across all nodes
ratio_ok=true
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    set +u
    _fh="${FINAL_H[$i]:-0}"
    set -u
    check_view_height_ratio "$((BASE_HTTP + i))" "${_fh}" "v${i}(final)" || ratio_ok=false
done

hash_ok=true
ref_hash=""
if (( common_h > 0 )); then
    ref_hash=$(get_block_hash_at "$RPC_V0" "$common_h")
    info "  Reference hash at height $common_h: ${ref_hash:0:40}..."
    for i in $(seq 1 $((NUM_VALIDATORS - 1))); do
        set +u
        _h=$(get_block_hash_at "$((BASE_HTTP + i))" "$common_h" 2>/dev/null || echo "")
        set -u
        candidate="${_h:-}"
        if [[ -n "$ref_hash" && "$candidate" != "$ref_hash" ]]; then
            err "  v$i FORK at height $common_h: $candidate"
            hash_ok=false
        else
            info "  v$i hash OK: ${candidate:0:20}..."
        fi
    done
else
    warn "  common_h=0, skipping hash comparison"
fi

# ══════════════════════════════════════════════════════════════════════════════
# Result
# ══════════════════════════════════════════════════════════════════════════════
height_delta=$(( height_after_hold - height_at_partition_start ))

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Partition test ($NUM_VALIDATORS nodes, hold=${PARTITION_HOLD_S}s)"
echo "════════════════════════════════════════════════════════════════"

PASS=true

# 1. Safety
if [[ "$height_stalled" == true ]]; then
    echo -e "  ${GREEN}✓${NC} 分区安全性: 分区期间出块停止（V0 高度 ${height_at_partition_start} → ${height_after_hold}，差 ${height_delta} 块）"
else
    echo -e "  ${RED}✗${NC} 分区安全性违反: 分区期间 V0 高度从 ${height_at_partition_start} 推进到 ${height_after_hold}"
    PASS=false
fi

# 2. Liveness
if [[ "$liveness_ok" == true ]]; then
    echo -e "  ${GREEN}✓${NC} 活性恢复: ${recovery_elapsed}s 内产出新块至高度 ${recovery_block}"
else
    echo -e "  ${RED}✗${NC} 活性恢复失败: ${RECOVERY_WINDOW_S}s 内未产出足够新块 (当前高度=${recovery_block}，期望>=${_target_h})"
    PASS=false
fi

# 3. No-fork
if [[ "$hash_ok" == true && -n "$ref_hash" ]]; then
    echo -e "  ${GREEN}✓${NC} 无分叉: 高度 ${common_h} 全节点哈希一致（${ref_hash:0:20}...）"
elif [[ -z "$ref_hash" ]]; then
    echo -e "  ${YELLOW}⚠${NC} 无分叉: 无法获取哈希（高度=${common_h}）"
else
    echo -e "  ${RED}✗${NC} 检测到分叉: 节点在高度 ${common_h} 哈希不一致"
    PASS=false
fi

# 4. View/height ratio
if [[ "$ratio_ok" == true ]]; then
    echo -e "  ${GREEN}✓${NC} View/height 比例正常（无过度超时）"
else
    echo -e "  ${RED}✗${NC} View/height 比例异常（>= 3.0x，超时过多）"
    PASS=false
fi

echo "════════════════════════════════════════════════════════════════"
if [[ "$PASS" == true ]]; then
    echo -e "  ${GREEN}PASS${NC}"
else
    echo -e "  ${RED}FAIL${NC}"
fi
echo "════════════════════════════════════════════════════════════════"
echo ""

if [[ "$KEEP_RUNNING" == "1" ]]; then
    log "KEEP_RUNNING=1 — testnet still running in $DATA_DIR"
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        info "  v$i: http://127.0.0.1:$((BASE_HTTP + i))"
    done
    trap - EXIT; wait
fi

if [[ "$PASS" == true ]]; then exit 0; fi

warn "Logs preserved in $DATA_DIR"
trap - EXIT
for pid in "${ALL_PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
exit 1
