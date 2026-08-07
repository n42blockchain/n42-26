#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME GOV_REPO EXPECTED_MAIN AUDITOR EXPECTED_AUDITOR_SHA}"
gov_repo="${2:?Gov5 repository is required}"
expected_main="${3:?expected Gov5 main commit is required}"
auditor="${4:?905 data compatibility auditor is required}"
expected_auditor_sha="${5:?auditor SHA-256 is required}"
preflight_only="${N42_FINAL_905_PREFLIGHT_ONLY:-0}"
total="$runtime/evidence/gov5-906-total-goal-final-verification.json"
output="$runtime/evidence/runtime28-final-905-data-compatibility-audit.json"
failure="$runtime/evidence/runtime28-final-905-data-compatibility-audit-failure.json"

test -d "$runtime"
git -C "$gov_repo" rev-parse --git-dir >/dev/null
test -x "$auditor"
test ! -e "$output"
test ! -e "$failure"
[[ "$expected_main" =~ ^[0-9a-f]{40}$ ]]
[[ "$expected_auditor_sha" =~ ^[0-9a-f]{64}$ ]]

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

assert_state() {
  local pid_file remote
  test "$(sha256 "$auditor")" = "$expected_auditor_sha"
  for pid_file in "$runtime"/pids/gov{1,2,3,4,5}.pid \
    "$runtime/pids/rust.pid"; do
    test -s "$pid_file"
    kill -0 "$(<"$pid_file")"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | \
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected_main"
  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/official-reth-stable/latest-reth-failures.jsonl" \
    "$runtime/evidence/official-reth-stable/latest-reth-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json"; do
    test ! -s "$path"
  done
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"runtime28_final_905_data_compatibility_audit_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

assert_state
if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg auditor "$expected_auditor_sha" \
    --arg expected_main "$expected_main" --arg output "$output" \
    --argjson total_present "$(test -s "$total" && echo true || echo false)" \
    '{at:$at,event:"runtime28_final_905_data_compatibility_waiter_preflight",
      status:"PASS",auditorSha256:$auditor,expectedMain:$expected_main,
      output:$output,totalGoalEvidencePresent:$total_present,nodesAlive:true,
      noFailureEvidence:true,mutationPerformed:false}'
  exit 0
fi

while ! test -s "$total"; do
  assert_state
  sleep 60
done

jq -e '.status=="PASS" and .latestRethExtraHourExact==true and
  .latestAndPendingNonce=="0x22" and .sourceAndRemotePinsExact==true and
  .noFailureEvidence==true' "$total" >/dev/null
assert_state
"$auditor" "$runtime" "$gov_repo" "$expected_main" "$output" 0x22 >/dev/null
jq -e '.status=="PASS" and .mutationPerformed==false and
  .latestAndPendingNonce=="0x22" and .allFiveProcessesAlive and
  .allFiveChaindataPresent and .allFiveTxindexRangesAbsent and
  .genesisAndCopiedHeadSixEndpointExact and .liveSixEndpointIdentityExact and
  .dataRecopyOrRegenerationRequired==false' "$output" >/dev/null
cat "$output"
