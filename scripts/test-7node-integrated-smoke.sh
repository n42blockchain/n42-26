#!/usr/bin/env bash
#
# 7-node integrated smoke test:
# - starts 7 real n42-node validators
# - optionally runs tx generator, mobile simulator, and Blockscout
# - validates height/hash consistency plus service-specific health signals
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

NUM_VALIDATORS=7
RUN_DURATION=30
TX_RATE=2
WAIT_FOR_BLOCKS_SECS=120
WAIT_FOR_PHASE2_SECS=180
WAIT_FOR_BLOCKSCOUT_SECS=180

ARTIFACT_DIR="$PROJECT_DIR/.artifacts/7node-integrated-smoke-$(date +%Y%m%d-%H%M%S)"
GENESIS_SOURCE=""
FORCE_BUILD=false
WITH_TX_GEN=true
WITH_MOBILE=true
WITH_BLOCKSCOUT=true

BASE_HTTP_RPC=28100
BASE_WS=28400
BASE_AUTH=28500
BASE_P2P=30400
BASE_CONSENSUS=19500
BASE_STARHUB=19600
BASE_METRICS=19100
BLOCKSCOUT_PORT=5082

BLOCK_INTERVAL_MS=2000
BASE_TIMEOUT_MS=10000
MAX_TIMEOUT_MS=30000
STARTUP_DELAY_MS=3000

NODE_BIN=""
MOBILE_BIN=""
PYTHON_BIN=""
PIP_BIN=""
VENV_DIR=""
BLOCKSCOUT_PROJECT=""
GENESIS_FILE=""

declare -a NODE_PIDS=()
declare -a ENODES=()
declare -a P2P_SECRETS=()
declare -a STARHUB_PORTS=()
declare -a HEIGHTS=()

TX_GEN_PID=""
MOBILE_PID=""
ALL_ALIVE=0
MIN_HEIGHT=0
MAX_HEIGHT=0
HASH_OK=0
REF_HASH=""

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${GREEN}[7node-smoke]${NC} $*"; }
warn() { echo -e "${YELLOW}[7node-smoke]${NC} $*"; }
err()  { echo -e "${RED}[7node-smoke]${NC} $*"; }
info() { echo -e "${CYAN}[7node-smoke]${NC} $*"; }

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --artifact-dir DIR      Write logs/results under DIR
  --genesis PATH          Use an existing genesis.json
  --duration SECONDS      Active smoke duration after startup (default: ${RUN_DURATION})
  --tx-rate TPS           TX generator rate when enabled (default: ${TX_RATE})
  --build                 Force rebuild of n42-node / n42-mobile-sim
  --no-tx-gen             Skip tx generator
  --no-mobile             Skip mobile simulator
  --no-blockscout         Skip Blockscout
  --blockscout-port PORT  Host port for Blockscout nginx (default: ${BLOCKSCOUT_PORT})
  -h, --help              Show this help
EOF
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --artifact-dir)
                ARTIFACT_DIR="$2"
                shift 2
                ;;
            --genesis)
                GENESIS_SOURCE="$2"
                shift 2
                ;;
            --duration)
                RUN_DURATION="$2"
                shift 2
                ;;
            --tx-rate)
                TX_RATE="$2"
                shift 2
                ;;
            --build)
                FORCE_BUILD=true
                shift
                ;;
            --no-tx-gen)
                WITH_TX_GEN=false
                shift
                ;;
            --no-mobile)
                WITH_MOBILE=false
                shift
                ;;
            --no-blockscout)
                WITH_BLOCKSCOUT=false
                shift
                ;;
            --blockscout-port)
                BLOCKSCOUT_PORT="$2"
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                err "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done
}

