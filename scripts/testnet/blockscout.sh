#!/usr/bin/env bash
#
# N42 Testnet — Blockscout Block Explorer
#
# Starts Blockscout via Docker Compose. Ctrl+C stops it.
#
# Usage:
#   ./scripts/testnet/blockscout.sh
#   ./scripts/testnet/blockscout.sh --data-dir ~/my-testnet
#   ./scripts/testnet/blockscout.sh --public-ip 1.2.3.4
#
# Requires setup.sh and nodes.sh to have been started first.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/common.sh"

LOG_PREFIX="blockscout"

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
DATA_DIR=""
PUBLIC_IP="localhost"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)  DATA_DIR="$2"; shift 2 ;;
        --public-ip) PUBLIC_IP="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--data-dir DIR] [--public-ip IP]"
            exit 0
            ;;
        *) err "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "$DATA_DIR" ]]; then
    for candidate in "$HOME/n42-testnet-data" "$HOME/n42-testnet-data-"*; do
        if [[ -f "$candidate/config.env" ]]; then
            DATA_DIR="$candidate"
            break
        fi
    done
fi

if [[ -z "$DATA_DIR" ]]; then
    err "Could not find testnet data dir. Run setup.sh first, or pass --data-dir."
    exit 1
fi

load_config "$DATA_DIR"

# ---------------------------------------------------------------------------
# Check Docker
# ---------------------------------------------------------------------------
if ! command -v docker &>/dev/null; then
    err "Docker not found"
    exit 1
fi
if ! docker info &>/dev/null 2>&1; then
    err "Docker daemon not running"
    exit 1
fi

# ---------------------------------------------------------------------------
# Cleanup
# ---------------------------------------------------------------------------
cleanup() {
    echo ""
    log "Stopping Blockscout..."
    docker compose \
        -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
        -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" \
        down 2>/dev/null || true
    log "Blockscout stopped."
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Generate override file and start
# ---------------------------------------------------------------------------

# NEXT_PUBLIC_API_HOST must be reachable from BOTH the browser AND the
# Next.js SSR server running inside the frontend Docker container.
#
# Problem: "localhost" inside Docker refers to the container itself, not to
# the nginx proxy on the host — so SSR fetches fail → 500 error on address pages.
#
# Solution:
#   - When no public IP: use host.docker.internal (Docker resolves it to the
#     host machine; also works in the browser on macOS/Windows Docker Desktop).
#   - When a real public IP is given: use it directly — SSR traffic routes
#     back through the host network and the browser accesses it directly.
if [[ "$PUBLIC_IP" == "localhost" ]]; then
    API_HOST="host.docker.internal"
    EXTRA_HOSTS_BLOCK="    extra_hosts:
      - \"host.docker.internal:host-gateway\""
else
    API_HOST="$PUBLIC_IP"
    EXTRA_HOSTS_BLOCK=""
fi

cat > "$DATA_DIR/blockscout-override.yml" << YAMLEOF
# Auto-generated for ${NUM_VALIDATORS}-node testnet
services:
  backend:
    environment:
      ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_TRACE_URL: http://host.docker.internal:${BASE_HTTP_RPC}/
      ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:${BASE_WS}
  frontend:
${EXTRA_HOSTS_BLOCK}
    environment:
      NEXT_PUBLIC_API_HOST: ${API_HOST}
      NEXT_PUBLIC_API_PROTOCOL: http
      NEXT_PUBLIC_APP_HOST: ${PUBLIC_IP}
      NEXT_PUBLIC_APP_PROTOCOL: http
YAMLEOF

log "Starting Blockscout (public IP: ${PUBLIC_IP})..."
log "Press Ctrl+C to stop."

docker compose \
    -f "$PROJECT_DIR/docker/docker-compose.blockscout.yml" \
    -f "$DATA_DIR/blockscout-override.yml" \
    -p "n42-testnet-${NUM_VALIDATORS}n-blockscout" \
    up
