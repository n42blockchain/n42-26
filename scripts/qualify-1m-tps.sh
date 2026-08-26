#!/usr/bin/env bash
# One-minute, already-running-cluster qualification for the 1M TPS architecture.
# Captures strict committed throughput plus CPU/RSS/disk/network/direct-QUIC data.

set -euo pipefail

NODES="${N42_BENCH_NODES:-7}"
DURATION="${N42_BENCH_DURATION_SECS:-60}"
TARGET_TPS="${N42_BENCH_TARGET_TPS:-1200000}"
WAVE="${N42_BENCH_WAVE_TXS:-163000}"
STRESS_BIN="${N42_STRESS_BIN:-target/release/n42-stress}"
PRESIGNED="${N42_PRESIGNED_TXS:-/data/n42-bench-artifacts-20260823/presigned-30m.bin}"
DATA_DIR="${N42_BENCH_DATA_DIR:-/data/n42-bench-current}"
VARIANT="${N42_BENCH_VARIANT:-control}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-${VARIANT}"
ARTIFACT_DIR="${N42_BENCH_ARTIFACT_DIR:-$PWD/bench-artifacts/$RUN_ID}"

if ! [[ "$NODES" =~ ^[1-9][0-9]*$ && "$DURATION" =~ ^[1-9][0-9]*$ ]]; then
    echo "N42_BENCH_NODES and N42_BENCH_DURATION_SECS must be positive integers" >&2
    exit 2
fi
if [[ ! -x "$STRESS_BIN" ]]; then
    echo "Missing stress binary: $STRESS_BIN (build with cargo build --release --bin n42-stress)" >&2
    exit 2
fi
if [[ ! -r "$PRESIGNED" ]]; then
    echo "Missing pre-signed transaction file: $PRESIGNED" >&2
    exit 2
fi

mkdir -p "$ARTIFACT_DIR"

rpc_urls=()
ingest_endpoints=()
metrics_ports=()
for ((node=0; node<NODES; node++)); do
    rpc_urls+=("http://127.0.0.1:$((18000 + node))")
    ingest_endpoints+=("127.0.0.1:$((19900 + node))")
    metrics_ports+=("$((19200 + node))")
done
rpc_csv="$(IFS=,; echo "${rpc_urls[*]}")"
ingest_csv="$(IFS=,; echo "${ingest_endpoints[*]}")"

rpc_block_number() {
    curl -fsS --max-time 2 -H 'Content-Type: application/json' \
        --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' "$1" \
        | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["result"], 16))'
}

collect_metrics() {
    local phase="$1" port
    for port in "${metrics_ports[@]}"; do
        curl -fsS --max-time 3 "http://127.0.0.1:${port}/" \
            > "$ARTIFACT_DIR/metrics-${phase}-${port}.prom" || true
    done
}

for rpc in "${rpc_urls[@]}"; do
    rpc_block_number "$rpc" >/dev/null || {
        echo "RPC is not ready: $rpc" >&2
        exit 2
    }
done

{
    printf 'run_id\t%s\n' "$RUN_ID"
    printf 'variant\t%s\n' "$VARIANT"
    printf 'duration_seconds\t%s\n' "$DURATION"
    printf 'target_submission_tps\t%s\n' "$TARGET_TPS"
    printf 'wave_transactions\t%s\n' "$WAVE"
    printf 'nodes\t%s\n' "$NODES"
    printf 'chain_id\t%s\n' "${N42_CHAIN_ID:-4242}"
    printf 'max_transactions_per_block\t%s\n' "${N42_MAX_TXS_PER_BLOCK:-48000}"
    printf 'skip_transaction_verification\t%s\n' "${N42_SKIP_TX_VERIFY:-0}"
    printf 'defer_state_root\t%s\n' "${N42_DEFER_STATE_ROOT:-0}"
    printf 'presigned_file\t%s\n' "$PRESIGNED"
    printf 'presigned_bytes\t%s\n' "$(stat -c %s "$PRESIGNED")"
    printf 'data_dir\t%s\n' "$DATA_DIR"
    printf 'rpc\t%s\n' "$rpc_csv"
    printf 'ingest\t%s\n' "$ingest_csv"
    printf 'cpuset_mode\t%s\n' "${N42_CPUSET_AB_MODE:-off}"
    printf 'execution_lanes\t%s\n' "${N42_EXECUTION_LANES:-8}"
    printf 'sender_sharded_drain\t%s\n' "${N42_SENDER_SHARDED_DRAIN:-1}"
    printf 'async_finalize_fcu\t%s\n' "${N42_ASYNC_FINALIZE_FCU:-1}"
    printf 'payload_zstd\t%s\n' "${N42_PAYLOAD_ZSTD:-1}"
    printf 'zstd_level\t%s\n' "${N42_ZSTD_LEVEL:-3}"
    printf 'block_direct_only\t%s\n' "${N42_BLOCK_DIRECT_ONLY:-0}"
    printf 'block_direct_fanout\t%s\n' "${N42_BLOCK_DIRECT_FANOUT:-$((NODES - 1))}"
    printf 'block_direct_chunk_mib\t%s\n' "${N42_BLOCK_DIRECT_CHUNK_MIB:-4}"
    printf 'quic_max_stream_data\t%s\n' "${N42_QUIC_MAX_STREAM_DATA:-41943040}"
    printf 'quic_max_connection_data\t%s\n' "${N42_QUIC_MAX_CONNECTION_DATA:-100663296}"
    printf 'mobile_packets_disabled\t%s\n' "${N42_DISABLE_MOBILE_PACKETS:-0}"
} > "$ARTIFACT_DIR/config.tsv"