cleanup() {
    local exit_code=$?

    if [[ -n "$TX_GEN_PID" ]]; then
        kill "$TX_GEN_PID" 2>/dev/null || true
    fi
    if [[ -n "$MOBILE_PID" ]]; then
        kill "$MOBILE_PID" 2>/dev/null || true
    fi
    for pid in "${NODE_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done

    if [[ -n "$BLOCKSCOUT_PROJECT" ]] && [[ -f "$ARTIFACT_DIR/blockscout-override.yml" ]]; then
        docker compose \
            -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -f "$ARTIFACT_DIR/blockscout-override.yml" \
            -p "$BLOCKSCOUT_PROJECT" \
            down -v >/dev/null 2>&1 || true
    fi

    wait 2>/dev/null || true
    exit "$exit_code"
}

setup_python_venv() {
    VENV_DIR="$ARTIFACT_DIR/.venv"
    PYTHON_BIN="$VENV_DIR/bin/python3"
    PIP_BIN="$VENV_DIR/bin/pip"

    if [[ ! -d "$VENV_DIR" ]]; then
        log "Creating Python virtual environment at $VENV_DIR"
        python3 -m venv "$VENV_DIR"
    fi

    if ! "$PYTHON_BIN" -c 'import eth_account, eth_keys, eth_utils' >/dev/null 2>&1; then
        log "Installing Python dependencies"
        "$PIP_BIN" install --quiet -r "$SCRIPT_DIR/requirements-testnet.txt"
    fi
}

ensure_binaries() {
    NODE_BIN="$PROJECT_DIR/target/release/n42-node"
    MOBILE_BIN="$PROJECT_DIR/target/release/n42-mobile-sim"

    local need_build=false
    if [[ "$FORCE_BUILD" == true ]] || [[ ! -x "$NODE_BIN" ]]; then
        need_build=true
    fi
    if [[ "$WITH_MOBILE" == true ]] && ([[ "$FORCE_BUILD" == true ]] || [[ ! -x "$MOBILE_BIN" ]]); then
        need_build=true
    fi

    if [[ "$need_build" == true ]]; then
        log "Building required release binaries"
        local build_cmd=(cargo build --release --bin n42-node)
        if [[ "$WITH_MOBILE" == true ]]; then
            build_cmd+=(--bin n42-mobile-sim)
        fi
        (
            cd "$PROJECT_DIR"
            "${build_cmd[@]}"
        )
    else
        info "Reusing existing release binaries"
    fi
}

prepare_genesis() {
    GENESIS_FILE="$ARTIFACT_DIR/genesis.json"

    if [[ -n "$GENESIS_SOURCE" ]]; then
        cp "$GENESIS_SOURCE" "$GENESIS_FILE"
        return
    fi

    if [[ -f "$HOME/n42-testnet-data/genesis.json" ]]; then
        cp "$HOME/n42-testnet-data/genesis.json" "$GENESIS_FILE"
        return
    fi

    log "Generating fallback genesis.json"
    DATA_DIR="$ARTIFACT_DIR" ACCOUNTS_CACHE="$SCRIPT_DIR/testnet-accounts-cache.json" "$PYTHON_BIN" <<'PYEOF'
import json
import os
from eth_account import Account
from eth_utils import keccak

NUM_ACCOUNTS = 5000
CHAIN_ID = 4242
INITIAL_BALANCE = "0x4B3B4CA85A86C47A098A224000000"
data_dir = os.environ["DATA_DIR"]
cache_file = os.environ.get("ACCOUNTS_CACHE", "")

if cache_file and os.path.exists(cache_file):
    with open(cache_file) as fh:
        accounts = json.load(fh)
else:
    accounts = []
    for i in range(NUM_ACCOUNTS):
        private_key = keccak(f"n42-test-key-{i}".encode())
        acct = Account.from_key(private_key)
        accounts.append({"key": private_key.hex(), "address": acct.address})

genesis = {
    "config": {
        "chainId": CHAIN_ID,
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
    "gasLimit": hex(2_000_000_000), "difficulty": "0x0",
    "mixHash": "0x" + "0" * 64,
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc": {"balance": "0x60EF6B1ABA6F072330000000"},
        **{a["address"]: {"balance": INITIAL_BALANCE} for a in accounts},
    },
    "number": "0x0", "gasUsed": "0x0",
    "parentHash": "0x" + "0" * 64,
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0", "blobGasUsed": "0x0",
}

with open(os.path.join(data_dir, "genesis.json"), "w") as fh:
    json.dump(genesis, fh, indent=2)
PYEOF
}

