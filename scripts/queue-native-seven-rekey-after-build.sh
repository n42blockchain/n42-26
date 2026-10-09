#!/usr/bin/env bash
# Queue the measured round only after the test/build claim has finished.
set -euo pipefail

build_record="${1:?build claim record required}"
campaign="${2:?campaign directory required}"
build_sha="$campaign/release-node-sha256.txt"

while ! tail -n 1 "$build_record" 2>/dev/null | grep -q '"event": "released"'; do
    sleep 10
done
if [[ ! -s "$build_sha" ]]; then
    echo "Engine test or release build failed; no seven-node round queued" >&2
    exit 1
fi

export N42_QUIET_CLAIM=1
export N42_SEVEN_CAMPAIGN_DIR="$campaign"
exec python3 /home/n42/src/n42/N42-gov5/build/perf7/box-claim.py \
    --record "$campaign/benchmark-claim.jsonl" \
    -- bash scripts/run-native-seven-rekey-round.sh
