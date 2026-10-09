#!/usr/bin/env bash
# Run only as a foreground child of the shared box-claim driver.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Run under the shared box-claim driver with N42_QUIET_CLAIM=1" >&2
    exit 2
fi

ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924}"
if [[ -e "$ARTIFACT_DIR" ]]; then
    echo "Refusing to overwrite existing fleet: $ARTIFACT_DIR" >&2
    exit 2
fi

mkdir -p "$PROJECT_DIR/.artifacts"
nice -n 19 systemd-run --user --scope -q -p MemoryMax=40G -p MemorySwapMax=0 \
    cargo test --offline --locked --release -j 8 -p n42-consensus --bin n42-verify-commit \
    seven_validator_h2v4_commit_requires_five_signers
nice -n 19 systemd-run --user --scope -q -p MemoryMax=40G -p MemorySwapMax=0 \
    cargo test --offline --locked --release -j 8 -p n42-node-bin --bin n42-native-fleet \
    accepts_seven_validator_fleet
nice -n 19 systemd-run --user --scope -q -p MemoryMax=40G -p MemorySwapMax=0 \
    cargo build --offline --locked --release -j 8 -p n42-node-bin --bin n42-node --bin n42-native-fleet \
    -p n42-stress --bin n42-stress --bin n42-keccak \
    -p n42-consensus --bin n42-verify-commit

target/release/n42-native-fleet --output "$ARTIFACT_DIR" --chain-id 941007 \
    --validators 7 --senders 5000 --recipients 147000 --slot-ms 200 \
    --gas-limit 5000000000 --max-txs-per-block 220000

RPC_URLS="$(python3 -c 'print(",".join(f"http://127.0.0.1:{23400+i}" for i in range(7)))')"
N42_CHAIN_ID=941007 nice -n 19 target/release/n42-stress \
    --presign-genesis "$ARTIFACT_DIR/artifacts/genesis.json" \
    --recipients-file "$ARTIFACT_DIR/recipients.json" \
    --presign-save "$ARTIFACT_DIR/presigned-24m.bin" \
    --presign 24000000 --accounts 5000 --rpc "$RPC_URLS"

sha256sum target/release/n42-node target/release/n42-native-fleet \
    target/release/n42-stress target/release/n42-verify-commit \
    "$ARTIFACT_DIR/presigned-24m.bin" > "$ARTIFACT_DIR/binary-workload-sha256.txt"
git rev-parse HEAD > "$ARTIFACT_DIR/source-head.txt"
git status --short > "$ARTIFACT_DIR/source-status.txt"
echo "Prepared fresh seven-validator fleet and pre-signed workload in $ARTIFACT_DIR"
