#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME LABEL START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
label="${2:?usage: $0 RUNTIME LABEL START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
start="${3:?usage: $0 RUNTIME LABEL START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
auditor="${4:?usage: $0 RUNTIME LABEL START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
expected_auditor_sha="${5:?usage: $0 RUNTIME LABEL START_HEIGHT AUDITOR EXPECTED_SHA [OUTPUT]}"
output="${6:-$runtime/evidence/runtime28-$label-six-producer-full-range.json}"
milestone="$runtime/evidence/gov5-906-$label-milestone.json"
failure="${N42_PRODUCER_WAITER_FAILURE:-${output%.json}-failure.json}"
preflight_only="${N42_PRODUCER_WAITER_PREFLIGHT_ONLY:-0}"

test -d "$runtime"
test -s "$auditor"
test "$(shasum -a 256 "$auditor" | awk '{print $1}')" = "$expected_auditor_sha"
test ! -e "$output"
test ! -e "${output%.json}-raw"
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
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json"; do
    test ! -s "$item"
  done
}

on_error() {
  local exit_code=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --argjson exit_code "$exit_code" --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_milestone_six_producer_waiter_failure",status:"FAIL",
      label:$label,exitCode:$exit_code,line:$line,command:$command}' >"$failure"
  exit "$exit_code"
}
trap on_error ERR

assert_nodes
assert_no_failures
if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --arg auditor "$auditor" --arg sha "$expected_auditor_sha" \
    --arg output "$output" --argjson milestone_present "$(test -s "$milestone" && echo true || echo false)" \
    '{at:$at,event:"gov5_milestone_six_producer_waiter_preflight",status:"PASS",
      label:$label,auditor:$auditor,auditorSha256:$sha,output:$output,
      milestonePresent:$milestone_present,nodesAlive:true,noFailureEvidence:true,
      mutationPerformed:false}'
  exit 0
fi

while ! test -s "$milestone"; do
  assert_nodes
  assert_no_failures
  sleep 30
done
jq -e --arg label "$label" '
  .status=="PASS" and .label==$label and .acceptanceRelaxed==false and
  .transactionsSent==0 and .failureEvidencePresent==false
' "$milestone" >/dev/null

milestone_end="$(jq -er '.soak.endHeight' "$milestone")"
test "$milestone_end" -ge "$start"
complete_cycles=$(((milestone_end - start + 1) / 6))
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
cat "$output"
