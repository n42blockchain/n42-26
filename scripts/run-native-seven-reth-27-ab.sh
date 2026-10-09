#!/usr/bin/env bash
# Compare pre-upgrade and Reth 2.7 binaries under one immutable seven-node workload.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname -- "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

if [[ "${N42_QUIET_CLAIM:-}" != 1 || ! -f /data/blockchain/.box-claim-codex || ! -f /data/blockchain/wr-logs/.box-claim-codex ]]; then
    echo "Requires the shared box claim on both directories" >&2
    exit 2
fi
for name in N42_RETH_OLD_BINARY N42_RETH_NEW_BINARY N42_RETH_OLD_MANIFEST N42_RETH_NEW_MANIFEST; do
    if [[ -z "${!name:-}" || ! -f "${!name}" ]]; then
        echo "$name must name an existing immutable build artifact" >&2
        exit 2
    fi
done

export N42_SEVEN_ARTIFACT_DIR="${N42_SEVEN_ARTIFACT_DIR:-$PROJECT_DIR/.artifacts/native-seven-20260924-v2}"
export N42_SEVEN_CAMPAIGN_DIR="${N42_SEVEN_CAMPAIGN_DIR:-$PROJECT_DIR/.artifacts/native-seven-reth-27-ab-20261003}"
export N42_SEVEN_PRESIGNED_TXS="${N42_SEVEN_PRESIGNED_TXS:-$PROJECT_DIR/.artifacts/native-seven-presigned-48m-20260927.bin}"
export N42_BENCH_MAX_TXS_PER_BLOCK=220000
export N42_BENCH_RPC_MAX_RESPONSE_MB=512
export N42_GOV5_REUSE_BUILDER_EXECUTION=1

if [[ -e "$N42_SEVEN_CAMPAIGN_DIR" ]]; then
    echo "Refusing to overwrite A/B/A campaign: $N42_SEVEN_CAMPAIGN_DIR" >&2
    exit 2
fi
for file in "$N42_RETH_OLD_BINARY" "$N42_RETH_NEW_BINARY"; do
    if [[ ! -x "$file" || "$(basename -- "$file")" != n42-node ]]; then
        echo "Expected executable n42-node binary: $file" >&2
        exit 2
    fi
done
for file in "$N42_RETH_OLD_MANIFEST" "$N42_RETH_NEW_MANIFEST"; do
    python3 - "$file" <<'PY'
import json, sys
from pathlib import Path
m = json.loads(Path(sys.argv[1]).read_text())
required = ("reth_revision", "reth_source_sha256", "lockfile_sha256", "source_tree_sha256")
if not all(m.get(key) for key in required):
    raise SystemExit(f"incomplete source manifest: {sys.argv[1]}")
PY
done

