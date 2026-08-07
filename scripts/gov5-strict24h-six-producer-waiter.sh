#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
start="${2:?usage: $0 RUNTIME START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
auditor="${3:?usage: $0 RUNTIME START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
expected_auditor_sha="${4:?usage: $0 RUNTIME START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
output="${5:-$runtime/evidence/runtime28-strict24h-six-producer-full-range.json}"
soak="$runtime/evidence/mixed-soak-24h-audit.json"
linkage="${N42_STRICT24H_PRODUCER_LINKAGE:-${output%.json}-linkage.json}"
failure="${N42_STRICT24H_PRODUCER_FAILURE:-${output%.json}-failure.json}"
preflight_only="${N42_STRICT24H_PRODUCER_PREFLIGHT_ONLY:-0}"

test -d "$runtime"
test -s "$auditor"
test "$(shasum -a 256 "$auditor" | awk '{print $1}')" = "$expected_auditor_sha"
test ! -e "$output"
test ! -e "${output%.json}-raw"
test ! -e "$linkage"
test ! -e "$failure"
[[ "$start" =~ ^[0-9]+$ ]]

assert_nodes() {
  local pid_file
  for pid_file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$pid_file"
    kill -0 "$(<"$pid_file")"
  done
}

assert_no_failures() {
  local item
  for item in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json"; do
    test ! -s "$item"
  done
}

on_error() {
  local exit_code=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson exit_code "$exit_code" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_strict24h_six_producer_waiter_failure",status:"FAIL",
      exitCode:$exit_code,line:$line,command:$command}' >"$failure"
  exit "$exit_code"
}
trap on_error ERR

assert_nodes
assert_no_failures
if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg auditor "$auditor" \
    --arg sha "$expected_auditor_sha" --arg output "$output" \
    --argjson soak_present "$(test -s "$soak" && echo true || echo false)" \
    '{at:$at,event:"gov5_strict24h_six_producer_waiter_preflight",status:"PASS",
      auditor:$auditor,auditorSha256:$sha,output:$output,soakAuditPresent:$soak_present,
      nodesAlive:true,noFailureEvidence:true,mutationPerformed:false}'
  exit 0
fi

while ! test -s "$soak"; do
  assert_nodes
  assert_no_failures
  sleep 30
done
jq -e '
  .status=="PASS" and .elapsedSeconds>=86400 and
  .zeroTransactionRequired==true and .maximumLag<=6 and
  .maximumSampleGapSeconds<=120
' "$soak" >/dev/null

soak_end="$(jq -er '.endHeight' "$soak")"
test "$soak_end" -ge "$start"
complete_cycles=$(((soak_end - start + 1) / 6))
test "$complete_cycles" -gt 0
closed_end=$((start + complete_cycles * 6 - 1))
"$auditor" "$runtime" "$start" "$output" "$closed_end" >/dev/null
jq -e --argjson start "$start" --argjson end "$closed_end" \
  --argjson cycles "$complete_cycles" '
  .status=="PASS" and .startHeight==$start and .endHeight==$end and
  .completeCycles==$cycles and .allSixEndpointSequencesExact and
  .parentChainContinuous and .expectedProducerSlotsExact and
  .allProducerCountsBalanced and .zeroTransactions and (.mutationPerformed|not)
' "$output" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg soak "$soak" --arg soak_sha "$(shasum -a 256 "$soak" | awk '{print $1}')" \
  --arg producer "$output" --arg producer_sha "$(shasum -a 256 "$output" | awk '{print $1}')" \
  --argjson soak_end "$soak_end" --argjson closed_end "$closed_end" \
  '{at:$at,event:"gov5_strict24h_six_producer_linkage",status:"PASS",
    soakAudit:$soak,soakAuditSha256:$soak_sha,producerAudit:$producer,
    producerAuditSha256:$producer_sha,soakEndHeight:$soak_end,
    closedFullCycleEndHeight:$closed_end,historicalWindowOnly:true,
    postSoakTransactionsCannotAlterAuditedHistory:true,mutationPerformed:false}' \
  >"$linkage.pending"
mv "$linkage.pending" "$linkage"
cat "$output"
