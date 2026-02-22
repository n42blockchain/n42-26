#!/usr/bin/env bash
# Mobile verification simulator for N42.
#
# Demonstrates the simplified RPC-based mobile verification flow:
#   1. Subscribe to verification tasks via WebSocket
#   2. Submit BLS attestation via HTTP RPC
#
# Prerequisites: wscat (npm install -g wscat) or websocat
#
# Usage:
#   ./scripts/mobile-sim.sh                    # Show help
#   ./scripts/mobile-sim.sh subscribe          # Subscribe to verification tasks
#   ./scripts/mobile-sim.sh status             # Query consensus status
#   ./scripts/mobile-sim.sh submit <pubkey> <signature> <block_hash> <slot>

set -euo pipefail

RPC_HTTP="${N42_RPC_HTTP:-http://127.0.0.1:8545}"
RPC_WS="${N42_RPC_WS:-ws://127.0.0.1:8546}"

rpc_call() {
    local method="$1"
    local params="$2"
    curl -s -X POST "$RPC_HTTP" \
        -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}"
}

case "${1:-help}" in
    subscribe)
        echo "Subscribing to verification tasks on $RPC_WS ..."
        echo "Press Ctrl+C to stop."
        if command -v websocat &>/dev/null; then
            echo '{"jsonrpc":"2.0","method":"n42_subscribeVerification","params":[],"id":1}' \
                | websocat "$RPC_WS"
        elif command -v wscat &>/dev/null; then
            wscat -c "$RPC_WS" \
                -x '{"jsonrpc":"2.0","method":"n42_subscribeVerification","params":[],"id":1}' \
                --wait 999999
        else
            echo "Error: install websocat or wscat (npm install -g wscat)"
            exit 1
        fi
        ;;

    status)
        echo "Querying consensus status..."
        rpc_call "n42_consensusStatus" "[]" | python3 -m json.tool 2>/dev/null || rpc_call "n42_consensusStatus" "[]"
        ;;

    validators)
        echo "Querying validator set..."
        rpc_call "n42_validatorSet" "[]" | python3 -m json.tool 2>/dev/null || rpc_call "n42_validatorSet" "[]"
        ;;

    submit)
        if [ $# -lt 5 ]; then
            echo "Usage: $0 submit <pubkey_hex> <signature_hex> <block_hash> <slot>"
            echo ""
            echo "  pubkey_hex    - 48-byte BLS public key (hex, with or without 0x)"
            echo "  signature_hex - 96-byte BLS signature (hex, with or without 0x)"
            echo "  block_hash    - 32-byte block hash (0x-prefixed)"
            echo "  slot          - Slot/view number"
            exit 1
        fi
        echo "Submitting attestation for block $4 at slot $5 ..."
        rpc_call "n42_submitAttestation" "[\"${2}\",\"${3}\",\"${4}\",${5}]" \
            | python3 -m json.tool 2>/dev/null \
            || rpc_call "n42_submitAttestation" "[\"${2}\",\"${3}\",\"${4}\",${5}]"
        ;;

    *)
        echo "N42 Mobile Verification Simulator"
        echo ""
        echo "Usage: $0 <command> [args...]"
        echo ""
        echo "Commands:"
        echo "  subscribe                              Subscribe to verification tasks"
        echo "  status                                 Query consensus status"
        echo "  validators                             Query validator set"
        echo "  submit <pubkey> <sig> <hash> <slot>    Submit BLS attestation"
        echo ""
        echo "Environment:"
        echo "  N42_RPC_HTTP   HTTP RPC endpoint (default: http://127.0.0.1:8545)"
        echo "  N42_RPC_WS     WebSocket endpoint (default: ws://127.0.0.1:8546)"
        ;;
esac
