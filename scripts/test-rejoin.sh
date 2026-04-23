#!/usr/bin/env bash
#
# Validator rejoin-after-isolation test
#
# One validator is killed (SIGKILL) after the network is running.
# The remaining validators must continue producing blocks (they have
# enough quorum without the missing node).  The killed node is then
# restarted from its persisted MDBX state and must sync the missed
# blocks and rejoin consensus — without any manual intervention.
#
# What this exercises:
#   - Block-sync path (peer_committed_view chase loop, up to 128 blocks/round)
#   - Consensus snapshot load on restart
#   - View-change / timeout recovery when the isolated node was scheduled leader
#   - Hash consistency: all N nodes on the same chain after rejoin
#
# Usage:
#   ./scripts/test-rejoin.sh                         # 4 nodes, isolate 20 blocks
#   ./scripts/test-rejoin.sh --nodes 5               # 5 nodes
#   ./scripts/test-rejoin.sh --isolate-blocks 50     # miss 50 blocks
#   KEEP_RUNNING=1 ./scripts/test-rejoin.sh
#
# Requirements:
#   cargo build --release -p n42-node-bin
#   pip3 install eth-account eth-utils eth-keys
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
log()  { echo -e "${GREEN}[rejoin]${NC} $*"; }
info() { echo -e "${CYAN}[rejoin]${NC} $*"; }
warn() { echo -e "${YELLOW}[rejoin]${NC} $*"; }
err()  { echo -e "${RED}[rejoin]${NC} $*"; }
fail() { err "$*"; exit 1; }

# ── Defaults ──────────────────────────────────────────────────────────
NUM_VALIDATORS=4    # Must leave >= quorum nodes running; quorum=2f+1, f=(n-1)/3
ISOLATE_BLOCKS=20   # How many blocks the killed node misses before restart
DEBUG_BUILD=false
KEEP_RUNNING="${KEEP_RUNNING:-0}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --nodes)           NUM_VALIDATORS="$2"; shift 2 ;;
        --isolate-blocks)  ISOLATE_BLOCKS="$2"; shift 2 ;;
        --debug)           DEBUG_BUILD=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--nodes N] [--isolate-blocks N] [--debug]"
            echo "  --nodes N            Validator count (default: 4)"
            echo "  --isolate-blocks N   Blocks the isolated node misses (default: 20)"
            exit 0 ;;
        *) fail "Unknown option: $1" ;;
    esac
done

# With n validators f=(n-1)/3, quorum=2f+1.
# After removing 1 we need (n-1) >= quorum = 2*((n-1)/3)+1.
# This holds for n>=4.  n=3 technically works (f=0, quorum=1, 2 remaining > 1)
# but is less interesting because the leader self-votes without followers anyway.
if [[ $NUM_VALIDATORS -lt 3 ]]; then
    fail "--nodes must be >= 3"
fi

# The isolated node is the last validator (index NUM_VALIDATORS-1).
# Node 0 stays up and serves as our stable RPC endpoint throughout.
ISOLATED_IDX=$((NUM_VALIDATORS - 1))

# ── Ports (distinct range to avoid conflicts with other test scripts) ──
BASE_HTTP=26000
BASE_WS=26400
BASE_AUTH=26800
BASE_P2P=30600   # 30600–30609: avoids 30303 (Ethereum default) and other test scripts
BASE_CONSENSUS=16000
BASE_STARHUB=16400
BASE_METRICS=25800

DATA_DIR="/tmp/n42-test-rejoin-$$"

if [[ "$DEBUG_BUILD" == true ]]; then
    BINARY="$PROJECT_DIR/target/debug/n42-node"
else
    BINARY="$PROJECT_DIR/target/release/n42-node"
fi
[[ -f "$BINARY" ]] || fail "Binary not found: $BINARY\n  Run: cargo build --release -p n42-node-bin"

# ── Cleanup ────────────────────────────────────────────────────────────
declare -a ALL_PIDS=()
cleanup() {
    for pid in "${ALL_PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    [[ "$KEEP_RUNNING" == "1" ]] || rm -rf "$DATA_DIR"
}
trap cleanup EXIT

# ── RPC helpers ────────────────────────────────────────────────────────
rpc() {
    curl -s --max-time 5 \
        -X POST -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$2\",\"params\":$3,\"id\":1}" \
        "http://127.0.0.1:$1" 2>/dev/null || true
}

block_number() {
    local raw; raw=$(rpc "$1" eth_blockNumber '[]')
    python3 -c "
import json, sys
try: print(int(json.loads('''$raw''').get('result','0x0'), 16))
except: print(0)
" 2>/dev/null || echo 0
}

