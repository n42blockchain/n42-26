#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME LABEL FIRST_RUST_HEIGHT GOV_REPO EXPECTED_MAIN STATIC_BASELINE}"
label="${2:?milestone label is required}"
first_rust_height="${3:?first Rust-authored height is required}"
gov_repo="${4:?Gov5 repository is required}"
expected_main="${5:?expected Gov5 main is required}"
static_baseline="${6:?static-boundary baseline is required}"
preflight_only="${N42_DEEP_PREFLIGHT_ONLY:-0}"

[[ "$label" =~ ^[a-z0-9-]+$ ]]
[[ "$first_rust_height" =~ ^[0-9]+$ ]]
test -d "$runtime"
git -C "$gov_repo" rev-parse --git-dir >/dev/null
test -s "$static_baseline"

harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
rechecker="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/recheck-gov5-runtime-static-boundary.sh"
milestone="$runtime/evidence/gov5-906-$label-milestone.json"
prefix="$runtime/evidence/runtime28-$label-closed"
snapshot="$prefix-log-snapshot"
leader="$prefix-leader-audit.jsonl"
timeout="$prefix-timeout-audit.jsonl"
runtime_log="$prefix-runtime-log-audit.jsonl"
static="$prefix-static-data-boundary-recheck.json"
output="$prefix-deep-audit.json"
failure="$prefix-deep-audit-failure.json"
rust_miner=0x81d4c1f92ddb837cb46f82280d9b491b101fa582
ports=(28501 28502 28503 28504 28505 29545)

for required in "$harness" "$rechecker" "$static_baseline"; do
  test -s "$required"
done
for path in "$snapshot" "$leader" "$timeout" "$runtime_log" "$static" \
  "$output" "$failure"; do
  test ! -e "$path"
done

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

check_wait_state() {
  local file remote
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
  for file in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json" \
    "$runtime/evidence/gov5-qualification-controller-guardian-failures.jsonl" \
    "$runtime/evidence/runtime22-monitor-pid-guardian-failures.jsonl"; do
    test ! -s "$file"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}')"
  test "$remote" = "$expected_main"
  test "$(jq -er '.status' "$static_baseline")" = PASS
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --argjson status "$status" --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_runtime_milestone_deep_audit_failure",status:"FAIL",
      label:$label,statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

check_wait_state
if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --arg harness "$(sha256 "$harness")" --arg rechecker "$(sha256 "$rechecker")" \
    --arg baseline "$(sha256 "$static_baseline")" \
    '{at:$at,event:"gov5_runtime_milestone_deep_audit_preflight",status:"PASS",
      label:$label,mutationPerformed:false,harnessSha256:$harness,
      staticRecheckerSha256:$rechecker,staticBaselineSha256:$baseline}'
  exit 0
fi

while ! test -s "$milestone"; do
  check_wait_state
  sleep 60
done
jq -e --arg label "$label" \
  '.status=="PASS" and .label==$label and .transactionsSent==0 and
   .failureEvidencePresent==false and .soak.maximumLag<=1 and
   .soak.zeroTransactionRequired==true and .equivocations.total==0' \
  "$milestone" >/dev/null

# Freeze only after a canonical Rust-authored block so the preceding missing-
# validator timeout has its immediate 5+5 recovery in the immutable log.
minimum_head=-1
for port in "${ports[@]}"; do
  head_hex="$(rpc "$port" eth_blockNumber '[]' | jq -er '.result')"
  head=$((head_hex))
  if test "$minimum_head" -lt 0 || test "$head" -lt "$minimum_head"; then
    minimum_head="$head"
  fi
done
end_height=""
for ((height=minimum_head; height>=minimum_head-12 && height>=first_rust_height; height--)); do
  block="$(rpc 29545 eth_getBlockByNumber \
    "[\"$(printf '0x%x' "$height")\",false]" | jq -ec '.result')"
  if test "$(jq -r '.miner|ascii_downcase' <<<"$block")" = "$rust_miner"; then
    end_height="$height"
    break
  fi