compute_devp2p_keys() {
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        read -r secret pubkey <<< "$("$PYTHON_BIN" - "$i" <<'PYEOF'
from eth_utils import keccak
from eth_keys import keys
import sys

idx = int(sys.argv[1])
sk = keccak(f"n42-devp2p-key-{idx}".encode())
pk = keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
PYEOF
)"
        P2P_SECRETS+=("$secret")
        ENODES+=("enode://${pubkey}@127.0.0.1:$((BASE_P2P + i))")
        STARHUB_PORTS+=("$((BASE_STARHUB + i))")
    done
}

rpc_call() {
    local port="$1"
    local method="$2"
    local params="$3"
    curl -s --max-time 5 -X POST -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}" \
        "http://127.0.0.1:${port}/" 2>/dev/null || true
}

rpc_block_number() {
    local out
    out="$(rpc_call "$1" eth_blockNumber '[]')"
    python3 -c 'import json,sys
s = sys.stdin.read().strip()
try:
    body = json.loads(s) if s else {}
    print(int(body.get("result", "0x0"), 16))
except Exception:
    print(0)' <<<"$out"
}

rpc_block_hash() {
    local port="$1"
    local height="$2"
    local out
    out="$(rpc_call "$port" eth_getBlockByNumber "[\"0x$(printf '%x' "$height")\",false]")"
    python3 -c 'import json,sys
s = sys.stdin.read().strip()
try:
    body = json.loads(s) if s else {}
    print((body.get("result") or {}).get("hash") or "")
except Exception:
    print("")' <<<"$out"
}

json_value() {
    local file="$1"
    local path="$2"
    local default_value="${3:-}"
    python3 - "$file" "$path" "$default_value" <<'PYEOF'
import json
import sys
from pathlib import Path

file, path, default = sys.argv[1:]
try:
    data = json.loads(Path(file).read_text())
except Exception:
    print(default)
    raise SystemExit(0)

current = data
for part in path.split("."):
    if not part:
        continue
    if isinstance(current, dict):
        current = current.get(part)
    else:
        current = None
        break

if current is None:
    print(default)
else:
    print(current)
PYEOF
}