block_hash_at() {
    local hex_h; hex_h=$(printf '0x%x' "$2")
    local raw; raw=$(rpc "$1" eth_getBlockByNumber "[\"$hex_h\",false]")
    python3 -c "
import json
try: print((json.loads('''$raw''').get('result') or {}).get('hash',''))
except: print('')
" 2>/dev/null || echo ""
}

wait_for_height() {
    # wait_for_height PORT TARGET [TIMEOUT_S] [LABEL]
    local port=$1 target=$2 timeout=${3:-120} label=${4:-node}
    local waited=0
    while (( waited < timeout )); do
        local h; h=$(block_number "$port")
        (( h >= target )) && return 0
        (( waited % 15 == 0 )) && info "  $label: height=$h → waiting for $target  (${waited}s)"
        sleep 3; (( waited+=3 ))
    done
    return 1
}

# ── Key generation ─────────────────────────────────────────────────────
bls_secret_hex() { printf '%056x%08x' 0 $(($1 + 1)); }

generate_p2p_key() {
    python3 -c "
from eth_utils import keccak; from eth_keys import keys
sk = keccak(b'n42-devp2p-key-$1')
pk = keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
"
}

# ── Genesis ────────────────────────────────────────────────────────────
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

# ── Pre-generate keys ──────────────────────────────────────────────────
log "Generating keys for $NUM_VALIDATORS validators..."
declare -a BLS_KEYS=() P2P_SECRETS=() P2P_PUBKEYS=()
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    BLS_KEYS+=("$(bls_secret_hex "$i")")
    read -r secret pubkey <<< "$(generate_p2p_key "$i")"
    P2P_SECRETS+=("$secret")
    P2P_PUBKEYS+=("$pubkey")
done

# ══════════════════════════════════════════════════════════════════════
# Phase 1 — Start all validators, wait for stable block production
# ══════════════════════════════════════════════════════════════════════
log "═══ Phase 1: Starting $NUM_VALIDATORS validators ═══"

declare -a ENODES=()
declare -a NODE_PIDS=()

# Launch one validator node in the background.
# Called directly (NOT in a command substitution) so that ENODES and ALL_PIDS
# updates take effect in the parent shell and $! is the actual node PID.
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
        --max-outbound-peers "$((NUM_VALIDATORS + 1))" \
        --max-inbound-peers "$((NUM_VALIDATORS + 1))" \
        --metrics "127.0.0.1:$((BASE_METRICS + idx))" \
        --builder.gaslimit 2000000000 \
        --builder.interval 50ms \
        --rpc.max-connections 100 \
        --disable-tx-gossip \
        --p2p-secret-key-hex "${P2P_SECRETS[$idx]}" \
        $trusted \
        >> "$log_file" 2>&1 &
    # Caller captures $! immediately after this function returns.
    # Do NOT echo the PID — this function must not run in a command substitution.
    ENODES+=("enode://${P2P_PUBKEYS[$idx]}@127.0.0.1:$((BASE_P2P + idx))")
}

for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    start_validator "$i"
    pid=$!
    NODE_PIDS+=("$pid")
    ALL_PIDS+=("$pid")
    info "  v$i started (pid=$pid) http=:$((BASE_HTTP+i))"
    [[ $i -lt $((NUM_VALIDATORS - 1)) ]] && sleep 2
done

RPC="$((BASE_HTTP))"   # node 0 is our stable RPC for the whole test

log "Waiting for first block..."
wait_for_height "$RPC" 1 120 "v0" || fail "No blocks within 120s — check $DATA_DIR/validator-0.log"

log "Waiting for 5 blocks to confirm stable liveness..."
h1=$(block_number "$RPC")
wait_for_height "$RPC" $((h1 + 5)) 60 "v0" || warn "Stability wait timed out, continuing"

H_BEFORE=$(block_number "$RPC")
log "Network stable at height $H_BEFORE"

# ══════════════════════════════════════════════════════════════════════
# Phase 2 — Kill the isolated validator, let the rest keep going
# ══════════════════════════════════════════════════════════════════════
log "═══ Phase 2: Isolating v$ISOLATED_IDX (kill + wait $ISOLATE_BLOCKS blocks) ═══"

ISOLATED_PID="${NODE_PIDS[$ISOLATED_IDX]}"
kill -KILL "$ISOLATED_PID" 2>/dev/null || fail "Could not kill v$ISOLATED_IDX (pid=$ISOLATED_PID)"
wait "$ISOLATED_PID" 2>/dev/null || true
info "v$ISOLATED_IDX killed (pid=$ISOLATED_PID)"