done
test -n "$end_height"

mkdir -p "$snapshot/logs"
cp "$runtime"/logs/gov{1,2,3,4,5}.log "$runtime/logs/rust.log" "$snapshot/logs/"

N42_QUAL_RUNTIME="$runtime" N42_QUAL_RUST_LOG="$snapshot/logs/rust.log" \
  "$harness" audit-rust-leaders "$first_rust_height" "$end_height" "$leader" >/dev/null
N42_QUAL_RUNTIME="$runtime" \
  "$harness" audit-timeout-recovery "$snapshot/logs/rust.log" "$timeout" >/dev/null
N42_QUAL_RUNTIME="$runtime" \
  "$harness" audit-runtime-logs "$snapshot/logs/rust.log" "$runtime_log" >/dev/null

jq -e '.status=="PASS" and .startHeight=='"$first_rust_height"' and
  .endHeight=='"$end_height"' and .parentChainContinuous and
  .expectedLeaderSlotsExact and .allConfiguredEndpointsExact and
  .leaderCommitLog.allVotesFivePlusFive and .leaderCommitLog.viewStrideExact and
  .leaderCommitLog.hashOrderExact' "$leader" >/dev/null
jq -e '.status=="PASS" and .pendingTimeouts==0 and
  .timeoutAndPacemakerSetsExact and .everyCompletedTimeoutRecoveredAtNextView and
  .recoveredByRustVotesFivePlusFive' "$timeout" >/dev/null
jq -e '.status=="PASS" and .warningPartitionExact and
  .unexpectedWarnings==0 and .criticalSignals==0' "$runtime_log" >/dev/null
test -z "$(rg -il ' ERROR |(^|[^a-z])(panic|fatal|equivocat)' "$snapshot/logs" || true)"

"$rechecker" "$runtime" "$static_baseline" "$static" >/dev/null
jq -e '.status=="PASS" and .mutationPerformed==false and
  .staticGov5Data.filesChecked==24 and .staticGov5Data.allCurrentHashesMatchInitialCopy' \
  "$static" >/dev/null

temporary="$(mktemp "$runtime/evidence/.deep-audit.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
  --argjson start "$first_rust_height" --argjson end "$end_height" \
  --arg milestone_sha "$(sha256 "$milestone")" \
  --arg snapshot_sha "$(sha256 "$snapshot/logs/rust.log")" \
  --arg leader_sha "$(sha256 "$leader")" --arg timeout_sha "$(sha256 "$timeout")" \
  --arg runtime_log_sha "$(sha256 "$runtime_log")" --arg static_sha "$(sha256 "$static")" \
  --slurpfile leader "$leader" --slurpfile timeout "$timeout" \
  --slurpfile runtime_log "$runtime_log" --slurpfile static "$static" '
  {at:$at,event:"gov5_runtime_milestone_deep_audit",status:"PASS",label:$label,
   acceptanceRelaxed:false,mutationPerformed:false,startHeight:$start,endHeight:$end,
   milestoneSha256:$milestone_sha,frozenRustLogSha256:$snapshot_sha,
   evidenceSha256:{leader:$leader_sha,timeout:$timeout_sha,
     runtimeLog:$runtime_log_sha,staticBoundary:$static_sha},
   rustLeaders:$leader[0],timeoutRecovery:$timeout[0],runtimeLogs:$runtime_log[0],
   staticBoundary:$static[0],transactionsSent:0,failureEvidencePresent:false}' \
  >"$temporary"
mv "$temporary" "$output"
jq -e '.status=="PASS" and .transactionsSent==0 and
  .failureEvidencePresent==false and .timeoutRecovery.pendingTimeouts==0 and
  .rustLeaders.leaderCommitLog.allVotesFivePlusFive and
  .staticBoundary.status=="PASS"' "$output" >/dev/null
cat "$output"