start_nodes() {
    log "Starting ${NUM_VALIDATORS} validator nodes"
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        local datadir="$ARTIFACT_DIR/validator-$i"
        local http_port=$((BASE_HTTP_RPC + i))
        local ws_port=$((BASE_WS + i))
        local auth_port=$((BASE_AUTH + i))
        local p2p_port=$((BASE_P2P + i))
        local consensus_port=$((BASE_CONSENSUS + i))
        local starhub_port=$((BASE_STARHUB + i))
        local metrics_port=$((BASE_METRICS + i))
        local validator_key
        validator_key="$(printf '%056x%08x' 0 $((i + 1)))"
        mkdir -p "$datadir/logs"

        local trusted_peers=()
        local trusted_csv=""
        if (( i > 0 )); then
            for j in $(seq 0 $((i - 1))); do
                trusted_peers+=("${ENODES[$j]}")
            done
            trusted_csv="$(IFS=,; echo "${trusted_peers[*]}")"
        fi

        local cmd=(
            "$NODE_BIN" node
            --chain "$GENESIS_FILE"
            --datadir "$datadir"
            --http
            --http.port "$http_port"
            --http.api eth,net,web3,txpool,rpc,debug,trace
            --http.corsdomain '*'
            --ws
            --ws.port "$ws_port"
            --ws.api eth,net,web3,debug,trace
            --authrpc.port "$auth_port"
            --port "$p2p_port"
            --discovery.port "$p2p_port"
            --log.file.directory "$datadir/logs"
            --ipcdisable
            --max-outbound-peers "$NUM_VALIDATORS"
            --max-inbound-peers "$NUM_VALIDATORS"
            --metrics "127.0.0.1:$metrics_port"
            --builder.gaslimit 2000000000
            --builder.interval 50ms
            --rpc.max-request-size 50
            --rpc.max-response-size 50
            --disable-tx-gossip
            --p2p-secret-key-hex "${P2P_SECRETS[$i]}"
        )
        if [[ -n "$trusted_csv" ]]; then
            cmd+=(--trusted-peers "$trusted_csv")
        fi

        env \
            RUST_LOG='info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error' \
            N42_VALIDATOR_KEY="$validator_key" \
            N42_VALIDATOR_COUNT="$NUM_VALIDATORS" \
            N42_ENABLE_MDNS='false' \
            N42_DATA_DIR="$datadir" \
            N42_CONSENSUS_PORT="$consensus_port" \
            N42_STARHUB_PORT="$starhub_port" \
            N42_MAX_EMPTY_SKIPS='0' \
            N42_BLOCK_INTERVAL_MS="$BLOCK_INTERVAL_MS" \
            N42_BASE_TIMEOUT_MS="$BASE_TIMEOUT_MS" \
            N42_MAX_TIMEOUT_MS="$MAX_TIMEOUT_MS" \
            N42_STARTUP_DELAY_MS="$STARTUP_DELAY_MS" \
            N42_REWARD_EPOCH_BLOCKS='50' \
            N42_DAILY_BASE_REWARD_GWEI='100000000' \
            N42_REWARD_CURVE_K='4.0' \
            N42_MIN_ATTESTATION_THRESHOLD='1' \
            N42_OPEN_VERIFICATION='1' \
            N42_MAX_TXS_PER_BLOCK='48000' \
            N42_FAST_PROPOSE='0' \
            N42_MIN_PROPOSE_DELAY_MS='0' \
            N42_SKIP_TX_VERIFY='0' \
            N42_POOL_VALIDATION_THREADS='2' \
            "${cmd[@]}" > "$ARTIFACT_DIR/validator-$i.log" 2>&1 &

        NODE_PIDS+=("$!")
        sleep 2
    done
}

wait_for_blocks() {
    log "Waiting for first block"
    local deadline=$((SECONDS + WAIT_FOR_BLOCKS_SECS))
    local height0=0

    while (( SECONDS < deadline )); do
        height0="$(rpc_block_number "$BASE_HTTP_RPC")"
        if (( height0 > 0 )); then
            info "Chain active at block $height0"
            return 0
        fi
        sleep 3
    done

    return 1
}

start_tx_generator() {
    if [[ "$WITH_TX_GEN" != true ]]; then
        return
    fi
    log "Starting tx generator at ${TX_RATE} tx/s"
    env PYTHONUNBUFFERED=1 \
        "$PYTHON_BIN" "$SCRIPT_DIR/tx-load-generator.py" \
        --rpc "http://127.0.0.1:${BASE_HTTP_RPC}" \
        --rate "$TX_RATE" \
        > "$ARTIFACT_DIR/tx-generator.log" 2>&1 &
    TX_GEN_PID="$!"
}

wait_for_tx_phase2() {
    if [[ "$WITH_TX_GEN" != true ]]; then
        echo "skipped"
        return
    fi

    local deadline=$((SECONDS + WAIT_FOR_PHASE2_SECS))
    while (( SECONDS < deadline )); do
        if grep -q "Phase 2: Starting continuous TX generation" "$ARTIFACT_DIR/tx-generator.log" 2>/dev/null; then
            echo "ready"
            return
        fi
        sleep 5
    done
    echo "timeout"
}

