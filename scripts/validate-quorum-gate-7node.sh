#!/usr/bin/env bash
#
# Behavioral validation for the leader-proposal quorum gate.
#
# Runs local real n42-node processes and writes logs / parsed summaries under
# .artifacts/quorum-gate-7node-<timestamp>.
#
# Scenarios:
#   - baseline: all 7 validators start together
#   - staggered: validators 0,1,2 start, wait 15s, then 3,4,5,6
#   - leader-first: validator 1 (view-1 leader) starts alone, wait 15s, then rest
#   - partition: start 7, wait for blocks, stop 3 validators, then restore them
#   - single: n=1 immediate proposal sanity

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

NODE_BIN="${NODE_BIN:-$PROJECT_DIR/target/release/n42-node}"
ARTIFACT_ROOT="${ARTIFACT_ROOT:-$PROJECT_DIR/.artifacts/quorum-gate-7node-$(date +%Y%m%d-%H%M%S)}"

BASE_HTTP=29100
BASE_WS=29200
BASE_AUTH=29300
BASE_P2P=29400
BASE_CONSENSUS=29500
BASE_STARHUB=29600
BASE_METRICS=29700

BLOCK_INTERVAL_MS="${BLOCK_INTERVAL_MS:-1000}"
BASE_TIMEOUT_MS="${BASE_TIMEOUT_MS:-30000}"
MAX_TIMEOUT_MS="${MAX_TIMEOUT_MS:-60000}"
STARTUP_DELAY_MS="${STARTUP_DELAY_MS:-500}"
STAGGER_WAIT_SECS="${STAGGER_WAIT_SECS:-15}"
PARTITION_WAIT_SECS="${PARTITION_WAIT_SECS:-12}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log() { echo -e "${GREEN}[quorum-gate]${NC} $*"; }
info() { echo -e "${CYAN}[quorum-gate]${NC} $*"; }
warn() { echo -e "${YELLOW}[quorum-gate]${NC} $*"; }
err() { echo -e "${RED}[quorum-gate]${NC} $*" >&2; }

mkdir -p "$ARTIFACT_ROOT"

if [[ ! -x "$NODE_BIN" ]]; then
    err "n42-node binary not found: $NODE_BIN"
    err "Build first: cargo build --release --bin n42-node"
    exit 1
fi

PYTHON_BIN="$ARTIFACT_ROOT/.venv/bin/python3"
PIP_BIN="$ARTIFACT_ROOT/.venv/bin/pip"
if [[ ! -x "$PYTHON_BIN" ]]; then
    python3 -m venv "$ARTIFACT_ROOT/.venv"
fi
if ! "$PYTHON_BIN" -c 'import eth_keys, eth_utils' >/dev/null 2>&1; then
    "$PIP_BIN" install --quiet -r "$SCRIPT_DIR/requirements-testnet.txt"
fi

declare -a PIDS=()

cleanup_all() {
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup_all EXIT

bls_secret_hex() {
    local idx="$1"
    printf '%056x%08x' 0 $((idx + 1))
}

generate_genesis() {
    local out="$1"
    "$PYTHON_BIN" - "$out" <<'PY'
import json, os, sys
out = sys.argv[1]
genesis = {
    "config": {
        "chainId": 4242,
        "homesteadBlock": 0,
        "eip150Block": 0,
        "eip155Block": 0,
        "eip158Block": 0,
        "byzantiumBlock": 0,
        "constantinopleBlock": 0,
        "petersburgBlock": 0,
        "istanbulBlock": 0,
        "muirGlacierBlock": 0,
        "berlinBlock": 0,
        "londonBlock": 0,
        "arrowGlacierBlock": 0,
        "grayGlacierBlock": 0,
        "mergeNetsplitBlock": 0,
        "shanghaiTime": 0,
        "cancunTime": 0,
        "terminalTotalDifficulty": "0x0",
        "terminalTotalDifficultyPassed": True,
    },
    "nonce": "0x0",
    "timestamp": "0x0",
    "extraData": "0x",
    "gasLimit": hex(int(os.environ.get("N42_GAS_LIMIT", "2000000000"))),
    "difficulty": "0x0",
    "mixHash": "0x" + "0" * 64,
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc": {
            "balance": "0x60EF6B1ABA6F072330000000"
        }
    },
    "number": "0x0",
    "gasUsed": "0x0",
    "parentHash": "0x" + "0" * 64,
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0",
    "blobGasUsed": "0x0",
}
with open(out, "w") as fh:
    json.dump(genesis, fh, indent=2)
