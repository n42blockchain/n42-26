#!/usr/bin/env bash
set -euo pipefail

: "${N42_GOV5_GENESIS:?set N42_GOV5_GENESIS to the gov5 custom-chain genesis.json}"
: "${N42_CONSENSUS_CONFIG:?set N42_CONSENSUS_CONFIG to the matching validator config}"
: "${N42_INTEROP_GENESIS_HASH:?set N42_INTEROP_GENESIS_HASH to gov5 block 0 hash}"
: "${N42_TRUSTED_PEERS:?set N42_TRUSTED_PEERS to one or more gov5 TCP multiaddrs}"

n42_node_bin="${N42_NODE_BIN:-target/debug/n42-node}"
interop_runtime_root="${N42_INTEROP_RUNTIME_ROOT:-$PWD/.n42-interop-observer}"
consensus_port="${N42_CONSENSUS_PORT:-19400}"
reth_port="${N42_INTEROP_RETH_PORT:-30442}"
http_port="${N42_INTEROP_HTTP_PORT:-28600}"

if [[ ! -x "$n42_node_bin" ]]; then
    echo "n42-node binary is not executable: $n42_node_bin" >&2
    echo "build it with: cargo build -p n42-node-bin --bin n42-node" >&2
    exit 1
fi

if [[ ! -f "$N42_GOV5_GENESIS" ]]; then
    echo "gov5 genesis does not exist: $N42_GOV5_GENESIS" >&2
    exit 1
fi

if [[ -n "${N42_QMDB_BOOTSTRAP:-}" ]]; then
    : "${N42_QMDB_BOOTSTRAP_BLOCK:?set N42_QMDB_BOOTSTRAP_BLOCK with N42_QMDB_BOOTSTRAP}"
    : "${N42_QMDB_BOOTSTRAP_BLOCK_HASH:?set N42_QMDB_BOOTSTRAP_BLOCK_HASH with N42_QMDB_BOOTSTRAP}"
    : "${N42_QMDB_BOOTSTRAP_ROOT:?set N42_QMDB_BOOTSTRAP_ROOT with N42_QMDB_BOOTSTRAP}"
    if [[ ! -f "$N42_QMDB_BOOTSTRAP" ]]; then
        echo "QMDB bootstrap does not exist: $N42_QMDB_BOOTSTRAP" >&2
        exit 1
    fi
fi

if [[ "${N42_GOV5_QMDB_EXECUTION:-0}" == "1" ]]; then
    : "${N42_QMDB_BOOTSTRAP:?set N42_QMDB_BOOTSTRAP with N42_GOV5_QMDB_EXECUTION}"
    : "${N42_GOV5_GENESIS_BOOTSTRAP:?set N42_GOV5_GENESIS_BOOTSTRAP with N42_GOV5_QMDB_EXECUTION}"
    if [[ ! -f "$N42_GOV5_GENESIS_BOOTSTRAP" ]]; then
        echo "gov5 genesis bootstrap does not exist: $N42_GOV5_GENESIS_BOOTSTRAP" >&2
        exit 1
    fi
fi

if [[ -n "${N42_FINALIZED_RANGE_BOOTSTRAP:-}" ]]; then
    : "${N42_QMDB_BOOTSTRAP:?set N42_QMDB_BOOTSTRAP with N42_FINALIZED_RANGE_BOOTSTRAP}"
    if [[ ! -f "$N42_FINALIZED_RANGE_BOOTSTRAP" ]]; then
        echo "finalized range bootstrap does not exist: $N42_FINALIZED_RANGE_BOOTSTRAP" >&2
        exit 1
    fi
fi

if [[ "${N42_GOV5_REPLAY_IMPORT:-0}" == "1" ]]; then
    : "${N42_GOV5_QMDB_EXECUTION:?set N42_GOV5_QMDB_EXECUTION=1 with N42_GOV5_REPLAY_IMPORT}"
    : "${N42_FINALIZED_RANGE_BOOTSTRAP:?set N42_FINALIZED_RANGE_BOOTSTRAP with N42_GOV5_REPLAY_IMPORT}"
    if [[ "$N42_GOV5_QMDB_EXECUTION" != "1" ]]; then
        echo "N42_GOV5_QMDB_EXECUTION must be 1 with N42_GOV5_REPLAY_IMPORT" >&2
        exit 1
    fi
fi

if [[ ! -f "$N42_CONSENSUS_CONFIG" ]]; then
    echo "consensus config does not exist: $N42_CONSENSUS_CONFIG" >&2
    exit 1
fi

case "$interop_runtime_root" in
    "" | /)
        echo "refusing unsafe N42_INTEROP_RUNTIME_ROOT: $interop_runtime_root" >&2
        exit 1
        ;;
esac

mkdir -p "$interop_runtime_root/consensus" "$interop_runtime_root/reth"

exec env \
    N42_OBSERVER_MODE=1 \
    N42_GOV5_HEADER_PROFILE=1 \
    N42_CONSENSUS_CONFIG="$N42_CONSENSUS_CONFIG" \
    N42_INTEROP_GENESIS_HASH="$N42_INTEROP_GENESIS_HASH" \
    N42_TRUSTED_PEERS="$N42_TRUSTED_PEERS" \
    N42_DATA_DIR="$interop_runtime_root/consensus" \
    N42_CONSENSUS_PORT="$consensus_port" \
    "$n42_node_bin" node \
    --chain "$N42_GOV5_GENESIS" \
    --datadir "$interop_runtime_root/reth" \
    --disable-discovery \
    --port "$reth_port" \
    --http \
    --http.addr 127.0.0.1 \
    --http.port "$http_port" \
    --ipcdisable \
    --disable-auth-server \
    --log.file.max-files 0 \
    --color never \
    -vvv \
    "$@"