start_mobile_sim() {
    if [[ "$WITH_MOBILE" != true ]]; then
        return
    fi
    local star_ports_csv
    star_ports_csv="$(IFS=,; echo "${STARHUB_PORTS[*]}")"
    log "Starting mobile simulator on ports ${star_ports_csv}"
    "$MOBILE_BIN" \
        --starhub-ports "$star_ports_csv" \
        --phone-count "$NUM_VALIDATORS" \
        --duration "$RUN_DURATION" \
        > "$ARTIFACT_DIR/mobile-sim.log" 2>&1 &
    MOBILE_PID="$!"
}

start_blockscout() {
    if [[ "$WITH_BLOCKSCOUT" != true ]]; then
        return
    fi
    if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
        err "Docker is required for Blockscout; rerun with --no-blockscout or start Docker"
        exit 1
    fi

    BLOCKSCOUT_PROJECT="n42-smoke-7node-$(date +%H%M%S)"
    cat > "$ARTIFACT_DIR/blockscout-override.yml" <<EOF
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
      NEXT_PUBLIC_APP_PORT: "${BLOCKSCOUT_PORT}"
      NEXT_PUBLIC_API_HOST: localhost
      NEXT_PUBLIC_API_PORT: "${BLOCKSCOUT_PORT}"
  nginx:
    ports:
      - "${BLOCKSCOUT_PORT}:80"
EOF

    log "Starting Blockscout on port ${BLOCKSCOUT_PORT}"
    if ! docker compose \
        -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
        -f "$ARTIFACT_DIR/blockscout-override.yml" \
        -p "$BLOCKSCOUT_PROJECT" \
        up -d > "$ARTIFACT_DIR/blockscout-compose.log" 2>&1; then
        warn "Blockscout failed to start"
        return
    fi
}

wait_for_blockscout() {
    if [[ "$WITH_BLOCKSCOUT" != true ]]; then
        echo "skipped"
        return
    fi

    local deadline=$((SECONDS + WAIT_FOR_BLOCKSCOUT_SECS))
    while (( SECONDS < deadline )); do
        local code
        code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${BLOCKSCOUT_PORT}/" || true)"
        if [[ "$code" == "200" || "$code" == "302" ]]; then
            curl -s "http://127.0.0.1:${BLOCKSCOUT_PORT}/" > "$ARTIFACT_DIR/blockscout-root.html" || true
            echo "ready"
            return
        fi
        sleep 5
    done
    echo "timeout"
}

wait_for_active_window() {
    local mobile_ok="skipped"

    if [[ "$WITH_MOBILE" == true ]]; then
        if wait "$MOBILE_PID"; then
            mobile_ok="ready"
        else
            mobile_ok="failed"
        fi
        MOBILE_PID=""
    else
        sleep "$RUN_DURATION"
    fi

    echo "$mobile_ok"
}

stop_tx_generator() {
    if [[ -n "$TX_GEN_PID" ]]; then
        kill -INT "$TX_GEN_PID" 2>/dev/null || true
        wait "$TX_GEN_PID" 2>/dev/null || true
        TX_GEN_PID=""
    fi
}

collect_status_files() {
    rpc_call "$BASE_HTTP_RPC" n42_attestationStats '[]' > "$ARTIFACT_DIR/attestation-stats.json"
    rpc_call "$BASE_HTTP_RPC" n42_consensusStatus '[]' > "$ARTIFACT_DIR/consensus-status.json"
    rpc_call "$BASE_HTTP_RPC" txpool_status '[]' > "$ARTIFACT_DIR/txpool-status.json"
}

