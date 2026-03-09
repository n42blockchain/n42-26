#!/bin/bash
# System monitor: CPU, Network, Disk IO per second
# Usage: ./scripts/sysmon.sh [output_file] [duration_seconds]
# Requires macOS (uses vm_stat, netstat, iostat)

OUTPUT="${1:-/tmp/n42-sysmon.csv}"
DURATION="${2:-600}"

echo "timestamp,cpu_user%,cpu_sys%,cpu_idle%,disk_read_KB/s,disk_write_KB/s,net_in_KB/s,net_out_KB/s,reth_rss_MB,n42_node_cpu%" > "$OUTPUT"

# Get initial network counters
get_net_bytes() {
    # macOS: use netstat -ib for interface bytes
    netstat -ib 2>/dev/null | awk '/^en0/ && !/\*/ {print $7, $10; exit}'
}

prev_net=($(get_net_bytes))
prev_time=$(date +%s)

for i in $(seq 1 "$DURATION"); do
    sleep 1

    ts=$(date +%H:%M:%S)

    # CPU usage via top (1 sample, 0 delay)
    cpu_line=$(top -l 1 -n 0 2>/dev/null | grep "CPU usage")
    cpu_user=$(echo "$cpu_line" | sed 's/.*: \([0-9.]*\)% user.*/\1/')
    cpu_sys=$(echo "$cpu_line" | sed 's/.*user, \([0-9.]*\)% sys.*/\1/')
    cpu_idle=$(echo "$cpu_line" | sed 's/.*sys, \([0-9.]*\)% idle.*/\1/')

    # Disk IO via iostat (1 sample)
    io_line=$(iostat -d -c 1 2>/dev/null | tail -1)
    disk_read=$(echo "$io_line" | awk '{print $3}')   # KB/t × transfers
    disk_write=$(echo "$io_line" | awk '{print $4}')

    # Network bytes
    cur_net=($(get_net_bytes))
    cur_time=$(date +%s)
    dt=$((cur_time - prev_time))
    if [ "$dt" -gt 0 ] && [ "${#cur_net[@]}" -eq 2 ] && [ "${#prev_net[@]}" -eq 2 ]; then
        net_in_kb=$(( (cur_net[0] - prev_net[0]) / dt / 1024 ))
        net_out_kb=$(( (cur_net[1] - prev_net[1]) / dt / 1024 ))
    else
        net_in_kb=0
        net_out_kb=0
    fi
    prev_net=("${cur_net[@]}")
    prev_time=$cur_time

    # Process-level: reth RSS and CPU
    reth_info=$(ps -eo rss,%cpu,comm 2>/dev/null | grep n42-node | head -1)
    reth_rss_mb=0
    reth_cpu=0
    if [ -n "$reth_info" ]; then
        reth_rss_mb=$(echo "$reth_info" | awk '{printf "%.0f", $1/1024}')
        reth_cpu=$(echo "$reth_info" | awk '{print $2}')
    fi

    echo "${ts},${cpu_user},${cpu_sys},${cpu_idle},${disk_read},${disk_write},${net_in_kb},${net_out_kb},${reth_rss_mb},${reth_cpu}" >> "$OUTPUT"

    # Print every 10s
    if [ $((i % 10)) -eq 0 ]; then
        echo "[sysmon] ${ts} cpu=${cpu_user}+${cpu_sys}% disk_r=${disk_read} disk_w=${disk_write} net_in=${net_in_kb}KB/s net_out=${net_out_kb}KB/s reth_rss=${reth_rss_mb}MB reth_cpu=${reth_cpu}%"
    fi
done

echo "[sysmon] Done. Output: $OUTPUT"