# Note: leader for view v is (v % n). The killed node is leader for views
# where v % n == ISOLATED_IDX, so those views will time out — that's intentional.
# The pacemaker will advance past them.

H_TARGET=$((H_BEFORE + ISOLATE_BLOCKS))
info "Waiting for remaining validators to reach height $H_TARGET (missing blocks for v$ISOLATED_IDX)..."
if ! wait_for_height "$RPC" "$H_TARGET" $((ISOLATE_BLOCKS * 6 + 30)) "v0"; then
    # Show heights on all surviving nodes before failing
    for i in $(seq 0 $((ISOLATED_IDX - 1))); do
        info "  v$i: height=$(block_number "$((BASE_HTTP + i))")"
    done
    fail "Cluster stalled after isolating v$ISOLATED_IDX — consensus did not continue"
fi

H_AFTER=$(block_number "$RPC")
MISSED=$((H_AFTER - H_BEFORE))
log "Cluster produced $MISSED blocks without v$ISOLATED_IDX (height: $H_BEFORE → $H_AFTER)"

# Verify heights are consistent across the surviving nodes
info "Surviving node heights:"
for i in $(seq 0 $((ISOLATED_IDX - 1))); do
    info "  v$i: $(block_number "$((BASE_HTTP + i))")"
done

# ══════════════════════════════════════════════════════════════════════
# Phase 3 — Restart the isolated validator from persisted state
# ══════════════════════════════════════════════════════════════════════
log "═══ Phase 3: Restarting v$ISOLATED_IDX from persisted state ═══"
info "Data directory preserved: $DATA_DIR/validator-$ISOLATED_IDX"
info "Node will load MDBX snapshot and sync $MISSED missed blocks from peers"

# Remove the killed PID from ALL_PIDS so cleanup doesn't try to kill it again
ALL_PIDS=("${ALL_PIDS[@]/$ISOLATED_PID/}")

# Remove the stale enode entry for the isolated node before re-adding it,
# so start_validator re-appends a fresh entry and doesn't duplicate.
ENODES=("${ENODES[@]:0:$ISOLATED_IDX}" "${ENODES[@]:$((ISOLATED_IDX+1))}")

start_validator "$ISOLATED_IDX"
new_pid=$!
NODE_PIDS[$ISOLATED_IDX]=$new_pid
ALL_PIDS+=("$new_pid")
info "v$ISOLATED_IDX restarted (pid=$new_pid)"

REJOIN_RPC="$((BASE_HTTP + ISOLATED_IDX))"

# Wait for RPC to come up
log "Waiting for v$ISOLATED_IDX RPC..."
waited=0
while (( waited < 60 )); do
    rpc "$REJOIN_RPC" eth_blockNumber '[]' \
        | python3 -c "import sys,json; json.load(sys.stdin)" >/dev/null 2>&1 && break
    sleep 2; (( waited+=2 ))
done

# Wait for it to catch up — sync happens automatically via the chase loop
# (handle_sync_response re-calls initiate_sync while peer_committed_view > local_view + 3)
log "Waiting for v$ISOLATED_IDX to sync..."
SYNC_TIMEOUT=$(( MISSED * 3 + 60 ))   # generous: 3s per missed block + 60s overhead
waited=0
synced=false
while (( waited < SYNC_TIMEOUT )); do
    if ! kill -0 "$new_pid" 2>/dev/null; then
        err "v$ISOLATED_IDX crashed during sync! Last 40 lines of log:"
        tail -40 "$DATA_DIR/validator-$ISOLATED_IDX.log" || true
        fail "Rejoin node crashed"
    fi

    main_h=$(block_number "$RPC")
    rejoin_h=$(block_number "$REJOIN_RPC")
    lag=$(( main_h - rejoin_h ))

    if (( lag <= 3 && rejoin_h > 0 )); then
        synced=true
        log "v$ISOLATED_IDX synced — height=$rejoin_h (main=$main_h, lag=$lag)"
        break
    fi

    (( waited % 15 == 0 )) && info "  v$ISOLATED_IDX syncing: height=$rejoin_h  main=$main_h  lag=$lag  (${waited}s/${SYNC_TIMEOUT}s)"
    sleep 3; (( waited+=3 ))
done

if [[ "$synced" == false ]]; then
    err "v$ISOLATED_IDX failed to sync within ${SYNC_TIMEOUT}s"
    err "Last 40 lines of log:"
    tail -40 "$DATA_DIR/validator-$ISOLATED_IDX.log" || true
    # Don't fail yet — collect final state first