collect_log_summaries() {
    rg -n "ERROR|panic|fatal|failed to forward tx batch|tx import channel full|Failed to compute state root in parallel" \
        "$ARTIFACT_DIR"/validator-*.log > "$ARTIFACT_DIR/node-errors.txt" || true
    rg -n "mobile stream packet broadcast|block reached mobile attestation threshold|block attestation threshold reached|verification task broadcast" \
        "$ARTIFACT_DIR"/validator-*.log > "$ARTIFACT_DIR/mobile-events.txt" || true
    if [[ -f "$ARTIFACT_DIR/tx-generator.log" ]]; then
        rg -n "Phase 1: Deploying TestUSDT|TestUSDT deployed|Phase 2: Starting continuous TX generation|STATS sent=|TX generator shutdown complete|ERROR|Traceback" \
            "$ARTIFACT_DIR/tx-generator.log" > "$ARTIFACT_DIR/tx-generator-events.txt" || true
    fi
    if [[ "$WITH_BLOCKSCOUT" == true ]] && [[ -n "$BLOCKSCOUT_PROJECT" ]]; then
        docker compose \
            -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -f "$ARTIFACT_DIR/blockscout-override.yml" \
            -p "$BLOCKSCOUT_PROJECT" \
            ps > "$ARTIFACT_DIR/blockscout-ps.txt" 2>&1 || true
    fi
}

collect_consistency_status() {
    ALL_ALIVE=1
    MIN_HEIGHT=999999999
    MAX_HEIGHT=0

    HEIGHTS=()
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        if ! kill -0 "${NODE_PIDS[$i]}" 2>/dev/null; then
            ALL_ALIVE=0
        fi
        local height
        height="$(rpc_block_number "$((BASE_HTTP_RPC + i))")"
        HEIGHTS+=("$height")
        if (( height < MIN_HEIGHT )); then
            MIN_HEIGHT=$height
        fi
        if (( height > MAX_HEIGHT )); then
            MAX_HEIGHT=$height
        fi
    done

    REF_HASH="$(rpc_block_hash "$BASE_HTTP_RPC" "$MIN_HEIGHT")"
    HASH_OK=1
    for i in $(seq 1 $((NUM_VALIDATORS - 1))); do
        local height_hash
        height_hash="$(rpc_block_hash "$((BASE_HTTP_RPC + i))" "$MIN_HEIGHT")"
        if [[ -n "$REF_HASH" && "$height_hash" != "$REF_HASH" ]]; then
            HASH_OK=0
        fi
    done
}

write_summary() {
    local phase2_status="$1"
    local mobile_status="$2"
    local blockscout_status="$3"
    local blockscout_rpc_status="$4"
    local all_alive="$5"
    local min_height="$6"
    local max_height="$7"
    local hash_ok="$8"
    local ref_hash="$9"

    local att_total
    att_total="$(json_value "$ARTIFACT_DIR/attestation-stats.json" "result.totalAttestations" "0")"
    local att_earliest
    att_earliest="$(json_value "$ARTIFACT_DIR/attestation-stats.json" "result.earliestBlock" "")"
    local att_latest
    att_latest="$(json_value "$ARTIFACT_DIR/attestation-stats.json" "result.latestBlock" "")"
    local has_committed_qc
    has_committed_qc="$(json_value "$ARTIFACT_DIR/consensus-status.json" "result.hasCommittedQc" "false")"
    local latest_view
    latest_view="$(json_value "$ARTIFACT_DIR/consensus-status.json" "result.latestCommittedView" "")"
    local pending
    pending="$(json_value "$ARTIFACT_DIR/txpool-status.json" "result.pending" "")"
    local queued
    queued="$(json_value "$ARTIFACT_DIR/txpool-status.json" "result.queued" "")"
    local error_count
    error_count="$(wc -l < "$ARTIFACT_DIR/node-errors.txt" 2>/dev/null || echo 0)"
    local mobile_events
    mobile_events="$(wc -l < "$ARTIFACT_DIR/mobile-events.txt" 2>/dev/null || echo 0)"

    cat > "$ARTIFACT_DIR/SUMMARY.txt" <<EOF
artifact_dir=$ARTIFACT_DIR
phase2_status=$phase2_status
mobile_status=$mobile_status
blockscout_status=$blockscout_status
blockscout_rpc_status=$blockscout_rpc_status
all_alive=$all_alive
heights=${HEIGHTS[*]}
min_height=$min_height
max_height=$max_height
divergence=$((max_height - min_height))
hash_ok=$hash_ok
hash=$ref_hash
attestation_total=$att_total
attestation_earliest=$att_earliest
attestation_latest=$att_latest
mobile_event_lines=$mobile_events
has_committed_qc=$has_committed_qc
latest_committed_view=$latest_view
txpool_pending=$pending
txpool_queued=$queued
node_error_lines=$error_count
EOF
}

