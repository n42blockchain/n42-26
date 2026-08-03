#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME PRIMARY_REPO GOV_REPO COMBO_REPO RETH_REPO DEPS_REPO BASE_AUDITOR BASE_SHA V2_AUDITOR V2_SHA RESOURCE_AUDITOR RESOURCE_SHA EXPECTED_GOV_MAIN}"
primary_repo="${2:?primary repository is required}"
gov_repo="${3:?Gov5 repository is required}"
combo_repo="${4:?combination repository is required}"
reth_repo="${5:?Reth repository is required}"
deps_repo="${6:?dependency repository is required}"
base_auditor="${7:?base completion auditor is required}"
expected_base_sha="${8:?base auditor SHA-256 is required}"
v2_auditor="${9:?V2 completion auditor is required}"
expected_v2_sha="${10:?V2 auditor SHA-256 is required}"
resource_auditor="${11:?resource trend auditor is required}"
expected_resource_sha="${12:?resource auditor SHA-256 is required}"
expected_gov_main="${13:?expected Gov5 main commit is required}"
preflight_only="${N42_FINAL_COMPLETION_PREFLIGHT_ONLY:-0}"

final_905="$runtime/evidence/runtime28-final-905-data-compatibility-audit.json"
base_output="$runtime/evidence/gov5-906-goal-completion-audit.json"
v2_output="$runtime/evidence/gov5-906-goal-completion-audit-v2.json"
failure="$runtime/evidence/gov5-906-final-goal-completion-waiter-failure.json"

test -d "$runtime"
for repo in "$primary_repo" "$gov_repo" "$combo_repo" "$reth_repo" \
  "$deps_repo"; do
  git -C "$repo" rev-parse --git-dir >/dev/null
done
for auditor in "$base_auditor" "$v2_auditor" "$resource_auditor"; do
  test -x "$auditor"
done
test ! -e "$failure"
[[ "$expected_base_sha" =~ ^[0-9a-f]{64}$ ]]
[[ "$expected_v2_sha" =~ ^[0-9a-f]{64}$ ]]
[[ "$expected_resource_sha" =~ ^[0-9a-f]{64}$ ]]
[[ "$expected_gov_main" =~ ^[0-9a-f]{40}$ ]]

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

assert_state() {
  local item remote
  test "$(sha256 "$base_auditor")" = "$expected_base_sha"
  test "$(sha256 "$v2_auditor")" = "$expected_v2_sha"
  test "$(sha256 "$resource_auditor")" = "$expected_resource_sha"
  for item in "$runtime"/pids/gov{1,2,3,4,5}.pid \
    "$runtime/pids/rust.pid"; do
    test -s "$item"
    kill -0 "$(<"$item")"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | \
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected_gov_main"
  for item in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/official-reth-stable/latest-reth-failures.jsonl" \
    "$runtime/evidence/official-reth-stable/latest-reth-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-copied-boundary-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-final-network-verification-failure.json" \
    "$runtime/evidence/runtime28-strict24h-six-producer-full-range-failure.json" \
    "$runtime/evidence/runtime28-final-905-data-compatibility-audit-failure.json" \
    "$runtime/evidence/gov5-906-goal-completion-audit-failure.json" \
    "$runtime/evidence/gov5-906-goal-completion-audit-v2-failure.json"; do
    test ! -s "$item"
  done
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_906_final_goal_completion_waiter_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

run_base() {
  env \
    N42_COMPLETION_RUNTIME="$runtime" \
    N42_COMPLETION_PRIMARY_REPO="$primary_repo" \
    N42_COMPLETION_GOV_REPO="$gov_repo" \
    N42_COMPLETION_COMBO_REPO="$combo_repo" \
    N42_COMPLETION_RETH_REPO="$reth_repo" \
    N42_COMPLETION_DEPS_REPO="$deps_repo" \
    N42_COMPLETION_EXPECTED_SELF_SHA="$expected_base_sha" \
    "$base_auditor"
}

run_v2() {
  "$v2_auditor" "$runtime" "$primary_repo" "$resource_auditor" \
    "$expected_resource_sha"
}

assert_state
if test "$preflight_only" = 1; then
  base_preflight="$(N42_COMPLETION_PREFLIGHT_ONLY=1 run_base)"
  v2_preflight="$(N42_COMPLETION_V2_PREFLIGHT_ONLY=1 run_v2)"
  jq -e '.status=="PASS" and .completionNotClaimed and
    (.mutationPerformed|not)' <<<"$base_preflight" >/dev/null
  jq -e '.status=="PASS" and .completionNotClaimed and
    (.mutationPerformed|not)' <<<"$v2_preflight" >/dev/null
  jq -nc --arg at "$(date -u +%FT%TZ)" \
    --arg base "$expected_base_sha" --arg v2 "$expected_v2_sha" \
    --arg resource "$expected_resource_sha" \
    --argjson final_905_present "$(test -s "$final_905" && echo true || echo false)" \
    '{at:$at,event:"gov5_906_final_goal_completion_waiter_preflight",
      status:"PASS",baseAuditorSha256:$base,v2AuditorSha256:$v2,
      resourceAuditorSha256:$resource,final905EvidencePresent:$final_905_present,
      nodesAlive:true,noFailureEvidence:true,completionNotClaimed:true,
      mutationPerformed:false}'
  exit 0
fi

while ! test -s "$final_905"; do
  assert_state
  sleep 60
done

jq -e '.status=="PASS" and .latestAndPendingNonce=="0x22" and
  .allFiveProcessesAlive and .allFiveChaindataPresent and
  .allFiveTxindexRangesAbsent and .genesisAndCopiedHeadSixEndpointExact and
  .liveSixEndpointIdentityExact and
  .dataRecopyOrRegenerationRequired==false' "$final_905" >/dev/null
assert_state

if ! test -s "$base_output"; then
  run_base >/dev/null
fi
jq -e '.status=="PASS" and .objectiveRequirementsIndependentlyClosed and
  .latestAndPendingNonce=="0x22" and .noFailureEvidence' \
  "$base_output" >/dev/null

if ! test -s "$v2_output"; then
  run_v2 >/dev/null
fi
jq -e '.status=="PASS" and .objectiveRequirementsExtendedClosure and
  .latestAndPendingNonce=="0x22" and .noFailureEvidence' \
  "$v2_output" >/dev/null
cat "$v2_output"
