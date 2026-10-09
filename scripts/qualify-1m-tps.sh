#!/usr/bin/env bash
# One-minute, already-running-cluster qualification for the 1M TPS architecture.
# Audits H2 boundaries, QMDB-only read progress and validator/process identities.
# Recomputes native header hashes and receipt roots; the threshold counts successful receipts.
# BLS commit certificates must bind the operator-authenticated H2-v4 roster and chain.
# EVM execution, transaction trie roots and hardware durability remain separate evidence.

set -euo pipefail

NODES="${N42_BENCH_NODES:-4}"
DURATION="${N42_BENCH_DURATION_SECS:-60}"
TARGET_TPS="${N42_BENCH_TARGET_TPS:-1200000}"
MIN_COMMITTED_TPS="${N42_BENCH_MIN_COMMITTED_TPS:-1000000}"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
WAVE="${N42_BENCH_WAVE_TXS:-163000}"
STRESS_BIN="${N42_STRESS_BIN:-target/release/n42-stress}"
TRUSTED_CONFIG="${N42_BENCH_TRUSTED_CONFIG:-}"
export N42_COMMIT_VERIFY_BIN="${N42_COMMIT_VERIFY_BIN:-target/release/n42-verify-commit}"
export N42_KECCAK_BIN="${N42_KECCAK_BIN:-target/release/n42-keccak}"
PRESIGNED="${N42_PRESIGNED_TXS:-/data/n42-bench-artifacts-20260823/presigned-30m.bin}"
DATA_DIR="${N42_BENCH_DATA_DIR:-/data/n42-bench-current}"
VARIANT="${N42_BENCH_VARIANT:-control}"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-${VARIANT}"
ARTIFACT_DIR="${N42_BENCH_ARTIFACT_DIR:-$PWD/bench-artifacts/$RUN_ID}"

if ! [[ "$NODES" =~ ^[1-9][0-9]*$ && "$DURATION" =~ ^[1-9][0-9]*$ ]]; then
    echo "N42_BENCH_NODES and N42_BENCH_DURATION_SECS must be positive integers" >&2
    exit 2
fi
if [[ "$NODES" != 4 && "$NODES" != 7 ]]; then
    echo "H2-v4 qualification requires four or seven validators" >&2
    exit 2
fi
if [[ ! -x "$STRESS_BIN" ]]; then
    echo "Missing stress binary: $STRESS_BIN (build with cargo build --release --bin n42-stress)" >&2
    exit 2
fi
if [[ ! -x "$N42_KECCAK_BIN" ]]; then
    echo "Missing Keccak helper: $N42_KECCAK_BIN (build with cargo build --release --bin n42-keccak)" >&2
    exit 2
fi
if [[ ! -x "$N42_COMMIT_VERIFY_BIN" ]]; then
    echo "Missing commit verifier: $N42_COMMIT_VERIFY_BIN (build with cargo build --release --bin n42-verify-commit)" >&2
    exit 2
fi
if [[ -z "$TRUSTED_CONFIG" || ! -r "$TRUSTED_CONFIG" ]]; then
    echo "Set N42_BENCH_TRUSTED_CONFIG to the authenticated H2-v4 chain and ordered validator configuration" >&2
    exit 2
fi
if [[ ! -r "$PRESIGNED" ]]; then
    echo "Missing pre-signed transaction file: $PRESIGNED" >&2
    exit 2
fi

mkdir -p "$ARTIFACT_DIR"
# Freeze the supplied file before any observations; never derive trust from RPC.
cp -- "$TRUSTED_CONFIG" "$ARTIFACT_DIR/trusted-config.json"
TRUSTED_CONFIG="$ARTIFACT_DIR/trusted-config.json"

# Reject wrong-chain and undersized files before contacting nodes or starting the clock.
python3 "$SCRIPT_DIR/presign-workload-preflight.py" --file "$PRESIGNED" \
    --trusted-config "$TRUSTED_CONFIG" --chain-id "${N42_CHAIN_ID:-4242}" \
    --duration "$DURATION" --minimum-tps "$MIN_COMMITTED_TPS" \
    --out "$ARTIFACT_DIR/presigned-workload.json"

RPC_BASE="${N42_BENCH_RPC_BASE:-18000}"
INGEST_BASE="${N42_BENCH_INGEST_BASE:-19900}"
METRICS_BASE="${N42_BENCH_METRICS_BASE:-19200}"
for base in "$RPC_BASE" "$INGEST_BASE" "$METRICS_BASE"; do
    if ! [[ "$base" =~ ^[1-9][0-9]*$ ]] || ((base + NODES - 1 > 65535)); then
        echo "Benchmark endpoint bases must fit valid TCP ports" >&2
        exit 2
    fi
done
rpc_urls=()
ingest_endpoints=()
metrics_ports=()
for ((node=0; node<NODES; node++)); do
    rpc_urls+=("http://127.0.0.1:$((RPC_BASE + node))")
    ingest_endpoints+=("127.0.0.1:$((INGEST_BASE + node))")
    metrics_ports+=("$((METRICS_BASE + node))")
done
rpc_csv="$(IFS=,; echo "${rpc_urls[*]}")"
ingest_csv="$(IFS=,; echo "${ingest_endpoints[*]}")"

collect_metrics() {
    local phase="$1" port
    for port in "${metrics_ports[@]}"; do
        curl -fsS --max-time 3 "http://127.0.0.1:${port}/" \
            > "$ARTIFACT_DIR/metrics-${phase}-${port}.prom" || true
    done
}