export N42_SEVEN_LEG_OPTIONS="--parallel-build"
quiet_between_legs() {
    local sample
    for sample in 1 2 3; do
      python3 - <<'PY'
import os, time
from pathlib import Path
dirs = (Path('/data/blockchain'), Path('/data/blockchain/wr-logs'))
now = time.time()
for directory in dirs:
    for claim in directory.glob('.box-claim-*'):
        if claim.name == '.box-claim-codex':
            continue
        try:
            stamp = int(claim.read_text().strip())
            if now - max(stamp, claim.stat().st_mtime) < 90 * 60:
                raise SystemExit(f'active competing claim: {claim}')
        except ValueError:
            raise SystemExit(f'unreadable competing claim: {claim}')
load1 = os.getloadavg()[0]
mem = {line.split(':', 1)[0]: line.split(':', 1)[1].split()[0]
       for line in Path('/proc/meminfo').read_text().splitlines() if ':' in line}
available = int(mem['MemAvailable']) * 1024
heavy = []
for proc in Path('/proc').iterdir():
    if not proc.name.isdigit():
        continue
    try:
        name = (proc / 'comm').read_text().strip()
        if name.startswith(('n42', 'h2_validator', 'datc', 'txflood')) or name in {'reth', 'geth', 'cargo', 'rustc'}:
            heavy.append(f'{proc.name}:{name}')
    except (FileNotFoundError, PermissionError):
        continue
if load1 >= 8 or available < 80 * 1024**3 or heavy:
    raise SystemExit(f'host not quiet: load1={load1:.2f} available={available} heavy={heavy}')
PY
      if [[ "$sample" != 3 ]]; then sleep 30; fi
    done
}
verify_binary_unchanged() {
    local tag="$1" binary="$2" recorded current
    recorded="$(awk '$2 ~ /\/n42-node$/ {print $1}' "$N42_SEVEN_CAMPAIGN_DIR/result-$tag/binary-workload-sha256.txt")"
    current="$(sha256sum "$binary" | awk '{print $1}')"
    if [[ -z "$recorded" || "$recorded" != "$current" ]]; then
      echo "n42-node binary changed during leg $tag" >&2
      return 1
    fi
}
restart_verify() {
    local tag="$1" node="$2" runtime="$N42_SEVEN_CAMPAIGN_DIR/runtime-$1"
    local result="$N42_SEVEN_CAMPAIGN_DIR/result-$1" pid="" ready=0
    python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$runtime" start \
      --foreground --binary "$node" --disable-tx-forward --qmdb-reads only \
      --max-txs-per-block "$N42_BENCH_MAX_TXS_PER_BLOCK" \
      --rpc-max-response-mb "$N42_BENCH_RPC_MAX_RESPONSE_MB" --parallel-build \
      > "$result/restart-supervisor.log" 2>&1 &
    pid=$!
    cleanup_restart() {
      if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -INT "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
      fi
      python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$runtime" stop \
        >> "$result/restart-supervisor.log" 2>&1 || true
    }
    trap cleanup_restart RETURN
    for _ in $(seq 1 180); do
      if ! kill -0 "$pid" 2>/dev/null; then
        echo "Restarted fleet exited before readiness: $tag" >&2
        return 1
      fi
      if curl -fsS --max-time 1 -H 'Content-Type: application/json' \
        -d '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
        http://127.0.0.1:23400/ >/dev/null 2>&1; then ready=1; break; fi
      sleep 1
    done
    if [[ "$ready" != 1 ]]; then echo "Restarted fleet did not become ready: $tag" >&2; return 1; fi
    python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$runtime" status \
      > "$result/restart-status.log" 2>&1
    python3 "$SCRIPT_DIR/chain94-fleet.py" --runtime "$runtime" verify \
      --seconds 30 --min-blocks 2 > "$result/verify-after-restart.log" 2>&1
    grep -q 'all 7 roots/hashes agree' "$result/verify-after-restart.log"
    printf '\nPASS: post-restart state agrees on all 7 validators\n' >> "$result/verify-after.log"
    cleanup_restart
    pid=""
    trap - RETURN
}
N42_SEVEN_LEG_TAG=reth-a1 N42_SEVEN_NODE_BINARY="$N42_RETH_OLD_BINARY" \
  N42_RETH_SOURCE_MANIFEST="$N42_RETH_OLD_MANIFEST" bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"
verify_binary_unchanged reth-a1 "$N42_RETH_OLD_BINARY"
restart_verify reth-a1 "$N42_RETH_OLD_BINARY"
quiet_between_legs
N42_SEVEN_LEG_TAG=reth-b N42_SEVEN_NODE_BINARY="$N42_RETH_NEW_BINARY" \
  N42_RETH_SOURCE_MANIFEST="$N42_RETH_NEW_MANIFEST" bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"
verify_binary_unchanged reth-b "$N42_RETH_NEW_BINARY"
restart_verify reth-b "$N42_RETH_NEW_BINARY"
quiet_between_legs
N42_SEVEN_LEG_TAG=reth-a2 N42_SEVEN_NODE_BINARY="$N42_RETH_OLD_BINARY" \
  N42_RETH_SOURCE_MANIFEST="$N42_RETH_OLD_MANIFEST" bash "$SCRIPT_DIR/run-native-seven-220k-warmed.sh"
verify_binary_unchanged reth-a2 "$N42_RETH_OLD_BINARY"
restart_verify reth-a2 "$N42_RETH_OLD_BINARY"

python3 "$SCRIPT_DIR/summarize_native_seven_reth_ab.py" "$N42_SEVEN_CAMPAIGN_DIR"