fi

# ══════════════════════════════════════════════════════════════════════
# Phase 4 — Verify participation: all nodes advance together
# ══════════════════════════════════════════════════════════════════════
log "═══ Phase 4: Verifying participation after rejoin ═══"

# Wait for NUM_VALIDATORS more blocks — covers at least one full leader-rotation
# cycle, so v$ISOLATED_IDX will be scheduled as leader at least once.
PARTICIPATION_BLOCKS=$((NUM_VALIDATORS * 2))
cur_h=$(block_number "$RPC")
info "Waiting $PARTICIPATION_BLOCKS more blocks for full leader-rotation coverage..."
wait_for_height "$RPC" $((cur_h + PARTICIPATION_BLOCKS)) $((PARTICIPATION_BLOCKS * 6)) "v0" || \
    warn "Participation wait timed out"

# ── Final state snapshot ───────────────────────────────────────────────
log "Collecting final heights and block hashes..."
declare -a FINAL_HEIGHTS=()
all_alive=true
for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
    if ! kill -0 "${NODE_PIDS[$i]}" 2>/dev/null; then
        err "  v$i: DEAD"
        all_alive=false
        FINAL_HEIGHTS+=(0)
        continue
    fi
    h=$(block_number "$((BASE_HTTP + i))")
    FINAL_HEIGHTS+=("$h")
    info "  v$i: height=$h"
done

min_h=${FINAL_HEIGHTS[0]}; max_h=${FINAL_HEIGHTS[0]}
for h in "${FINAL_HEIGHTS[@]}"; do
    (( h < min_h )) && min_h=$h
    (( h > max_h )) && max_h=$h
done
height_skew=$(( max_h - min_h ))

# Block hash at the common minimum height — detects forks
ref_hash=$(block_hash_at "$RPC" "$min_h")
info "Reference hash at height $min_h: ${ref_hash:0:20}..."
hash_ok=true
for i in $(seq 1 $((NUM_VALIDATORS - 1))); do
    p=$((BASE_HTTP + i))
    h=$(block_hash_at "$p" "$min_h")
    if [[ -n "$ref_hash" && "$h" != "$ref_hash" ]]; then
        err "  v$i FORK DETECTED at height $min_h: $h"
        hash_ok=false
    fi
done

# ══════════════════════════════════════════════════════════════════════
# Result
# ══════════════════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════════════════"
echo "  Rejoin test ($NUM_VALIDATORS nodes, $ISOLATE_BLOCKS blocks missed by v$ISOLATED_IDX)"
echo "════════════════════════════════════════════════════════════════"

PASS=true

# 1. Did the cluster keep making blocks while the node was isolated?
if (( MISSED >= ISOLATE_BLOCKS )); then
    echo -e "  ${GREEN}✓${NC} Cluster continued producing blocks during isolation ($H_BEFORE → $H_AFTER, $MISSED blocks)"
else
    echo -e "  ${RED}✗${NC} Cluster produced only $MISSED blocks (expected >= $ISOLATE_BLOCKS)"
    PASS=false
fi

# 2. Did the restarted node sync?
rejoin_final=$(block_number "$REJOIN_RPC")
if [[ "$synced" == true ]]; then
    echo -e "  ${GREEN}✓${NC} v$ISOLATED_IDX synced from MDBX snapshot (final height: $rejoin_final)"
else
    echo -e "  ${RED}✗${NC} v$ISOLATED_IDX did not catch up (height=$rejoin_final, main=$max_h)"
    PASS=false
fi

# 3. All nodes alive?
if [[ "$all_alive" == true ]]; then
    echo -e "  ${GREEN}✓${NC} All $NUM_VALIDATORS nodes alive"
else
    echo -e "  ${RED}✗${NC} One or more nodes crashed"
    PASS=false
fi

# 4. Height consistency?
if (( height_skew <= 3 && min_h > 0 )); then
    echo -e "  ${GREEN}✓${NC} Height consistency: skew=${height_skew} blocks (${min_h}–${max_h})"
else
    echo -e "  ${RED}✗${NC} Height inconsistency: skew=${height_skew} blocks (${min_h}–${max_h})"
    PASS=false
fi

# 5. No forks?
if [[ "$hash_ok" == true && -n "$ref_hash" ]]; then
    echo -e "  ${GREEN}✓${NC} No fork: all nodes agree on hash at height $min_h"
else
    echo -e "  ${RED}✗${NC} Fork detected — nodes disagree on block hash at height $min_h"
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
