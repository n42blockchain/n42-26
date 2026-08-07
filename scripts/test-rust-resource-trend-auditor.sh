#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
auditor="$repo/scripts/audit-rust-resource-trend.sh"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

fixture="$work_dir/resources.jsonl"
output="$work_dir/pass.json"

for spec in \
  '2026-08-03T00:00:00Z 100 200000 161 93 10000 5000 1000 100' \
  '2026-08-03T00:05:00Z 110 190000 162 95 10100 5100 1100 110' \
  '2026-08-03T00:10:00Z 120 180000 162 97 10200 5200 1200 120'; do
  read -r at head rss threads fds reth consensus log wal <<<"$spec"
  jq -nc --arg at "$at" --argjson head "$head" --argjson rss "$rss" \
    --argjson threads "$threads" --argjson fds "$fds" --argjson reth "$reth" \
    --argjson consensus "$consensus" --argjson log "$log" --argjson wal "$wal" \
    '{at:$at,event:"rust_resource_snapshot",pid:42,processElapsed:"00:00",
      head:$head,rssKiB:$rss,vszKiB:1000000,cpuPercent:0,
      threads:$threads,fileDescriptors:$fds,rethDataKiB:$reth,
      consensusDataKiB:$consensus,logBytes:$log,
      qmdbWalFile:"/tmp/test.wal",qmdbWalBytes:$wal}' >>"$fixture"
done

"$auditor" "$fixture" "$output" 600 3600 1048576 >/dev/null
jq -e '.status=="PASS" and .fileDescriptors.first==93 and
  .fileDescriptors.last==97 and .fileDescriptors.finalGrowth==4 and
  .fileDescriptors.finalGrowthLimit==4 and
  .fileDescriptors.finalWithinGrowthLimit and
  .fileDescriptors.max<=.fileDescriptors.limit' "$output" >/dev/null

jq -c 'if input_line_number==3 then .fileDescriptors=98 else . end' \
  "$fixture" >"$work_dir/fd-growth-five.jsonl"
if "$auditor" "$work_dir/fd-growth-five.jsonl" \
  "$work_dir/fd-growth-five.json" 600 3600 1048576 >/dev/null 2>&1; then
  echo 'resource trend auditor accepted final FD growth above four' >&2
  exit 1
fi

jq -c 'if input_line_number==2 then .fileDescriptors=257 else . end' \
  "$fixture" >"$work_dir/fd-absolute-limit.jsonl"
if "$auditor" "$work_dir/fd-absolute-limit.jsonl" \
  "$work_dir/fd-absolute-limit.json" 600 3600 1048576 >/dev/null 2>&1; then
  echo 'resource trend auditor accepted FD count above absolute limit' >&2
  exit 1
fi

jq -nc --arg at "$(date -u +%FT%TZ)" \
  '{at:$at,event:"rust_resource_trend_auditor_regression_test",status:"PASS",
    boundedTransientFdGrowthAccepted:true,excessFinalFdGrowthRejected:true,
    absoluteFdLimitEnforced:true}'
