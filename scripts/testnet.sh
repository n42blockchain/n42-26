#!/usr/bin/env bash
#
# N42 Unified Testnet — All-in-One Launcher
#
# Starts a multi-validator HotStuff-2 testnet on a single machine with:
#   - Release-optimized n42-node binaries
#   - Custom genesis (10 pre-funded test accounts, chain ID 4242)
#   - Blockscout block explorer (optional)
#   - Continuous transaction load generator (optional)
#   - Mobile phone simulator (optional)
#   - Error monitor daemon (optional)
#
# Usage:
#   ./scripts/testnet.sh                              # 7-node full stack
#   ./scripts/testnet.sh --nodes 1 --no-explorer      # single node, no Blockscout
#   ./scripts/testnet.sh --nodes 21 --clean            # 21 nodes, wipe data first
#   ./scripts/testnet.sh --nodes 3 --debug             # 3 nodes, debug build
#
# Ctrl+C to gracefully stop everything.
#

set -euo pipefail

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Port allocation: each range has 400-slot gap to support up to 400 nodes.
# Range layout (for N nodes):
#   HTTP:      18000 .. 18000+N-1
#   WS:        18400 .. 18400+N-1
#   AUTH:      18800 .. 18800+N-1
#   METRICS:   19200 .. 19200+N-1
#   P2P:       30303 .. 30303+N-1
#   CONSENSUS:  9000 ..  9000+N-1
#   STARHUB:    9400 ..  9400+N-1
BASE_HTTP_RPC=18000
BASE_WS=18400
BASE_AUTH=18800
BASE_METRICS=19200
BASE_P2P=30303
BASE_CONSENSUS=9000
BASE_STARHUB=9400

CHAIN_ID="${N42_CHAIN_ID:-4242}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

parse_args() {
    NUM_VALIDATORS=7
    CLEAN=false
    DEBUG_BUILD=false
    NO_EXPLORER=false
    NO_TX_GEN=false
    NO_MONITOR=false
    NO_MOBILE_SIM=false
    DATA_DIR=""
    BLOCK_INTERVAL_MS="${N42_TARGET_CADENCE_MS:-163}"
    PUBLIC_IP="localhost"

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --nodes)
                NUM_VALIDATORS="$2"
                shift 2
                ;;
            --clean)
                CLEAN=true
                shift
                ;;
            --debug)
                DEBUG_BUILD=true
                shift
                ;;
            --no-explorer)
                NO_EXPLORER=true
                shift
                ;;
            --no-tx-gen)
                NO_TX_GEN=true
                shift
                ;;
            --no-monitor)
                NO_MONITOR=true
                shift
                ;;
            --no-mobile-sim)
                NO_MOBILE_SIM=true
                shift
                ;;
            --mobile-sim)
                NO_MOBILE_SIM=false
                shift
                ;;
            --data-dir)
                DATA_DIR="$2"
                shift 2
                ;;
            --public-ip)
                PUBLIC_IP="$2"
                shift 2
                ;;
            --block-interval)
                BLOCK_INTERVAL_MS="$2"
                shift 2
                ;;
            -h|--help)
                echo "Usage: $0 [OPTIONS]"
                echo ""
                echo "Options:"
                echo "  --nodes N            Validator count (1/3/5/7/21, default: 7)"
                echo "  --clean              Wipe all testnet data before starting"
                echo "  --debug              Use debug binary (faster compilation)"
                echo "  --no-explorer        Don't start Blockscout"
                echo "  --no-tx-gen          Don't start transaction generator"
                echo "  --no-monitor         Don't start error monitor"
                echo "  --no-mobile-sim      Don't start mobile phone simulator"
                echo "  --mobile-sim         Start mobile phone simulator (default)"
                echo "  --data-dir DIR       Custom data directory (default: ~/n42-testnet-data-N)"
                echo "  --block-interval MS  Block interval in ms (default: 163)"
                exit 0
                ;;
            *)
                echo -e "${RED}Unknown option: $1${NC}"
                echo "Use --help for usage information"
                exit 1
                ;;
        esac
    done

    # Validate node count
    if [[ "$NUM_VALIDATORS" -lt 1 || "$NUM_VALIDATORS" -gt 400 ]]; then
        echo -e "${RED}Invalid node count: $NUM_VALIDATORS (must be 1-400)${NC}"
        exit 1
    fi

    # Default data directory based on node count
    if [[ -z "$DATA_DIR" ]]; then
        if [[ "$NUM_VALIDATORS" -eq 7 ]]; then
            DATA_DIR="$HOME/n42-testnet-data"
        else
            DATA_DIR="$HOME/n42-testnet-data-${NUM_VALIDATORS}"
        fi
    fi

    # Single node: default to no mobile sim
    if [[ "$NUM_VALIDATORS" -eq 1 ]]; then
        # Single node doesn't need mobile sim by default
        # (can be overridden with --mobile-sim before --nodes 1 in args)
        : # keep whatever was set
    fi
}

# ---------------------------------------------------------------------------
# Optional CCD/L3-aware affinity A/B
# ---------------------------------------------------------------------------

declare -a N42_NODE_CPUSETS=()

