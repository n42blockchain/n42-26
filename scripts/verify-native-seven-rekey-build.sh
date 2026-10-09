#!/usr/bin/env bash
# Run under the shared box claim before the seven-node execution-reuse round.
set -euo pipefail

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Run under the shared box-claim driver with N42_QUIET_CLAIM=1" >&2
    exit 2
fi

campaign="${N42_REKEY_BUILD_CAMPAIGN:-.artifacts/native-seven-rekey-fixed-20260925}"
mkdir -p "$campaign"
cargo test -p n42-consensus-service corrupt_zstd_frame_is_rejected_without_poisoning_later_decodes --locked -j8 \
    > "$campaign/decoder-test.log" 2>&1 || {
        tail -n 80 "$campaign/decoder-test.log" >&2
        exit 1
    }
cargo test -p n42-node --test native_cached_qmdb_builder --locked -j8 \
    > "$campaign/engine-test.log" 2>&1 || {
        tail -n 80 "$campaign/engine-test.log" >&2
        exit 1
    }
cargo build --release --bin n42-node --locked -j8 \
    > "$campaign/release-build.log" 2>&1 || {
        tail -n 80 "$campaign/release-build.log" >&2
        exit 1
    }
sha256sum target/release/n42-node \
    > "$campaign/release-node-sha256.txt"
cat "$campaign/release-node-sha256.txt"
