#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
harness="$repo/scripts/gov5-interop-qualification.sh"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
mkdir -p "$work_dir/logs"
for node in gov1 gov2 gov3 gov4 gov5; do
  : >"$work_dir/logs/$node.log"
done

hash='0x1111111111111111111111111111111111111111111111111111111111111111'
head_hash='0x2222222222222222222222222222222222222222222222222222222222222222'
warning="2026-08-18T06:16:48.912304Z  WARN pending_finalization stale (>2 views behind), clearing and reconciling execution state pending_view=10 new_view=13 hash=$hash"
covered="2026-08-18T06:16:48.912305Z  INFO discarded stale pending_finalization already covered by the execution-valid head stale_view=10 hash=$hash execution_validated_head_view=12 execution_validated_head=$head_hash"
redrive="2026-08-18T06:16:48.912305Z  INFO stale pending_finalization re-driven from the retained committed broadcast stale_view=10 new_view=13 hash=$hash"

run_audit() {
  env N42_QUAL_RUNTIME="$work_dir" N42_QUAL_REQUIRE_TIMEOUTS=0 \
    N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=0 \
    "$harness" audit-runtime-logs "$work_dir/logs/rust.log"
}

printf '%s\n%s\n' "$warning" "$covered" >"$work_dir/logs/rust.log"
covered_result="$(run_audit)"
jq -e '
  .status=="PASS" and .warningCounts.stalePendingFinalization==1 and
  .warningCounts.stalePendingAlreadyCovered==1 and
  .warningCounts.stalePendingLocalRedrive==0 and
  .stalePendingRecoveryPartitionExact==true
' <<<"$covered_result" >/dev/null

printf '%s\n%s\n' "$warning" "$redrive" >"$work_dir/logs/rust.log"
redrive_result="$(run_audit)"
jq -e '
  .status=="PASS" and .warningCounts.stalePendingFinalization==1 and
  .warningCounts.stalePendingAlreadyCovered==0 and
  .warningCounts.stalePendingLocalRedrive==1 and
  .stalePendingRecoveryPartitionExact==true
' <<<"$redrive_result" >/dev/null

printf '%s\n' "$warning" >"$work_dir/logs/rust.log"
if run_audit >/dev/null 2>&1; then
  echo 'unpaired stale pending_finalization warning unexpectedly passed' >&2
  exit 1
fi

# Gov wrapper logs do not carry ISO timestamps. A requested Rust log window
# must not turn the Gov scans into empty files and hide a critical signal.
printf '%s\n%s\n' "$warning" "$covered" >"$work_dir/logs/rust.log"
printf '%s\n' 'FATAL synthetic Gov wrapper failure' >"$work_dir/logs/gov1.log"
if env N42_QUAL_RUNTIME="$work_dir" N42_QUAL_LOG_START=2026-08-18T06:00:00 \
  N42_QUAL_REQUIRE_TIMEOUTS=0 N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=0 \
  "$harness" audit-runtime-logs "$work_dir/logs/rust.log" >/dev/null 2>&1; then
  echo 'non-timestamped Gov critical signal unexpectedly escaped window audit' >&2
  exit 1
fi
: >"$work_dir/logs/gov1.log"
window_result="$(env N42_QUAL_RUNTIME="$work_dir" \
  N42_QUAL_LOG_START=2026-08-18T06:00:00 N42_QUAL_REQUIRE_TIMEOUTS=0 \
  N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=0 \
  "$harness" audit-runtime-logs "$work_dir/logs/rust.log")"
jq -e '.status=="PASS" and .govLogsChecked==5 and .govLogScope=="full"' \
  <<<"$window_result" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" '
  {at:$at,event:"runtime_log_stale_pending_audit_regression",status:"PASS",
   coveredRecoveryAccepted:true,localRedriveAccepted:true,
   unpairedWarningRejected:true,
   nonTimestampedGovCriticalSignalRejectedUnderRustWindow:true,
   govWrapperLogsUseFullScope:true}
'