PY
}

generate_p2p_keys() {
    local count="$1"
    local out="$2"
    "$PYTHON_BIN" - "$count" "$out" <<'PY'
from eth_utils import keccak
from eth_keys import keys
import json, sys
count = int(sys.argv[1])
out = sys.argv[2]
rows = []
for idx in range(count):
    sk = keccak(f"n42-devp2p-key-{idx}".encode())
    pk = keys.PrivateKey(sk)
    rows.append({"secret": sk.hex(), "pubkey": pk.public_key.to_hex()[2:]})
with open(out, "w") as fh:
    json.dump(rows, fh, indent=2)
PY
}

json_key() {
    local file="$1"
    local idx="$2"
    local key="$3"
    "$PYTHON_BIN" - "$file" "$idx" "$key" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
print(data[int(sys.argv[2])][sys.argv[3]])
PY
}

rpc_call() {
    local port="$1"
    local method="$2"
    local params="${3:-[]}"
    curl -s --max-time 3 -X POST -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}" \
        "http://127.0.0.1:${port}/" 2>/dev/null || true
}

block_number() {
    local port="$1"
    local out
    out="$(rpc_call "$port" eth_blockNumber '[]')"
    "$PYTHON_BIN" -c 'import json, sys
try:
    print(int((json.loads(sys.argv[1] or "{}").get("result") or "0x0"), 16))
except Exception:
    print(0)
' "$out"
}