# Fail before loading the stress workload if a node is off/verify/on, lacks
# durable QMDB state, or aliases another validator/process.
python3 "$SCRIPT_DIR/h2-tps-audit.py" capture --trusted-config "$TRUSTED_CONFIG" --rpc "$rpc_csv" \
    --out "$ARTIFACT_DIR/h2-preflight.json" --verify-receipts

{
    printf 'run_id\t%s\n' "$RUN_ID"
    printf 'variant\t%s\n' "$VARIANT"
    printf 'duration_seconds\t%s\n' "$DURATION"
    printf 'target_submission_tps\t%s\n' "$TARGET_TPS"
    printf 'minimum_successful_committed_tps\t%s\n' "$MIN_COMMITTED_TPS"
    printf 'configuration_source\t%s\n' 'launcher environment; not node attestation'
    printf 'qualification_scope\t%s\n' 'h2v4_commit_qcs_qmdb_only_native_headers_and_receipts'
    printf 'wave_transactions\t%s\n' "$WAVE"
    printf 'nodes\t%s\n' "$NODES"
    printf 'chain_id\t%s\n' "${N42_CHAIN_ID:-4242}"
    printf 'max_transactions_per_block\t%s\n' "${N42_MAX_TXS_PER_BLOCK:-48000}"
    printf 'skip_transaction_verification\t%s\n' "${N42_SKIP_TX_VERIFY:-0}"
    printf 'fast_transfer_configured\t%s\n' "${N42_FAST_TRANSFER:-0}"
    printf 'parallel_build_configured\t%s\n' "${N42_PARALLEL_BUILD:-0}"
    printf 'parallel_import_configured\t%s\n' "${N42_PARALLEL_IMPORT:-0}"
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
python3 "$SCRIPT_DIR/h2-tps-audit.py" capture --trusted-config "$TRUSTED_CONFIG" --rpc "$rpc_csv" \
    --out "$ARTIFACT_DIR/h2-start.json"

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
deadline=$((SECONDS + 120))
while [[ ! -s "$start_marker" ]]; do
    if ! kill -0 "$stress_pid" 2>/dev/null || (( SECONDS >= deadline )); then
        echo "Stress process failed to begin ingest after the start gate" >&2
        exit 1
    fi
    sleep 0.01
done

deadline=$((SECONDS + DURATION + 120))
while [[ ! -s "$end_marker" ]]; do
    if ! kill -0 "$stress_pid" 2>/dev/null; then
        wait "$stress_pid"
        echo "Stress process exited before the ingest end marker" >&2
        exit 1
    fi
    if (( SECONDS >= deadline )); then
        echo "Timed out waiting for stress ingest end marker" >&2
        exit 1
    fi
    sleep 0.02
done
python3 "$SCRIPT_DIR/h2-tps-audit.py" capture --trusted-config "$TRUSTED_CONFIG" --rpc "$rpc_csv" \
    --out "$ARTIFACT_DIR/h2-end.json"

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

audit_status=0
python3 "$SCRIPT_DIR/h2-tps-audit.py" audit --trusted-config "$TRUSTED_CONFIG" \
    --start "$ARTIFACT_DIR/h2-start.json" --end "$ARTIFACT_DIR/h2-end.json" \
    --out "$ARTIFACT_DIR/h2-audit.json" --min-tps "$MIN_COMMITTED_TPS" || audit_status=$?

python3 - "$ARTIFACT_DIR" <<'PY_SUMMARY'
import glob
import json
import re
import sys
from pathlib import Path

out = Path(sys.argv[1])
audit = json.loads((out / "h2-audit.json").read_text())
if audit.get("status") not in ("passed", "below_target"):
    raise SystemExit(f"H2 audit failed: {audit.get('error', 'no result')}")
start_block, end_block = audit["start_block"], audit["end_block"]
committed_txs = audit["committed_transactions"]
elapsed = audit["measurement_seconds"]

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
    ("throughput_status", audit["status"]),
    ("throughput_scope", audit["scope"]),
    ("validators", audit["nodes"]),
    ("start_block", start_block),
    ("end_block", end_block),
    ("committed_blocks", end_block - start_block),
    ("committed_transactions", committed_txs),
    ("successful_committed_transactions", audit["successful_committed_transactions"]),
    ("failed_committed_transactions", audit["failed_committed_transactions"]),
    ("measurement_seconds", f"{elapsed:.6f}"),
    ("strict_committed_tps", f"{committed_txs / elapsed:.2f}"),
    ("successful_committed_tps", f"{audit['successful_committed_tps']:.2f}"),
    ("keccak_binary_sha256", audit["keccak_binary_sha256"]),
    ("commit_verifier_sha256", audit["commit_verifier_sha256"]),
    ("trusted_config_sha256", audit["trusted_config_sha256"]),
]
for index, evidence in enumerate(audit["read_evidence"]):
    prefix = f"validator_{index}"
    for field in ("url", "instance_id", "validator_public_key"):
        rows.append((f"{prefix}_{field}", evidence[field]))
    for field, delta in evidence["counters_delta"].items():
        rows.append((f"{prefix}_qmdb_{field}_delta", delta))
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
PY_SUMMARY

trap - EXIT INT TERM
echo "Measurement complete (${audit_status}=exit status): $ARTIFACT_DIR"
column -t -s $'\t' "$ARTIFACT_DIR/summary.tsv" 2>/dev/null \
    || cat "$ARTIFACT_DIR/summary.tsv"

exit "$audit_status"