pids=""
if [[ -r "$DATA_DIR/node-pids.txt" ]]; then
    pids="$(tr -d '[:space:]' < "$DATA_DIR/node-pids.txt")"
fi
monitor_pids=()
stress_pid=""
cleanup_monitors() {
    local pid
    if [[ -n "$stress_pid" ]]; then
        kill "$stress_pid" 2>/dev/null || true
    fi
    for pid in "${monitor_pids[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup_monitors EXIT INT TERM

gate_file="$ARTIFACT_DIR/start.gate"
ready_marker="$ARTIFACT_DIR/ingest-ready.ns"
start_marker="$ARTIFACT_DIR/ingest-start.ns"
end_marker="$ARTIFACT_DIR/ingest-end.ns"

N42_SYNC_INGEST_CONTINUOUS=1 \
N42_DISABLE_TX_FORWARD=1 \
N42_BENCH_START_GATE_FILE="$gate_file" \
N42_BENCH_MARKER_DIR="$ARTIFACT_DIR" \
"$STRESS_BIN" \
    --duration "$DURATION" \
    --target-tps "$TARGET_TPS" \
    --accounts 5000 \
    --batch-size 4096 \
    --concurrency 4096 \
    --presign-load "$PRESIGNED" \
    --ingest "$ingest_csv" \
    --wave "$WAVE" \
    --sync-ingest-mode per-node-continuous \
    --ingest-soft-resume 300000 \
    --ingest-soft-target 380000 \
    --ingest-hard-target 430000 \
    --ingest-hard-cap 470000 \
    --ingest-target-spread 6000 \
    --rpc "$rpc_csv" > >(tee "$ARTIFACT_DIR/stress.log") 2>&1 &
stress_pid="$!"

deadline=$((SECONDS + 120))
while [[ ! -s "$ready_marker" ]]; do
    if ! kill -0 "$stress_pid" 2>/dev/null; then
        wait "$stress_pid"
        echo "Stress process exited before the ingest start gate" >&2
        exit 1
    fi
    if (( SECONDS >= deadline )); then
        echo "Timed out waiting for stress ingest start gate" >&2
        exit 1
    fi
    sleep 0.02
done

# The sender and duration timer are paused at the gate, so these snapshots are
# outside the measured window but immediately precede its first transaction.
cp /proc/net/dev "$ARTIFACT_DIR/net-before.txt"
cp /proc/net/snmp "$ARTIFACT_DIR/snmp-before.txt"
cp /proc/diskstats "$ARTIFACT_DIR/diskstats-before.txt"
cp /proc/vmstat "$ARTIFACT_DIR/vmstat-before.txt"
collect_metrics before
start_block="$(rpc_block_number "${rpc_urls[0]}")"

if [[ -n "$pids" ]] && command -v pidstat >/dev/null 2>&1; then
    pidstat -h -u -r -d -w -p "$pids" 1 "$DURATION" \
        > "$ARTIFACT_DIR/pidstat.txt" 2>&1 &
    monitor_pids+=("$!")
fi
if command -v iostat >/dev/null 2>&1; then
    iostat -dxm 1 "$DURATION" > "$ARTIFACT_DIR/iostat.txt" 2>&1 &
    monitor_pids+=("$!")
fi

touch "$gate_file"
while [[ ! -s "$start_marker" ]]; do sleep 0.01; done
start_ns="$(<"$start_marker")"

while [[ ! -s "$end_marker" ]]; do
    if ! kill -0 "$stress_pid" 2>/dev/null; then
        wait "$stress_pid"
        echo "Stress process exited before the ingest end marker" >&2
        exit 1
    fi
    sleep 0.02
done
end_ns="$(<"$end_marker")"
end_block="$(rpc_block_number "${rpc_urls[0]}")"

collect_metrics after
cp /proc/net/dev "$ARTIFACT_DIR/net-after.txt"
cp /proc/net/snmp "$ARTIFACT_DIR/snmp-after.txt"
cp /proc/diskstats "$ARTIFACT_DIR/diskstats-after.txt"
cp /proc/vmstat "$ARTIFACT_DIR/vmstat-after.txt"

for pid in "${monitor_pids[@]:-}"; do
    wait "$pid" 2>/dev/null || true
done
monitor_pids=()
wait "$stress_pid"
stress_pid=""

python3 - "$ARTIFACT_DIR" "${rpc_urls[0]}" "$start_block" "$end_block" "$start_ns" "$end_ns" <<'PY'
import glob
import json
import re
import sys
import urllib.request
from pathlib import Path

out = Path(sys.argv[1])
rpc = sys.argv[2]
start_block, end_block = map(int, sys.argv[3:5])
start_ns, end_ns = map(int, sys.argv[5:7])
elapsed = (end_ns - start_ns) / 1e9

def rpc_call(method, params):
    body = json.dumps({"jsonrpc":"2.0", "method":method, "params":params, "id":1}).encode()
    request = urllib.request.Request(rpc, data=body, headers={"Content-Type":"application/json"})
    with urllib.request.urlopen(request, timeout=10) as response:
        return json.load(response)["result"]

committed_txs = 0
for number in range(start_block + 1, end_block + 1):
    block = rpc_call("eth_getBlockByNumber", [hex(number), False])
    committed_txs += len(block["transactions"])

metric_names = [
    "n42_block_direct_bytes_sent_total",
    "n42_block_direct_bytes_received_total",
    "n42_block_direct_chunk_bytes_sent_total",
    "n42_block_direct_chunk_bytes_received_total",
    "n42_block_direct_chunks_sent_total",
    "n42_block_direct_chunks_received_total",
    "n42_block_direct_stream_transfers_received_total",
    "n42_block_direct_chunked_transfers_total",
    "n42_block_direct_send_failures",
    "n42_block_direct_remote_rejections_total",
    "n42_block_direct_queued_total",
    "n42_block_direct_queue_overflow_total",
    "n42_block_direct_retries_total",
    "n42_block_direct_digest_mismatch_total",
    "n42_block_direct_rejected_non_validator",
    "n42_validator_peer_auth_promotions_total",
    "n42_block_direct_ack_latency_ms_sum",
    "n42_block_direct_ack_latency_ms_count",
    "n42_sender_sharded_drain_ms_sum",
    "n42_sender_sharded_drain_ms_count",
    "n42_sender_sharded_group_ms_sum",
    "n42_sender_sharded_group_ms_count",
    "n42_sender_sharded_prepare_ms_sum",
    "n42_sender_sharded_prepare_ms_count",
    "n42_sender_sharded_merge_ms_sum",
    "n42_sender_sharded_merge_ms_count",
    "n42_sender_sharded_heap_runs_total",
    "n42_sender_sharded_batched_transactions_total",
    "n42_parallel_evm_blocks_total",
]

def metric_sum(phase, name):
    total = 0.0
    pattern = re.compile(r"^(?:reth_)?" + re.escape(name) + r"(?:\{[^}]*\})?\s+([-+0-9.eE]+)$")
    for filename in glob.glob(str(out / f"metrics-{phase}-*.prom")):
        for line in Path(filename).read_text(errors="replace").splitlines():
            match = pattern.match(line)
            if match:
                total += float(match.group(1))
    return total

def metric_sum_with_label(phase, name, key, value):
    total = 0.0
    pattern = re.compile(
        r"^(?:reth_)?" + re.escape(name) +
        r"\{([^}]*)\}\s+([-+0-9.eE]+)$"
    )
    label = re.compile(
        r"(?:^|,)" + re.escape(key) + r'=\"' + re.escape(value) + r'\"(?:,|$)'
    )
    for filename in glob.glob(str(out / f"metrics-{phase}-*.prom")):
        for line in Path(filename).read_text(errors="replace").splitlines():
            match = pattern.match(line)
            if match and label.search(match.group(1)):
                total += float(match.group(2))
    return total

rows = [
    ("start_block", start_block),
    ("end_block", end_block),
    ("committed_blocks", end_block - start_block),
    ("committed_transactions", committed_txs),
    ("measurement_seconds", f"{elapsed:.6f}"),
    ("strict_committed_tps", f"{committed_txs / elapsed:.2f}"),
]
for name in metric_names:
    rows.append((name + "_delta", f"{metric_sum('after', name) - metric_sum('before', name):.0f}"))
for stage in ("committed", "execution_ready", "finalized", "retryable"):
    before = metric_sum_with_label(
        "before", "n42_async_commit_transitions_total", "stage", stage
    )
    after = metric_sum_with_label(
        "after", "n42_async_commit_transitions_total", "stage", stage
    )
    rows.append((f"n42_async_commit_{stage}_delta", f"{after - before:.0f}"))
for status in ("valid", "syncing", "accepted", "invalid", "error"):
    before = metric_sum_with_label(
        "before", "n42_async_finalize_fcu_outcomes_total", "status", status
    )
    after = metric_sum_with_label(
        "after", "n42_async_finalize_fcu_outcomes_total", "status", status
    )
    rows.append((f"n42_async_finalize_fcu_{status}_delta", f"{after - before:.0f}"))

sent = float(dict(rows).get("n42_block_direct_bytes_sent_total_delta", 0))
received = float(dict(rows).get("n42_block_direct_bytes_received_total_delta", 0))
chunk_bytes = float(dict(rows).get("n42_block_direct_chunk_bytes_sent_total_delta", 0))
transfers = float(dict(rows).get("n42_block_direct_chunked_transfers_total_delta", 0))
ack_sum = float(dict(rows).get("n42_block_direct_ack_latency_ms_sum_delta", 0))
ack_count = float(dict(rows).get("n42_block_direct_ack_latency_ms_count_delta", 0))
drain_sum = float(dict(rows).get("n42_sender_sharded_drain_ms_sum_delta", 0))
drain_count = float(dict(rows).get("n42_sender_sharded_drain_ms_count_delta", 0))
group_sum = float(dict(rows).get("n42_sender_sharded_group_ms_sum_delta", 0))
group_count = float(dict(rows).get("n42_sender_sharded_group_ms_count_delta", 0))
prepare_sum = float(dict(rows).get("n42_sender_sharded_prepare_ms_sum_delta", 0))
prepare_count = float(dict(rows).get("n42_sender_sharded_prepare_ms_count_delta", 0))
merge_sum = float(dict(rows).get("n42_sender_sharded_merge_ms_sum_delta", 0))
merge_count = float(dict(rows).get("n42_sender_sharded_merge_ms_count_delta", 0))
rows.extend([
    ("direct_logical_send_MB_per_s", f"{sent / elapsed / 1_000_000:.3f}"),
    ("direct_logical_receive_MB_per_s", f"{received / elapsed / 1_000_000:.3f}"),
    ("chunked_transfer_mean_MB", f"{chunk_bytes / transfers / 1_000_000:.3f}" if transfers else "0.000"),
    ("direct_ack_mean_ms", f"{ack_sum / ack_count:.3f}" if ack_count else "0.000"),
    ("sender_sharded_drain_mean_ms", f"{drain_sum / drain_count:.3f}" if drain_count else "0.000"),
    ("sender_sharded_group_mean_ms", f"{group_sum / group_count:.3f}" if group_count else "0.000"),
    ("sender_sharded_prepare_mean_ms", f"{prepare_sum / prepare_count:.3f}" if prepare_count else "0.000"),
    ("sender_sharded_merge_mean_ms", f"{merge_sum / merge_count:.3f}" if merge_count else "0.000"),
])
(out / "summary.tsv").write_text("metric\tvalue\n" + "".join(f"{key}\t{value}\n" for key, value in rows))

def netdev(path):
    values = {}
    for line in Path(path).read_text().splitlines()[2:]:
        if ":" not in line:
            continue
        iface, rest = line.split(":", 1)
        fields = rest.split()
        values[iface.strip()] = (int(fields[0]), int(fields[8]))
    return values

before = netdev(out / "net-before.txt")
after = netdev(out / "net-after.txt")
with (out / "network-delta.tsv").open("w") as handle:
    handle.write("interface\treceive_bytes\ttransmit_bytes\treceive_MB_per_s\ttransmit_MB_per_s\n")
    for iface in sorted(set(before) | set(after)):
        rx = after.get(iface, (0, 0))[0] - before.get(iface, (0, 0))[0]
        tx = after.get(iface, (0, 0))[1] - before.get(iface, (0, 0))[1]
        handle.write(f"{iface}\t{rx}\t{tx}\t{rx/elapsed/1e6:.3f}\t{tx/elapsed/1e6:.3f}\n")
PY

trap - EXIT INT TERM
echo "Qualification complete: $ARTIFACT_DIR"
column -t -s $'\t' "$ARTIFACT_DIR/summary.tsv" 2>/dev/null \
    || cat "$ARTIFACT_DIR/summary.tsv"