main() {
    parse_args "$@"
    mkdir -p "$ARTIFACT_DIR"
    trap cleanup EXIT INT TERM

    log "Artifact directory: $ARTIFACT_DIR"
    setup_python_venv
    ensure_binaries
    prepare_genesis
    compute_devp2p_keys
    start_nodes

    if ! wait_for_blocks; then
        err "No blocks produced within ${WAIT_FOR_BLOCKS_SECS}s"
        tail -n 40 "$ARTIFACT_DIR"/validator-*.log || true
        exit 1
    fi

    start_tx_generator
    start_mobile_sim
    start_blockscout

    local phase2_status
    phase2_status="$(wait_for_tx_phase2)"
    local blockscout_status
    blockscout_status="$(wait_for_blockscout)"
    local mobile_status
    mobile_status="$(wait_for_active_window)"
    stop_tx_generator

    collect_status_files
    collect_log_summaries

    local blockscout_rpc_status="skipped"
    if [[ "$WITH_BLOCKSCOUT" == true ]]; then
        if bash "$SCRIPT_DIR/verify-blockscout-rpc.sh" \
            "http://127.0.0.1:${BASE_HTTP_RPC}" \
            "ws://127.0.0.1:${BASE_WS}" > "$ARTIFACT_DIR/blockscout-rpc-check.txt" 2>&1; then
            blockscout_rpc_status="ready"
        else
            blockscout_rpc_status="failed"
        fi
    fi

    collect_consistency_status
    write_summary \
        "$phase2_status" \
        "$mobile_status" \
        "$blockscout_status" \
        "$blockscout_rpc_status" \
        "$ALL_ALIVE" \
        "$MIN_HEIGHT" \
        "$MAX_HEIGHT" \
        "$HASH_OK" \
        "$REF_HASH"

    local att_total
    att_total="$(json_value "$ARTIFACT_DIR/attestation-stats.json" "result.totalAttestations" "0")"

    info "Summary: heights=${HEIGHTS[*]} divergence=$((MAX_HEIGHT - MIN_HEIGHT)) attestation_total=${att_total}"
    info "Services: phase2=${phase2_status} mobile=${mobile_status} blockscout=${blockscout_status} blockscout_rpc=${blockscout_rpc_status}"

    if (( ALL_ALIVE != 1 )) || (( MIN_HEIGHT < 2 )) || (( MAX_HEIGHT - MIN_HEIGHT > 1 )) || (( HASH_OK != 1 )); then
        err "Consensus consistency checks failed"
        exit 1
    fi
    if [[ "$WITH_TX_GEN" == true && "$phase2_status" != "ready" ]]; then
        err "TX generator did not reach Phase 2"
        exit 1
    fi
    if [[ "$WITH_MOBILE" == true && "$mobile_status" != "ready" ]]; then
        err "Mobile simulator did not complete successfully"
        exit 1
    fi
    if [[ "$WITH_MOBILE" == true && "$att_total" == "0" ]]; then
        err "No mobile attestations were recorded"
        exit 1
    fi
    if [[ "$WITH_BLOCKSCOUT" == true && "$blockscout_status" != "ready" ]]; then
        err "Blockscout did not become reachable"
        exit 1
    fi
    if [[ "$WITH_BLOCKSCOUT" == true && "$blockscout_rpc_status" != "ready" ]]; then
        err "Blockscout RPC compatibility check failed"
        exit 1
    fi

    log "Integrated smoke passed"
}

main "$@"