wait_rpc() {
    local port="$1"
    local deadline=$((SECONDS + 60))
    while (( SECONDS < deadline )); do
        if [[ "$(rpc_call "$port" web3_clientVersion '[]')" == *"result"* ]]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

wait_height_gt() {
    local port="$1"
    local min_height="$2"
    local timeout="$3"
    local deadline=$((SECONDS + timeout))
    local h=0
    while (( SECONDS < deadline )); do
        h="$(block_number "$port")"
        if (( h > min_height )); then
            echo "$h"
            return 0
        fi
        sleep 1
    done
    echo "$h"
    return 1
}

start_node() {
    local dir="$1"
    local count="$2"
    local idx="$3"
    local p2p_json="$4"
    local genesis="$5"
    local port_shift="$6"

    local datadir="$dir/validator-$idx"
    mkdir -p "$datadir/logs"

    local http_port=$((BASE_HTTP + port_shift + idx))
    local ws_port=$((BASE_WS + port_shift + idx))
    local auth_port=$((BASE_AUTH + port_shift + idx))
    local p2p_port=$((BASE_P2P + port_shift + idx))
    local consensus_port=$((BASE_CONSENSUS + port_shift + idx))
    local starhub_port=$((BASE_STARHUB + port_shift + idx))
    local metrics_port=$((BASE_METRICS + port_shift + idx))
    local p2p_secret
    p2p_secret="$(json_key "$p2p_json" "$idx" secret)"

    env \
        RUST_LOG='info,n42::cl::orchestrator=debug,n42::cl::consensus_loop=debug,n42::cl::exec_bridge=debug,n42::cl::voting=debug,n42::cl::timeout=debug,libp2p_mdns=off,libp2p_gossipsub::behaviour=error' \
        N42_VALIDATOR_KEY="$(bls_secret_hex "$idx")" \
        N42_VALIDATOR_COUNT="$count" \
        N42_ALLOW_DETERMINISTIC_P2P='1' \
        N42_ENABLE_MDNS='false' \
        N42_ENABLE_DHT='false' \
        N42_DATA_DIR="$datadir" \
        N42_CONSENSUS_PORT="$consensus_port" \
        N42_STARHUB_PORT="$starhub_port" \
        N42_MAX_EMPTY_SKIPS='0' \
        N42_BLOCK_INTERVAL_MS="$BLOCK_INTERVAL_MS" \
        N42_BASE_TIMEOUT_MS="$BASE_TIMEOUT_MS" \
        N42_MAX_TIMEOUT_MS="$MAX_TIMEOUT_MS" \
        N42_STARTUP_DELAY_MS="$STARTUP_DELAY_MS" \
        N42_FAST_PROPOSE='0' \
        N42_MIN_PROPOSE_DELAY_MS='0' \
        N42_REWARD_EPOCH_BLOCKS='50' \
        N42_DAILY_BASE_REWARD_GWEI='100000000' \
        N42_REWARD_CURVE_K='4.0' \
        N42_MIN_ATTESTATION_THRESHOLD='1' \
        N42_OPEN_VERIFICATION='1' \
        "$NODE_BIN" node \
            --chain "$genesis" \
            --datadir "$datadir" \
            --http \
            --http.addr 127.0.0.1 \
            --http.port "$http_port" \
            --http.api eth,net,web3,txpool,rpc,debug,trace \
            --ws \
            --ws.port "$ws_port" \
            --authrpc.port "$auth_port" \
            --port "$p2p_port" \
            --disable-discovery \
            --log.file.directory "$datadir/logs" \
            --ipcdisable \
            --max-outbound-peers "$count" \
            --max-inbound-peers "$count" \
            --metrics "127.0.0.1:$metrics_port" \
            --builder.gaslimit "${N42_GAS_LIMIT:-2000000000}" \
            --builder.interval 50ms \
            --rpc.max-connections 128 \
            --disable-tx-gossip \
            --p2p-secret-key-hex "$p2p_secret" \
            > "$dir/validator-$idx.log" 2>&1 &

    local pid="$!"
    PIDS+=("$pid")
    echo "$pid" > "$dir/validator-$idx.pid"
    wait_rpc "$http_port"
}

stop_node() {
    local dir="$1"
    local idx="$2"
    if [[ -f "$dir/validator-$idx.pid" ]]; then
        local pid
        pid="$(cat "$dir/validator-$idx.pid")"
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        rm -f "$dir/validator-$idx.pid"
    fi
}

make_evidence_tool() {
    local tool="$ARTIFACT_ROOT/evidence-dump"
    mkdir -p "$tool/src"
    cat > "$tool/Cargo.toml" <<EOF
[package]
name = "evidence-dump"
version = "0.1.0"
edition = "2024"

[workspace]

[dependencies]
n42-jmt = { path = "$PROJECT_DIR/crates/n42-jmt" }
eyre = "0.6"
serde_json = "1"
hex = "0.4"
EOF
    cat > "$tool/src/main.rs" <<'EOF'
use n42_jmt::{EvidenceStore, open_jmt_env};
use serde_json::json;

fn main() -> eyre::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let evidence_dir = args.get(1).expect("evidence dir arg");
    let block_number: u64 = args.get(2).expect("block number arg").parse()?;
    let env = open_jmt_env(evidence_dir)?;
    let store = EvidenceStore::open(env)?;
    let ev = store.get(block_number)?;
    match ev {
        Some(ev) => {
            println!("{}", json!({
                "block_number": block_number,
                "view": ev.view,
                "block_hash": format!("0x{}", hex::encode(ev.block_hash)),
                "signer_count": ev.signer_count,
                "packed_signers": format!("0x{}", hex::encode(ev.packed_signers)),
                "has_mobile": ev.mobile.is_some(),
            }));
        }
        None => println!("{}", json!({"block_number": block_number, "missing": true})),
    }
    Ok(())
}
EOF
}

dump_evidence() {
    local dir="$1"
    local block="${2:-1}"
    local out="$3"
    cargo run --quiet --manifest-path "$ARTIFACT_ROOT/evidence-dump/Cargo.toml" -- \
        "$dir/validator-0/evidence" "$block" > "$out" 2>"$out.stderr"
}