prepare_l3_cpusets() {
    local mode="${N42_CPUSET_AB_MODE:-off}"
    N42_NODE_CPUSETS=()
    if [[ "$mode" == "off" || "$mode" == "0" ]]; then
        log "CPU affinity A/B: control (scheduler-managed; no cpuset binding)"
        return
    fi
    if [[ "$mode" != "l3" ]]; then
        warn "Unknown N42_CPUSET_AB_MODE=$mode; leaving affinity scheduler-managed"
        return
    fi
    if ! command -v taskset >/dev/null 2>&1 || [[ ! -d /sys/devices/system/cpu ]]; then
        warn "L3 cpuset A/B requested but taskset/sysfs topology is unavailable; not binding"
        return
    fi

    local allowed_list
    allowed_list=$(awk '/^Cpus_allowed_list:/ {print $2}' /proc/self/status)
    if [[ -z "$allowed_list" ]]; then
        warn "Could not read current allowed CPU set; not binding"
        return
    fi
    declare -A allowed_cpu=()
    local range first last cpu
    IFS=',' read -ra allowed_ranges <<< "$allowed_list"
    for range in "${allowed_ranges[@]}"; do
        if [[ "$range" == *-* ]]; then
            first=${range%-*}
            last=${range#*-}
        else
            first=$range
            last=$range
        fi
        for ((cpu=first; cpu<=last; cpu++)); do
            allowed_cpu[$cpu]=1
        done
    done

    declare -A l3_cpus=()
    local id_file l3_id
    for id_file in /sys/devices/system/cpu/cpu[0-9]*/cache/index3/id; do
        [[ -r "$id_file" ]] || continue
        cpu=${id_file#/sys/devices/system/cpu/cpu}
        cpu=${cpu%%/*}
        [[ -n "${allowed_cpu[$cpu]:-}" ]] || continue
        [[ ! -r "/sys/devices/system/cpu/cpu${cpu}/online" ]] \
            || [[ "$(<"/sys/devices/system/cpu/cpu${cpu}/online")" == "1" ]] \
            || continue
        l3_id=$(<"$id_file")
        if [[ -n "${l3_cpus[$l3_id]:-}" ]]; then
            l3_cpus[$l3_id]+=",$cpu"
        else
            l3_cpus[$l3_id]="$cpu"
        fi
    done

    local -a l3_ids=()
    mapfile -t l3_ids < <(printf '%s\n' "${!l3_cpus[@]}" | sort -n)
    local domains_per_node="${N42_CCDS_PER_NODE:-2}"
    if ! [[ "$domains_per_node" =~ ^[1-9][0-9]*$ ]]; then
        warn "Invalid N42_CCDS_PER_NODE=$domains_per_node; not binding"
        return
    fi
    local needed=$((NUM_VALIDATORS * domains_per_node))
    if (( ${#l3_ids[@]} < needed )); then
        warn "L3 cpuset A/B needs ${needed} disjoint L3 domains, found ${#l3_ids[@]}; not binding"
        return
    fi

    local node domain_index domain cpus
    : > "$DATA_DIR/cpu-affinity.tsv"
    printf 'variant\tnode\tl3_domains\tcpus\n' >> "$DATA_DIR/cpu-affinity.tsv"
    for ((node=0; node<NUM_VALIDATORS; node++)); do
        cpus=""
        local domain_labels=""
        for ((domain_index=0; domain_index<domains_per_node; domain_index++)); do
            domain=${l3_ids[$((node * domains_per_node + domain_index))]}
            cpus+="${cpus:+,}${l3_cpus[$domain]}"
            domain_labels+="${domain_labels:+,}$domain"
        done
        N42_NODE_CPUSETS[$node]="$cpus"
        printf 'l3\t%s\t%s\t%s\n' "$node" "$domain_labels" "$cpus" \
            >> "$DATA_DIR/cpu-affinity.tsv"
    done
    log "CPU affinity A/B: treatment=l3, ${domains_per_node} disjoint L3 domains/node; mapping=$DATA_DIR/cpu-affinity.tsv"
}

# ---------------------------------------------------------------------------
# Timeout scaling — adapts to cluster size
# ---------------------------------------------------------------------------

compute_timeouts() {
    case "$NUM_VALIDATORS" in
        1)
            BASE_TIMEOUT_MS=10000
            MAX_TIMEOUT_MS=30000
            STARTUP_DELAY_MS=2000
            NODE_START_INTERVAL=0
            MAX_BLOCK_WAIT=60
            ;;
        3)
            BASE_TIMEOUT_MS=15000
            MAX_TIMEOUT_MS=45000
            STARTUP_DELAY_MS=2000
            NODE_START_INTERVAL=3
            MAX_BLOCK_WAIT=78
            ;;
        5)
            BASE_TIMEOUT_MS=15000
            MAX_TIMEOUT_MS=45000
            STARTUP_DELAY_MS=2000
            NODE_START_INTERVAL=2
            MAX_BLOCK_WAIT=90
            ;;
        7)
            BASE_TIMEOUT_MS=10000
            MAX_TIMEOUT_MS=30000
            STARTUP_DELAY_MS=3000
            NODE_START_INTERVAL=2
            MAX_BLOCK_WAIT=120
            ;;
        21)
            BASE_TIMEOUT_MS=30000
            MAX_TIMEOUT_MS=90000
            STARTUP_DELAY_MS=5200
            NODE_START_INTERVAL=1
            MAX_BLOCK_WAIT=180
            ;;
        *)
            # Generic scaling for large clusters
            # STARTUP_DELAY must exceed total node launch time (N * interval)
            # plus ~60s for gossipsub mesh formation.
            # CRITICAL: BASE_TIMEOUT must be >= STARTUP_DELAY so that non-leader
            # nodes do not timeout before the leader's first proposal arrives.
            if [[ "$NUM_VALIDATORS" -ge 200 ]]; then
                NODE_START_INTERVAL=2
            else
                NODE_START_INTERVAL=1
            fi
            STARTUP_DELAY_MS=$(( (NUM_VALIDATORS * NODE_START_INTERVAL * 1000) + 60000 ))
            BASE_TIMEOUT_MS=$(( STARTUP_DELAY_MS + 30000 ))
            MAX_TIMEOUT_MS=$((BASE_TIMEOUT_MS * 3))
            MAX_BLOCK_WAIT=$(( (NUM_VALIDATORS * NODE_START_INTERVAL * 3) + 180 ))
            ;;
    esac

    # Auto-enable low-memory mode for large local testnets unless explicitly set
    if [[ -z "${N42_LOW_MEMORY:-}" ]] && [[ "$NUM_VALIDATORS" -gt 20 ]]; then
        local total_ram_mb=$(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo 0)
        local needed_mb=$((NUM_VALIDATORS * 1200))  # ~1.2GB per node at full config
        if [[ "$needed_mb" -gt "$total_ram_mb" ]] || [[ "$NUM_VALIDATORS" -gt 50 ]]; then
            export N42_LOW_MEMORY=1
            echo -e "${YELLOW}[auto] N42_LOW_MEMORY=1 enabled (${NUM_VALIDATORS} nodes, ${total_ram_mb}MB RAM)${NC}"
        fi
    fi
}

# ---------------------------------------------------------------------------
# Logging helpers
# ---------------------------------------------------------------------------

LOG_PREFIX="testnet"
log()  { echo -e "${GREEN}[${LOG_PREFIX}]${NC} $*"; }
warn() { echo -e "${YELLOW}[${LOG_PREFIX}]${NC} $*"; }
err()  { echo -e "${RED}[${LOG_PREFIX}]${NC} $*"; }
info() { echo -e "${CYAN}[${LOG_PREFIX}]${NC} $*"; }

# ---------------------------------------------------------------------------
# File descriptor limit
# ---------------------------------------------------------------------------

setup_ulimit() {
    local current_ulimit
    current_ulimit=$(ulimit -n 2>/dev/null || echo "256")
    if [[ "$current_ulimit" != "unlimited" ]] && [[ "$current_ulimit" -lt 65536 ]]; then
        ulimit -n 65536 2>/dev/null || \
            warn "Could not raise ulimit to 65536 (current: $current_ulimit). May cause issues."
    fi
}

# ---------------------------------------------------------------------------
# Cleanup handler
# ---------------------------------------------------------------------------

NODE_PIDS=()
TX_GEN_PID=""
MONITOR_PID=""
MOBILE_SIM_PID=""
BLOCKSCOUT_STARTED=false

cleanup() {
    echo ""
    log "Shutting down testnet..."

    for pid_var in TX_GEN_PID MONITOR_PID MOBILE_SIM_PID; do
        pid="${!pid_var}"
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
    done

    if [[ "$BLOCKSCOUT_STARTED" == true ]]; then
        log "Stopping Blockscout..."
        docker compose -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" down 2>/dev/null || true
    fi

    if [[ ${#NODE_PIDS[@]} -gt 0 ]]; then
        log "Stopping ${#NODE_PIDS[@]} validator nodes..."
        for pid in "${NODE_PIDS[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
            fi
        done
    fi

    wait 2>/dev/null || true
    log "All services stopped."
}

trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Python virtual environment
# ---------------------------------------------------------------------------

setup_python_venv() {
    VENV_DIR="$DATA_DIR/.venv"
    if [[ ! -d "$VENV_DIR" ]]; then
        log "Creating Python virtual environment at $VENV_DIR..."
        python3 -m venv "$VENV_DIR"
    fi
    if [[ -f "$VENV_DIR/Scripts/activate" ]]; then
        source "$VENV_DIR/Scripts/activate"
    else
        source "$VENV_DIR/bin/activate"
    fi

    if ! python3 -c "import eth_account" 2>/dev/null; then
        log "Installing Python dependencies..."
        pip3 install --quiet -r "$SCRIPT_DIR/requirements-testnet.txt"
    fi
    log "Python venv ready: $(python3 --version)"
}

# ---------------------------------------------------------------------------
# Build binaries
# ---------------------------------------------------------------------------

build_binaries() {
    local build_flag="--release"
    local build_dir="release"
    if [[ -n "${N42_BUILD_PROFILE:-}" ]]; then
        build_dir="$N42_BUILD_PROFILE"
        case "$N42_BUILD_PROFILE" in
            release)
                build_flag="--release"
                ;;
            debug)
                build_flag=""
                ;;
            *)
                build_flag="--profile $N42_BUILD_PROFILE"
                ;;
        esac
    elif [[ "$DEBUG_BUILD" == true ]]; then
        build_flag=""
        build_dir="debug"
    fi

    BINARY="$PROJECT_DIR/target/${build_dir}/n42-node"
    # Skip build if binary already exists (e.g., pre-built with --no-default-features on Windows)
    if [[ -f "$BINARY" ]] || [[ -f "${BINARY}.exe" ]]; then
        [[ -f "${BINARY}.exe" ]] && [[ ! -f "$BINARY" ]] && BINARY="${BINARY}.exe"
        log "Binary already exists, skipping build: $BINARY"
    else
        log "Building n42-node and n42-mobile-sim (${build_dir}) - this may take a few minutes..."
        cd "$PROJECT_DIR"
        cargo build $build_flag --bin n42-node --bin n42-mobile-sim 2>&1 | tail -5
    fi

    if [[ ! -f "$BINARY" ]]; then
        err "Failed to build n42-node binary"
        exit 1
    fi
    log "Binary ready: $BINARY"

    MOBILE_SIM_BIN="$PROJECT_DIR/target/${build_dir}/n42-mobile-sim"
    if [[ -f "$MOBILE_SIM_BIN" ]]; then
        log "Mobile sim binary ready: $MOBILE_SIM_BIN"
    else
        warn "n42-mobile-sim binary not found (non-fatal)"
    fi
}

# ---------------------------------------------------------------------------
# Generate genesis.json
# ---------------------------------------------------------------------------

generate_genesis() {
    GENESIS_FILE="$DATA_DIR/genesis.json"
    ACCOUNTS_CACHE="$SCRIPT_DIR/testnet-accounts-cache.json"

    if [[ ! -f "$GENESIS_FILE" ]] || [[ "$CLEAN" == true ]]; then
        log "Generating genesis.json with pre-funded test accounts..."

        DATA_DIR="$DATA_DIR" ACCOUNTS_CACHE="$ACCOUNTS_CACHE" python3 << 'PYEOF'
import json, os, time

NUM_ACCOUNTS = 5000
CHAIN_ID = int(os.environ.get("N42_CHAIN_ID", "4242"))
INITIAL_BALANCE = "0x4B3B4CA85A86C47A098A224000000"  # 100M N42 per account
STRESS_CONTRACT_ADDRESS = "0x000000000000000000000000000000000000C042"
STRESS_CONTRACT_BYTECODE = "0x33600052600060205260406000208054600101905560016020526040600020429055600160005260206000f3"

data_dir = os.environ["DATA_DIR"]
cache_file = os.environ.get("ACCOUNTS_CACHE", "")

# Try loading from cache first (instant, no eth_account dependency)
if cache_file and os.path.exists(cache_file):
    t0 = time.time()
    with open(cache_file) as f:
        accounts = json.load(f)
    print(f"  Loaded {len(accounts)} accounts from cache ({time.time()-t0:.2f}s)")
else:
    # Generate from scratch (slow: ~30s for 5000 accounts)
    from eth_account import Account
    from eth_utils import keccak
    t0 = time.time()
    accounts = []
    for i in range(NUM_ACCOUNTS):
        private_key = keccak(f"n42-test-key-{i}".encode())
        acct = Account.from_key(private_key)
        accounts.append({"key": private_key.hex(), "address": acct.address})
        if i % 500 == 0:
            print(f"  Account {i}/{NUM_ACCOUNTS}...")
    print(f"  Generated {len(accounts)} accounts ({time.time()-t0:.2f}s)")
    # Save cache for future use
    if cache_file:
        with open(cache_file, "w") as f:
            json.dump(accounts, f, indent=2)
        print(f"  Saved accounts cache to {cache_file}")

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
    "gasLimit": hex(int(os.environ.get("N42_GAS_LIMIT", "2000000000"))), "difficulty": "0x0",
    "mixHash": "0x" + "0" * 64,
    "coinbase": "0x0000000000000000000000000000000000000000",
    "alloc": {
        # Genesis master account: 30 billion N42
        "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc": {"balance": "0x60EF6B1ABA6F072330000000"},
        # Predeployed storage burner used by n42-stress --erc20-ratio.
        STRESS_CONTRACT_ADDRESS: {"balance": "0x0", "code": STRESS_CONTRACT_BYTECODE},
        # Test accounts: 100M N42 each
        **{a["address"]: {"balance": INITIAL_BALANCE} for a in accounts},
    },
    "number": "0x0", "gasUsed": "0x0",
    "parentHash": "0x" + "0" * 64,
    "baseFeePerGas": "0x3B9ACA00",
    "excessBlobGas": "0x0", "blobGasUsed": "0x0",
}

with open(os.path.join(data_dir, "genesis.json"), "w") as f:
    json.dump(genesis, f, indent=2)
with open(os.path.join(data_dir, "test-accounts.json"), "w") as f:
    json.dump(accounts, f, indent=2)

print("  Genesis written.")
PYEOF

        if [[ $? -ne 0 ]]; then
            err "Genesis generation failed. Are eth-account and eth-utils installed?"
            err "Run: pip3 install -r scripts/requirements-testnet.txt"
            exit 1
        fi
    else
        log "Using existing genesis at $GENESIS_FILE"
    fi
}

# ---------------------------------------------------------------------------
# Generate BLS validator keys (deterministic)
# ---------------------------------------------------------------------------

generate_bls_keys() {
    # Format: 32 bytes, all zeros except last 4 bytes = (index+1) big-endian
    # Matches ConsensusConfig::deterministic_key_bytes()
    log "Generating BLS keys for $NUM_VALIDATORS validators..."
    KEYS=()
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        local val=$((i + 1))
        KEYS+=("$(printf '%056x%08x' 0 "$val")")
    done
    info "  Keys: v0=${KEYS[0]:0:12}... v$((NUM_VALIDATORS-1))=${KEYS[$((NUM_VALIDATORS-1))]:0:12}..."
}

# ---------------------------------------------------------------------------
# Generate deterministic devp2p keys
# ---------------------------------------------------------------------------

generate_p2p_keys() {
    if [[ "$NUM_VALIDATORS" -eq 1 ]]; then
        log "Single node mode — skipping P2P key generation"
        P2P_SECRETS=("")
        P2P_PUBKEYS=("")
        return
    fi

    log "Generating devp2p keys for $NUM_VALIDATORS validators..."
    P2P_SECRETS=()
    P2P_PUBKEYS=()

    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        read -r secret pubkey <<< "$(python3 -c "
from eth_utils import keccak
from eth_keys import keys
sk = keccak(b'n42-devp2p-key-$i')
pk = keys.PrivateKey(sk)
print(sk.hex(), pk.public_key.to_hex()[2:])
")"
        P2P_SECRETS+=("$secret")
        P2P_PUBKEYS+=("$pubkey")
    done
    log "  devp2p keys generated for $NUM_VALIDATORS nodes"
}

# ---------------------------------------------------------------------------
# Start validator nodes
# ---------------------------------------------------------------------------

start_validators() {
    if [[ "${N42_LOW_MEMORY:-0}" == "1" ]]; then
        log "Starting $NUM_VALIDATORS validator node(s) [LOW_MEMORY mode: ~200MB/node]..."
    else
        log "Starting $NUM_VALIDATORS validator node(s) [standard mode: ~5GB/node]..."
    fi
    ENODES=()

    # A dense testnet shares one machine.  Without explicit budgets every reth
    # process independently sizes all of its runtimes for the whole host (for
    # example, seven validators each created 256-thread pools on a 256-thread
    # host).  Keep an override for isolated-node benchmarks, but partition the
    # host by default and cap the per-node budget to avoid scheduler thrashing.
    local host_threads
    host_threads="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc)"
    local automatic_node_budget=$((host_threads / NUM_VALIDATORS))
    (( automatic_node_budget < 4 )) && automatic_node_budget=4
    (( automatic_node_budget > 32 )) && automatic_node_budget=32
    local runtime_threads="${N42_TESTNET_THREADS_PER_NODE:-$automatic_node_budget}"
    local tokio_threads="${N42_TESTNET_TOKIO_THREADS:-$runtime_threads}"
    if [[ -z "${N42_TESTNET_TOKIO_THREADS:-}" ]] && (( tokio_threads > 16 )); then
        tokio_threads=16
    fi
    local rpc_threads="${N42_TESTNET_RPC_THREADS:-4}"
    local storage_threads="${N42_TESTNET_STORAGE_THREADS:-4}"
    local global_rayon_threads="${N42_TESTNET_GLOBAL_RAYON_THREADS:-$runtime_threads}"
    local blocking_threads="${N42_TESTNET_MAX_BLOCKING_THREADS:-$((runtime_threads * 2))}"
    if [[ -z "${N42_TESTNET_MAX_BLOCKING_THREADS:-}" ]]; then
        (( blocking_threads < 16 )) && blocking_threads=16
        (( blocking_threads > 64 )) && blocking_threads=64
    fi
    local eth_blocking_threads="${N42_TESTNET_ETH_BLOCKING_THREADS:-$tokio_threads}"
    local prewarming_threads="${N42_TESTNET_PREWARMING_THREADS:-$tokio_threads}"
    local disable_mobile_packets="${N42_DISABLE_MOBILE_PACKETS:-}"
    if [[ -z "$disable_mobile_packets" ]]; then
        if [[ "${N42_BLOCK_DIRECT_ONLY:-0}" == "1" ]]; then
            disable_mobile_packets=1
        else
            disable_mobile_packets=0
        fi
    fi

    if [[ "${N42_LOW_MEMORY:-0}" == "1" ]]; then
        runtime_threads=2
        tokio_threads=2
        rpc_threads=2
        storage_threads=2
        global_rayon_threads=2
        blocking_threads=4
        eth_blocking_threads=2
        prewarming_threads=1
    fi
    log "Runtime budget: host=${host_threads} threads, per-node cpu=${runtime_threads} tokio=${tokio_threads} rpc=${rpc_threads} storage=${storage_threads} global-rayon=${global_rayon_threads} blocking<=${blocking_threads} prewarm=${prewarming_threads}"

    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        datadir="$DATA_DIR/validator-${i}"
        http_port=$((BASE_HTTP_RPC + i))
        ws_port=$((BASE_WS + i))
        auth_port=$((BASE_AUTH + i))
        p2p_port=$((BASE_P2P + i))
        discovery_v5_port=$((BASE_P2P + 200 + i))
        consensus_port=$((BASE_CONSENSUS + i))
        metrics_port=$((BASE_METRICS + i))
        starhub_port=$((BASE_STARHUB + i))
        log_file="$DATA_DIR/validator-${i}.log"

        mkdir -p "$datadir"

        # Build trusted-peers flag from previously started enodes
        TRUSTED_PEERS_FLAG=""
        if [[ ${#ENODES[@]} -gt 0 ]] && [[ "$NUM_VALIDATORS" -gt 1 ]]; then
            TRUSTED_PEERS_FLAG="--trusted-peers $(IFS=,; echo "${ENODES[*]}")"
        fi

        log "  v${i}: http=:${http_port} p2p=:${p2p_port} consensus=:${consensus_port}"

        local p2p_key_flag=""
        if [[ "$NUM_VALIDATORS" -gt 1 ]]; then
            p2p_key_flag="--p2p-secret-key-hex ${P2P_SECRETS[$i]}"
        fi

        local mdns_flag="${N42_ENABLE_MDNS:-true}"
        if [[ "$NUM_VALIDATORS" -eq 1 ]]; then
            mdns_flag="${N42_ENABLE_MDNS:-false}"
        fi

        local -a node_launcher=("$BINARY")
        local profile_output="${N42_PROFILE_OUTPUT:-$DATA_DIR/samply-validator-${i}.json}"
        if [[ "${N42_PROFILE_VALIDATOR_INDEX:-}" == "$i" ]]; then
            mkdir -p "$(dirname "$profile_output")"
            log "  v${i}: profiling with samply -> $profile_output"
            node_launcher=(samply record --save-only -o "$profile_output" -- "$BINARY")
        fi
        if [[ -n "${N42_NODE_CPUSETS[$i]:-}" ]]; then
            log "  v${i}: L3 treatment cpus=${N42_NODE_CPUSETS[$i]}"
            node_launcher=(taskset --cpu-list "${N42_NODE_CPUSETS[$i]}" "${node_launcher[@]}")
        fi

        # Low-memory mode: reduce reth caches, pool sizes, and thread counts
        # for dense local testnets. Activated when N42_LOW_MEMORY=1.
        local low_mem_flags=""
        if [[ "${N42_LOW_MEMORY:-0}" == "1" ]]; then
            low_mem_flags="--engine.cross-block-cache-size 64 --engine.disable-state-cache --engine.prewarming-threads 1 --engine.storage-worker-count 2 --engine.account-worker-count 2 --rpc.max-connections 50 --txpool.additional-validation-tasks 2"
        else
            low_mem_flags="--engine.prewarming-threads ${prewarming_threads} --engine.storage-worker-count ${storage_threads} --engine.account-worker-count ${storage_threads} --rpc.max-connections 1000 --txpool.additional-validation-tasks 16"
        fi

        RUST_LOG="${RUST_LOG:-info,libp2p_mdns=off,libp2p_gossipsub::behaviour=error}" \
        RETH_TOKIO_WORKER_THREADS="$tokio_threads" \
        RETH_TOKIO_MAX_BLOCKING_THREADS="$blocking_threads" \
        RETH_RAYON_CPU_THREADS="$runtime_threads" \
        RETH_RAYON_RPC_THREADS="$rpc_threads" \
        RETH_RAYON_STORAGE_THREADS="$storage_threads" \
        RETH_GLOBAL_RAYON_THREADS="$global_rayon_threads" \
        RAYON_NUM_THREADS="$eth_blocking_threads" \
        TOKIO_WORKER_THREADS="$tokio_threads" \
        N42_WORKER_THREADS="$runtime_threads" \
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
        N42_EXECUTION_LANES="${N42_EXECUTION_LANES:-8}" \
        N42_SENDER_SHARDED_DRAIN="${N42_SENDER_SHARDED_DRAIN:-1}" \
        N42_PARALLEL_WORKERS="${N42_PARALLEL_WORKERS:-8}" \
        N42_ASYNC_FINALIZE_FCU="${N42_ASYNC_FINALIZE_FCU:-1}" \
        N42_PAYLOAD_ZSTD="${N42_PAYLOAD_ZSTD:-1}" \
        N42_ZSTD_LEVEL="${N42_ZSTD_LEVEL:-3}" \
        N42_BLOCK_DIRECT_ONLY="${N42_BLOCK_DIRECT_ONLY:-0}" \
        N42_BLOCK_DIRECT_FANOUT="${N42_BLOCK_DIRECT_FANOUT:-$((NUM_VALIDATORS - 1))}" \
        N42_BLOCK_DIRECT_CHUNK_MIB="${N42_BLOCK_DIRECT_CHUNK_MIB:-4}" \
        N42_QUIC_MAX_STREAM_DATA="${N42_QUIC_MAX_STREAM_DATA:-41943040}" \
        N42_QUIC_MAX_CONNECTION_DATA="${N42_QUIC_MAX_CONNECTION_DATA:-100663296}" \
        N42_QUIC_MAX_CONCURRENT_STREAMS="${N42_QUIC_MAX_CONCURRENT_STREAMS:-256}" \
        N42_QUIC_UDP_SOCKET_BUFFER_BYTES="${N42_QUIC_UDP_SOCKET_BUFFER_BYTES:-16777216}" \
        N42_BLOCK_DIRECT_REQUEST_TIMEOUT_MS="${N42_BLOCK_DIRECT_REQUEST_TIMEOUT_MS:-10000}" \
        N42_DISABLE_MOBILE_PACKETS="$disable_mobile_packets" \
        N42_FAST_PROPOSE="${N42_FAST_PROPOSE:-0}" \
        N42_MIN_PROPOSE_DELAY_MS="${N42_MIN_PROPOSE_DELAY_MS:-0}" \
        N42_SKIP_TX_VERIFY="${N42_SKIP_TX_VERIFY:-0}" \
        N42_POOL_VALIDATION_THREADS="${N42_POOL_VALIDATION_THREADS:-2}" \
        N42_LOW_MEMORY="${N42_LOW_MEMORY:-0}" \
        N42_ADMIN_TOKEN="${N42_ADMIN_TOKEN:-}" \
        N42_INJECT_PORT="${N42_INJECT_PORT:+$((${N42_INJECT_PORT} + i))}" \
        "${node_launcher[@]}" node \
            --chain "$GENESIS_FILE" \
            --datadir "$datadir" \
            --http \
            --http.addr "0.0.0.0" \
            --http.port "$http_port" \
            --http.api eth,net,web3,txpool,rpc \
            --http.corsdomain "*" \
            --ws \
            --ws.port "$ws_port" \
            --ws.api eth,net,web3 \
            --authrpc.port "$auth_port" \
            --port "$p2p_port" \
            --discovery.port "$p2p_port" \
            --discovery.v5.port "$discovery_v5_port" \
            --log.file.directory "$datadir/logs" \
            --ipcdisable \
            --max-outbound-peers "$NUM_VALIDATORS" \
            --max-inbound-peers "$NUM_VALIDATORS" \
            --metrics "127.0.0.1:$metrics_port" \
            --builder.gaslimit ${N42_GAS_LIMIT:-2000000000} \
            --builder.interval 50ms \
            --rpc.max-request-size 50 \
            --rpc.max-response-size 50 \
            --disable-tx-gossip \
            ${low_mem_flags} \
            ${N42_EXTRA_NODE_FLAGS:-} \
            ${p2p_key_flag} \
            ${TRUSTED_PEERS_FLAG} \
            > "$log_file" 2>&1 &

        NODE_PIDS+=($!)

        # Record enode for subsequent nodes
        if [[ "$NUM_VALIDATORS" -gt 1 ]]; then
            ENODES+=("enode://${P2P_PUBKEYS[$i]}@127.0.0.1:${p2p_port}")
        fi

        # Staggered startup
        if [[ "$NODE_START_INTERVAL" -gt 0 ]] && [[ $i -lt $((NUM_VALIDATORS - 1)) ]]; then
            sleep "$NODE_START_INTERVAL"
        fi
    done

    # Write PIDs to file for error monitor
    PIDS_CSV=$(IFS=,; echo "${NODE_PIDS[*]}")
    echo "$PIDS_CSV" > "$DATA_DIR/node-pids.txt"
}

# ---------------------------------------------------------------------------
# Wait for block production
# ---------------------------------------------------------------------------

wait_for_blocks() {
    log "Waiting for block production (up to ${MAX_BLOCK_WAIT}s)..."
    local waited=0
    while [[ $waited -lt $MAX_BLOCK_WAIT ]]; do
        block_hex=$(curl -s --max-time 3 \
            -X POST -H 'Content-Type: application/json' \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
            "http://127.0.0.1:$BASE_HTTP_RPC/" 2>/dev/null | \
            python3 -c "import sys,json; print(json.load(sys.stdin).get('result','0x0'))" 2>/dev/null || echo "0x0")

        block_num=$(printf '%d' "$block_hex" 2>/dev/null || echo 0)
        if [[ $block_num -gt 0 ]]; then
            log "Chain active! Block $block_num produced."
            return
        fi

        sleep 3
        waited=$((waited + 3))
        local report_interval=15
        [[ "$NUM_VALIDATORS" -gt 10 ]] && report_interval=30
        [[ $((waited % report_interval)) -eq 0 ]] && info "  Waiting... (${waited}s / ${MAX_BLOCK_WAIT}s)"
    done

    warn "Timeout waiting for blocks. Check: tail -f $DATA_DIR/validator-0.log"
}

# ---------------------------------------------------------------------------
# Start Blockscout (optional)
# ---------------------------------------------------------------------------

start_blockscout() {
    if [[ "$NO_EXPLORER" == true ]]; then
        return
    fi

    log "Starting Blockscout block explorer..."

    if ! command -v docker &>/dev/null; then
        warn "Docker not found — skipping Blockscout"
        return
    fi
    if ! docker info &>/dev/null 2>&1; then
        warn "Docker daemon not running — skipping Blockscout"
        return
    fi

    cat > "$DATA_DIR/blockscout-override.yml" << YAMLEOF
# Auto-generated for ${NUM_VALIDATORS}-node testnet (port $BASE_HTTP_RPC)
services:
  backend:
    environment:
      ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_TRACE_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:${BASE_WS}
  frontend:
    environment:
      NEXT_PUBLIC_APP_HOST: ${PUBLIC_IP}
      NEXT_PUBLIC_APP_PROTOCOL: http
      NEXT_PUBLIC_API_HOST: ${PUBLIC_IP}
      NEXT_PUBLIC_API_PROTOCOL: http
YAMLEOF

    if docker compose \
        -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
        -f "$DATA_DIR/blockscout-override.yml" \
        -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" \
        up -d 2>&1 | tail -5; then
        BLOCKSCOUT_STARTED=true
        log "Blockscout: http://localhost:3000 (UI)  http://localhost:4000 (API)"
    else
        warn "Blockscout failed to start (non-fatal, continuing without explorer)"
    fi
}

# ---------------------------------------------------------------------------
# Start transaction generator (optional)
# ---------------------------------------------------------------------------

start_tx_generator() {
    if [[ "$NO_TX_GEN" == true ]]; then
        return
    fi

    log "Starting transaction load generator (2 tx/sec)..."
    python3 "$SCRIPT_DIR/tx-load-generator.py" \
        --rpc "http://127.0.0.1:$BASE_HTTP_RPC" \
        --rate 2 \
        > "$DATA_DIR/tx-generator.log" 2>&1 &

    TX_GEN_PID=$!
    log "  TX generator PID: $TX_GEN_PID"
}

# ---------------------------------------------------------------------------
# Start mobile phone simulator (optional)
# ---------------------------------------------------------------------------

start_mobile_sim() {
    if [[ "$NO_MOBILE_SIM" == true ]]; then
        return
    fi
    if [[ ! -f "$MOBILE_SIM_BIN" ]]; then
        warn "n42-mobile-sim binary not found, skipping"
        return
    fi

    local phone_count=$NUM_VALIDATORS
    if [[ "$NUM_VALIDATORS" -eq 1 ]]; then
        STARHUB_PORTS="$BASE_STARHUB"
    else
        STARHUB_PORTS=$(seq -s, $BASE_STARHUB $((BASE_STARHUB + NUM_VALIDATORS - 1)) | sed 's/,$//')
    fi
    log "Starting mobile simulator (${phone_count} phones, ports: ${STARHUB_PORTS})..."

    N42_MIN_ATTESTATION_THRESHOLD=1 \
    "$MOBILE_SIM_BIN" \
        --starhub-ports "$STARHUB_PORTS" \
        --phone-count "$phone_count" \
        > "$DATA_DIR/mobile-sim.log" 2>&1 &

    MOBILE_SIM_PID=$!
    log "  Mobile simulator PID: $MOBILE_SIM_PID (${phone_count} phones)"
}

# ---------------------------------------------------------------------------
# Start error monitor (optional)
# ---------------------------------------------------------------------------

start_error_monitor() {
    if [[ "$NO_MONITOR" == true ]]; then
        return
    fi

    log "Starting error monitor..."
    bash "$SCRIPT_DIR/error-monitor.sh" \
        --data-dir "$DATA_DIR" \
        --pids "$PIDS_CSV" \
        --nodes "$NUM_VALIDATORS" \
        --base-http-rpc "$BASE_HTTP_RPC" \
        > "$DATA_DIR/monitor.log" 2>&1 &

    MONITOR_PID=$!
    log "  Error monitor PID: $MONITOR_PID"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

print_summary() {
    echo ""
    log "===== N42 ${NUM_VALIDATORS}-Node Testnet Running ====="
    echo ""
    echo -e "  ${CYAN}Data directory:${NC}  $DATA_DIR"
    echo ""
    echo -e "  ${CYAN}Validator nodes (${NUM_VALIDATORS}):${NC}"
    for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
        printf "    v%-2d  http://127.0.0.1:%d  PID=%d\n" \
            "$i" "$((BASE_HTTP_RPC + i))" "${NODE_PIDS[$i]}"
    done
    echo ""
    [[ "$BLOCKSCOUT_STARTED" == true ]] && echo -e "  ${CYAN}Blockscout:${NC}   http://localhost:3000"
    [[ -n "$TX_GEN_PID"    ]] && echo -e "  ${CYAN}TX Generator:${NC} PID=$TX_GEN_PID (2 tx/s)"
    [[ -n "$MOBILE_SIM_PID" ]] && echo -e "  ${CYAN}Mobile Sim:${NC}   PID=$MOBILE_SIM_PID"
    [[ -n "$MONITOR_PID"   ]] && echo -e "  ${CYAN}Monitor:${NC}      PID=$MONITOR_PID"
    echo ""
    echo -e "  ${YELLOW}Quick commands:${NC}"
    echo "    # Check block number:"
    echo "    curl -s http://127.0.0.1:$BASE_HTTP_RPC -X POST -H 'Content-Type: application/json' \\"
    echo "      -d '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}'"
    echo ""
    echo "    # Check consensus status:"
    echo "    curl -s http://127.0.0.1:$BASE_HTTP_RPC -X POST -H 'Content-Type: application/json' \\"
    echo "      -d '{\"jsonrpc\":\"2.0\",\"method\":\"n42_consensusStatus\",\"params\":[],\"id\":1}'"
    echo ""
    echo "    # Tail logs:"
    echo "    tail -f $DATA_DIR/validator-0.log"
    echo ""
    log "Press Ctrl+C to stop all services"
    echo ""
}

# ---------------------------------------------------------------------------
# Keep alive — watchdog for node processes
# ---------------------------------------------------------------------------

keepalive_loop() {
    local watchdog_interval=10
    [[ "$NUM_VALIDATORS" -gt 10 ]] && watchdog_interval=15

    while true; do
        local alive=0
        for i in $(seq 0 $((NUM_VALIDATORS - 1))); do
            kill -0 "${NODE_PIDS[$i]}" 2>/dev/null && alive=$((alive + 1))
        done

        if [[ $alive -eq 0 ]]; then
            err "All validator nodes have exited! Check: $DATA_DIR/validator-*.log"
            exit 1
        elif [[ $alive -lt $NUM_VALIDATORS ]]; then
            warn "Only $alive/$NUM_VALIDATORS nodes alive"
        fi

        sleep "$watchdog_interval"
    done
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

parse_args "$@"
compute_timeouts

LOG_PREFIX="${NUM_VALIDATORS}node"

log "N42 Unified Testnet — ${NUM_VALIDATORS} validators"
log "Data directory: $DATA_DIR"

# Clean data if requested
if [[ "$CLEAN" == true ]]; then
    warn "Cleaning testnet data at $DATA_DIR..."
    rm -rf "$DATA_DIR"
    if command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
        warn "Cleaning Blockscout Docker volumes..."
        docker compose \
            -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
            -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" \
            down -v 2>/dev/null || true
    fi
fi

mkdir -p "$DATA_DIR"

prepare_l3_cpusets

# Read-only topology probe used by the cpuset A/B qualification.  It stops
# before ulimit changes, builds, key generation, or node startup.
if [[ "${N42_TESTNET_TOPOLOGY_ONLY:-0}" == "1" ]]; then
    log "Topology-only probe complete; no validators were started"
    exit 0
fi

setup_ulimit
setup_python_venv
build_binaries
generate_genesis
generate_bls_keys
generate_p2p_keys
start_validators
wait_for_blocks
start_blockscout
start_tx_generator
start_mobile_sim
start_error_monitor

# Open browser for Blockscout
if [[ "$BLOCKSCOUT_STARTED" == true ]]; then
    sleep 3
    if command -v open &>/dev/null; then
        open "http://localhost:3000" 2>/dev/null || true
    fi
fi

print_summary
keepalive_loop
