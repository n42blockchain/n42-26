#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
harness="$repo/scripts/gov5-interop-qualification.sh"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

base="$work_dir/base.jsonl"
summary="$work_dir/summary.json"

jq -nc \
  --arg at "2026-08-03T00:00:00Z" \
  '{at:$at,event:"rust_resource_snapshot",pid:42,processElapsed:"00:01:00",
    head:100,rssKiB:200000,vszKiB:1000000,cpuPercent:0.1,threads:100,
    fileDescriptors:50,rethDataKiB:10000,consensusDataKiB:5000,
    logBytes:1000,qmdbWalFile:"/tmp/test.wal",qmdbWalBytes:100}' >"$base"
jq -nc \
  --arg at "2026-08-03T00:05:00Z" \
  '{at:$at,event:"rust_resource_snapshot",pid:42,processElapsed:"00:06:00",
    head:110,rssKiB:201000,vszKiB:1001000,cpuPercent:0.2,threads:101,
    fileDescriptors:51,rethDataKiB:10100,consensusDataKiB:4000,
    logBytes:1100,qmdbWalFile:"/tmp/test.wal",qmdbWalBytes:110}' >>"$base"
jq -nc \
  --arg at "2026-08-03T00:10:00Z" \
  '{at:$at,event:"rust_resource_snapshot",pid:42,processElapsed:"00:11:00",
    head:120,rssKiB:202000,vszKiB:1002000,cpuPercent:0.3,threads:102,
    fileDescriptors:52,rethDataKiB:9900,consensusDataKiB:4500,
    logBytes:1200,qmdbWalFile:"/tmp/test.wal",qmdbWalBytes:120}' >>"$base"
jq -nc \
  --arg at "2026-08-03T00:15:00Z" \
  '{at:$at,event:"rust_resource_snapshot",pid:42,processElapsed:"00:16:00",
    head:130,rssKiB:203000,vszKiB:1003000,cpuPercent:0.4,threads:103,
    fileDescriptors:53,rethDataKiB:10200,consensusDataKiB:3500,
    logBytes:1300,qmdbWalFile:"/tmp/test.wal",qmdbWalBytes:130}' >>"$base"

N42_QUAL_RUNTIME="$work_dir" "$harness" audit-rust-resources \
  "$base" 900 "$summary" >/dev/null
jq -e '
  .status == "PASS" and .samples == 4 and .pid == 42 and
  .elapsedSeconds == 900 and .headGrowth == 30 and
  .growth.rethDataKiB == 200 and .growth.consensusDataKiB == -1500 and
  .allocatedStorageStepDecreaseKiB.maximumObserved == 1000 and
  .allocatedStorageStepDecreaseKiB.rethMaximum == 200 and
  .allocatedStorageStepDecreaseKiB.consensusMaximum == 1000 and
  .singleProcess == true and .logicalCountersMonotonic == true and
  .allocatedStorageMeasurementsNonnegative == true and
  .allocatedStorageMayDecreaseDuringCompaction == true and
  .headLogAndWalCountersMonotonic == true
' "$summary" >/dev/null

expect_reject() {
  local name="${1:?case name required}"
  local fixture="${2:?fixture required}"
  if N42_QUAL_RUNTIME="$work_dir" "$harness" audit-rust-resources \
    "$fixture" 900 >/dev/null 2>&1; then
    echo "resource auditor unexpectedly accepted $name" >&2
    return 1
  fi
}

jq -c 'if input_line_number == 3 then .logBytes = 1099 else . end' \
  "$base" >"$work_dir/log-decrease.jsonl"
expect_reject "logical log-byte decrease" "$work_dir/log-decrease.jsonl"

jq -c 'if input_line_number == 3 then .qmdbWalBytes = 109 else . end' \
  "$base" >"$work_dir/wal-decrease.jsonl"
expect_reject "QMDB WAL decrease" "$work_dir/wal-decrease.jsonl"

jq -c 'if input_line_number == 3 then .head = 109 else . end' \
  "$base" >"$work_dir/head-decrease.jsonl"
expect_reject "head decrease" "$work_dir/head-decrease.jsonl"

jq -c 'if input_line_number == 4 then .pid = 43 else . end' \
  "$base" >"$work_dir/pid-change.jsonl"
expect_reject "PID change" "$work_dir/pid-change.jsonl"

jq -c 'if input_line_number == 2 then .consensusDataKiB = 0 else . end' \
  "$base" >"$work_dir/nonpositive-allocation.jsonl"
expect_reject "nonpositive allocated storage" \
  "$work_dir/nonpositive-allocation.jsonl"

jq -c 'if input_line_number == 2 then .at = "2026-08-03T00:06:01Z" else . end' \
  "$base" >"$work_dir/sample-gap.jsonl"
expect_reject "sample gap above 360 seconds" "$work_dir/sample-gap.jsonl"

jq -nc --arg at "$(date -u +%FT%TZ)" \
  '{at:$at,event:"gov5_resource_auditor_regression_test",status:"PASS",
    allocatedStorageCompactionAccepted:true,
    logicalLogDecreaseRejected:true,qmdbWalDecreaseRejected:true,
    headDecreaseRejected:true,pidChangeRejected:true,
    nonpositiveAllocationRejected:true,oversizedSampleGapRejected:true}'
