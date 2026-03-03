#!/usr/bin/env bash
#
# DEPRECATED: Use scripts/testnet.sh --nodes 7 instead.
#
# This is a thin wrapper that forwards to the unified testnet launcher.
#

echo -e "\033[1;33m[DEPRECATED] testnet-7node.sh is deprecated. Use: scripts/testnet.sh --nodes 7\033[0m"
echo ""

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$SCRIPT_DIR/testnet.sh" --nodes 7 "$@"