analyze_scenario() {
    local dir="$1"
    local count="$2"
    local leader="$3"
    local label="$4"
    "$PYTHON_BIN" - "$dir" "$count" "$leader" "$label" <<'PY'
from pathlib import Path
import json, re, sys

root = Path(sys.argv[1])
count = int(sys.argv[2])
leader = int(sys.argv[3])
label = sys.argv[4]
needed = 0 if count <= 1 else (2 * ((count - 1) // 3) + 1) - 1
leader_log = root / f"validator-{leader}.log"

events = []
connected = 0
first_build = None
first_commit = None
first_timeout = None
first_newview = None
waiting = []

patterns = {
    "connected": "consensus peer connected",
    "waiting": "waiting for validator quorum",
    "timer": "leader build timer reached",
    "build": "triggering payload build",
    "commit": "block committed!",
    "timeout": "BroadcastMessage(Timeout",
    "newview": "BroadcastMessage(NewView",
}

if leader_log.exists():
    for line_no, line in enumerate(leader_log.read_text(errors="replace").splitlines(), 1):
        if patterns["connected"] in line:
            connected += 1
            events.append({"line": line_no, "kind": "peer_connected", "connected": connected, "text": line[-260:]})
        if patterns["waiting"] in line:
            waiting.append(line_no)
            events.append({"line": line_no, "kind": "waiting", "connected": connected, "needed": needed, "text": line[-260:]})
        if patterns["timer"] in line:
            events.append({"line": line_no, "kind": "gate_recheck", "connected": connected, "needed": needed, "text": line[-260:]})
        if patterns["build"] in line and first_build is None:
            first_build = {"line": line_no, "connected": connected, "needed": needed, "text": line[-260:]}
            events.append({"line": line_no, "kind": "first_build", **first_build})
        if patterns["commit"] in line and first_commit is None:
            first_commit = {"line": line_no, "connected": connected, "needed": needed, "text": line[-260:]}
        if patterns["timeout"] in line and first_timeout is None:
            first_timeout = {"line": line_no, "connected": connected, "needed": needed, "text": line[-260:]}
        if patterns["newview"] in line and first_newview is None:
            first_newview = {"line": line_no, "connected": connected, "needed": needed, "text": line[-260:]}

summary = {
    "label": label,
    "validator_count": count,
    "view1_leader": leader,
    "needed_validator_peers": needed,
    "leader_log": str(leader_log),
    "waiting_lines": waiting,
    "first_build": first_build,
    "first_commit": first_commit,
    "first_timeout_broadcast_on_leader": first_timeout,
    "first_newview_broadcast_on_leader": first_newview,
    "events": events[:80],
}

(root / "analysis.json").write_text(json.dumps(summary, indent=2))
md = [
    f"# {label}",
    "",
    f"- validators: {count}",
    f"- view-1 leader: validator-{leader}",
    f"- needed connected validator peers: {needed}",
    f"- waiting log lines: {waiting}",
    f"- first build: {first_build}",
    f"- first leader commit: {first_commit}",
    f"- first timeout broadcast on leader: {first_timeout}",
    f"- first newview broadcast on leader: {first_newview}",
]
(root / "analysis.md").write_text("\n".join(md) + "\n")
PY
}

scenario_prepare() {
    local name="$1"
    local count="$2"
    local shift="$3"
    local dir="$ARTIFACT_ROOT/$name"
    mkdir -p "$dir"
    generate_genesis "$dir/genesis.json"
    generate_p2p_keys "$count" "$dir/p2p-keys.json"
    echo "$dir"
}

run_all_together() {
    local name="$1"
    local count="$2"
    local shift="$3"
    local dir
    dir="$(scenario_prepare "$name" "$count" "$shift")"
    log "Scenario $name: starting $count validators together"
    local start_ts="$SECONDS"
    for i in $(seq 0 $((count - 1))); do
        start_node "$dir" "$count" "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    local h
    h="$(wait_height_gt $((BASE_HTTP + shift)) 0 90 || true)"
    local ttf=$((SECONDS - start_ts))
    echo "{\"height\":$h,\"time_to_first_block_secs\":$ttf}" > "$dir/result.json"
    dump_evidence "$dir" 1 "$dir/evidence-block-1.json"
    local leader=1
    if (( count == 1 )); then
        leader=0
    fi
    analyze_scenario "$dir" "$count" "$leader" "$name"
    log "Scenario $name complete: height=$h ttf=${ttf}s"
    cleanup_all
    PIDS=()
}

run_staggered() {
    local name="$1"
    local shift="$2"
    local dir
    dir="$(scenario_prepare "$name" 7 "$shift")"
    log "Scenario $name: starting validators 0,1,2, waiting ${STAGGER_WAIT_SECS}s"
    local start_ts="$SECONDS"
    for i in 0 1 2; do
        start_node "$dir" 7 "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    sleep "$STAGGER_WAIT_SECS"
    local early_h
    early_h="$(block_number $((BASE_HTTP + shift)))"
    info "$name early height with 3 validators: $early_h"
    local quorum_ts="$SECONDS"
    for i in 3 4 5 6; do
        start_node "$dir" 7 "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    local h
    h="$(wait_height_gt $((BASE_HTTP + shift)) 0 90 || true)"
    local ttf=$((SECONDS - start_ts))
    local after_quorum=$((SECONDS - quorum_ts))
    printf '{"early_height":%s,"height":%s,"time_to_first_block_secs":%s,"time_after_quorum_secs":%s}\n' \
        "$early_h" "$h" "$ttf" "$after_quorum" > "$dir/result.json"
    dump_evidence "$dir" 1 "$dir/evidence-block-1.json"
    analyze_scenario "$dir" 7 1 "$name"
    log "Scenario $name complete: early_height=$early_h height=$h ttf=${ttf}s after_quorum=${after_quorum}s"
    cleanup_all
    PIDS=()
}

run_leader_first() {
    local name="$1"
    local shift="$2"
    local dir
    dir="$(scenario_prepare "$name" 7 "$shift")"
    log "Scenario $name: starting view-1 leader validator 1 alone"
    local start_ts="$SECONDS"
    start_node "$dir" 7 1 "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    sleep "$STAGGER_WAIT_SECS"
    local early_h
    early_h="$(block_number $((BASE_HTTP + shift + 1)))"
    info "$name early height with leader alone: $early_h"
    local quorum_ts="$SECONDS"
    for i in 0 2 3 4 5 6; do
        start_node "$dir" 7 "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    local h
    h="$(wait_height_gt $((BASE_HTTP + shift)) 0 90 || true)"
    local ttf=$((SECONDS - start_ts))
    local after_quorum=$((SECONDS - quorum_ts))
    printf '{"early_height":%s,"height":%s,"time_to_first_block_secs":%s,"time_after_quorum_secs":%s}\n' \
        "$early_h" "$h" "$ttf" "$after_quorum" > "$dir/result.json"
    dump_evidence "$dir" 1 "$dir/evidence-block-1.json"
    analyze_scenario "$dir" 7 1 "$name"
    log "Scenario $name complete: early_height=$early_h height=$h ttf=${ttf}s after_quorum=${after_quorum}s"
    cleanup_all
    PIDS=()
}

run_partition() {
    local name="$1"
    local shift="$2"
    local dir
    dir="$(scenario_prepare "$name" 7 "$shift")"
    log "Scenario $name: starting 7 validators"
    for i in 0 1 2 3 4 5 6; do
        start_node "$dir" 7 "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    local h1
    h1="$(wait_height_gt $((BASE_HTTP + shift)) 0 120 || true)"
    log "$name reached height $h1; stopping validators 4,5,6"
    stop_node "$dir" 4
    stop_node "$dir" 5
    stop_node "$dir" 6
    local before
    before="$(block_number $((BASE_HTTP + shift)))"
    sleep "$PARTITION_WAIT_SECS"
    local during
    during="$(block_number $((BASE_HTTP + shift)))"
    log "$name partition window height before=$before during=$during"
    for i in 4 5 6; do
        start_node "$dir" 7 "$i" "$dir/p2p-keys.json" "$dir/genesis.json" "$shift"
    done
    local restored
    restored="$(wait_height_gt $((BASE_HTTP + shift)) "$during" 120 || true)"
    printf '{"height_before_partition":%s,"height_during_partition":%s,"height_after_restore":%s}\n' \
        "$before" "$during" "$restored" > "$dir/result.json"
    dump_evidence "$dir" 1 "$dir/evidence-block-1.json"
    analyze_scenario "$dir" 7 1 "$name"
    log "Scenario $name complete: before=$before during=$during restored=$restored"
    cleanup_all
    PIDS=()
}

make_evidence_tool

run_all_together "baseline-all-7" 7 0
run_staggered "staggered-0-1-2-then-rest" 100
run_leader_first "leader-1-alone-then-rest" 200
run_partition "mid-run-partition-kill-4-5-6" 300
run_all_together "single-node" 1 400

log "Artifacts written to $ARTIFACT_ROOT"
