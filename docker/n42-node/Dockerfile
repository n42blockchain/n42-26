# N42 Node — Multi-stage Docker build
# Stage 1: Build the n42-node binary from source
# Stage 2: Create a minimal runtime image

# ── Build stage ──
FROM rust:1.82-bookworm AS builder

WORKDIR /build

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    clang \
    libclang-dev \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY bin/ bin/

# Copy the local reth dependency (referenced via path = "../reth" in Cargo.toml)
# In Docker context, reth source must be available. The docker-compose.yml
# mounts the parent directory or uses a build context that includes ../reth.
# For standalone builds, copy reth into the build context first.
COPY reth/ /build/reth/

# Build release binary
RUN cargo build --release -p n42-node \
    && strip /build/target/release/n42-node

# ── Runtime stage ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -s /bin/bash n42

# Copy binary from builder
COPY --from=builder /build/target/release/n42-node /usr/local/bin/n42-node

# Create data directory
RUN mkdir -p /data/n42 && chown n42:n42 /data/n42

USER n42
WORKDIR /data/n42

# ── Environment variable defaults ──
# See config.example.toml for full documentation.

# Consensus
ENV N42_VALIDATOR_COUNT="1"
ENV N42_DATA_DIR="/data/n42"

# Network
ENV N42_CONSENSUS_PORT="9400"
ENV N42_STARHUB_PORT="9443"
ENV N42_TRUSTED_PEERS=""
ENV N42_ENABLE_MDNS="false"
ENV N42_ENABLE_DHT="false"

# Expose ports:
#   8545 - HTTP RPC (reth default)
#   8546 - WebSocket RPC (reth default)
#   9400 - Consensus P2P (QUIC)
#   9443 - Mobile verifier StarHub (QUIC)
#   9001 - Prometheus metrics
EXPOSE 8545 8546 9400/udp 9443/udp 9001

# Health check via n42 RPC
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -sf http://localhost:8545/ -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"n42_health","params":[],"id":1}' \
        | grep -q '"status":"ok"' || exit 1

ENTRYPOINT ["n42-node", "node"]
CMD ["--http", "--http.addr", "0.0.0.0", "--metrics", "0.0.0.0:9001"]
