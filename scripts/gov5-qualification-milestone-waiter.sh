#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_MILESTONE_RUNTIME:?runtime is required}"
gov_repo="${N42_MILESTONE_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_MILESTONE_GOV_MAIN:?Gov5 main is required}"
label="${1:?milestone label is required}"
minimum="${2:?minimum elapsed seconds is required}"
[[ "$label" =~ ^[a-z0-9-]+$ ]]
[[ "$minimum" =~ ^[0-9]+$ ]]

harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
heads="$runtime/evidence/mixed-soak-24h.jsonl"
resources="$runtime/evidence/rust-resource-24h.jsonl"
upstream="$runtime/evidence/gov5-upstream-24h.jsonl"
prefix="$runtime/evidence/gov5-906-$label"
head_snapshot="$prefix-heads.snapshot.jsonl"
resource_snapshot="$prefix-resources.snapshot.jsonl"
upstream_snapshot="$prefix-upstream.snapshot.jsonl"
soak_audit="$prefix-soak-audit.json"
resource_audit="$prefix-resource-audit.json"
upstream_audit="$prefix-upstream-audit.json"
summary="$prefix-milestone.json"
failure="$prefix-milestone-failure.json"

for path in "$head_snapshot" "$resource_snapshot" "$upstream_snapshot" \
  "$soak_audit" "$resource_audit" "$upstream_audit" "$summary" "$failure"; do
  test ! -e "$path"
done

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --argjson status "$status" --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_qualification_milestone_failure",status:"FAIL",
      label:$label,statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

elapsed() {
  local file="$1"
  if ! test -s "$file"; then
    printf '0\n'
    return
  fi
  jq -sr 'if length < 2 then 0 else
    ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)) end' "$file"
}

remote_main_with_retry() {
  local attempt remote=""
  local attempts="${N42_GOV_REMOTE_RETRY_ATTEMPTS:-6}"
  local delay="${N42_GOV_REMOTE_RETRY_DELAY_SECONDS:-10}"
  [[ "$attempts" =~ ^[1-9][0-9]*$ ]]
  [[ "$delay" =~ ^[0-9]+$ ]]
  for ((attempt=1; attempt<=attempts; attempt++)); do
    if remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main 2>/dev/null |
      awk 'NR==1 {print $1}')" && [[ "$remote" =~ ^[0-9a-f]{40}$ ]]; then
      printf '%s\n' "$remote"
      return 0
    fi
    if test "$attempt" -lt "$attempts"; then
      sleep "$delay"
    fi
  done
  return 1
}

while :; do
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-qualification-controller-guardian-failures.jsonl" \
    "$runtime/evidence/runtime22-monitor-pid-guardian-failures.jsonl"; do
    test ! -s "$path"
  done
  remote="$(remote_main_with_retry)"
  test "$remote" = "$expected_gov_main"
  if test -s "$heads"; then
    tail -n 1 "$heads" | jq -e '.ok==true and .zeroTxRequired==1' >/dev/null
  fi
  head_elapsed="$(elapsed "$heads")"
  resource_elapsed="$(elapsed "$resources")"
  upstream_elapsed="$(elapsed "$upstream")"
  if test "$head_elapsed" -ge "$minimum" &&
    test "$resource_elapsed" -ge "$minimum" &&
    test "$upstream_elapsed" -ge "$minimum"; then
    break
  fi
  sleep 60
done

cp "$heads" "$head_snapshot"
cp "$resources" "$resource_snapshot"
cp "$upstream" "$upstream_snapshot"

N42_QUAL_RUNTIME="$runtime" "$harness" audit-soak \
  "$head_snapshot" "$minimum" 120 6 1 >"$soak_audit"
N42_QUAL_RUNTIME="$runtime" "$harness" audit-rust-resources \
  "$resource_snapshot" "$minimum" "$resource_audit" >/dev/null

jq -e -s --arg expected "$expected_gov_main" --argjson minimum "$minimum" '
  length>=2 and all(.[];.remoteReachable==true and .baselineExact==true and
    .baseline==$expected and .remoteMain==$expected) and
  ([.[].at|fromdateiso8601] as $times |
    ($times[-1]-$times[0]) >= $minimum and
    ([range(1;$times|length) as $i |
      ($times[$i]-$times[$i-1])>0 and
      ($times[$i]-$times[$i-1])<=700] | all))' "$upstream_snapshot" >/dev/null
jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_gov_main" \
  --argjson minimum "$minimum" --slurpfile samples "$upstream_snapshot" '
  [$samples[].at|fromdateiso8601] as $times |
  {at:$at,event:"gov5_upstream_milestone_audit",status:"PASS",
    expectedMain:$expected,samples:($samples|length),firstAt:$samples[0].at,
    lastAt:$samples[-1].at,elapsedSeconds:($times[-1]-$times[0]),
    minimumElapsedSeconds:$minimum,
    maximumSampleGapSeconds:([range(1;$times|length) as $i |
      $times[$i]-$times[$i-1]]|max),allSnapshotsReachableAndExact:true}' \
  >"$upstream_audit"

consensus="$(curl -fsS --max-time 5 -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"n42_consensusStatus","params":[]}' \
  http://127.0.0.1:29545 | jq -ec '.result')"
equivocations="$(curl -fsS --max-time 5 -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"n42_equivocations","params":[]}' \
  http://127.0.0.1:29545 | jq -ec '.result')"
jq -e '.validatorCount==7 and .hasCommittedQc==true' <<<"$consensus" >/dev/null
jq -e '.total==0 and (.evidence|length)==0' <<<"$equivocations" >/dev/null
leader_count="$(rg -c 'block committed! view=.*votes=5\+5' "$runtime/logs/rust.log")"
test "$leader_count" -ge 2

jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
  --argjson minimum "$minimum" --argjson leaders "$leader_count" \
  --argjson consensus "$consensus" --argjson equivocations "$equivocations" \
  --slurpfile soak "$soak_audit" --slurpfile resource "$resource_audit" \
  --slurpfile upstream_audit "$upstream_audit" \
  '{at:$at,event:"gov5_qualification_milestone",status:"PASS",label:$label,
    acceptanceRelaxed:false,minimumElapsedSeconds:$minimum,soak:$soak[0],
    resources:$resource[0],gov5Upstream:$upstream_audit[0],
    rustLeaderCommitsFivePlusFive:$leaders,consensus:$consensus,
    equivocations:$equivocations,transactionsSent:0,
    failureEvidencePresent:false}' >"$summary"
jq -e --argjson minimum "$minimum" '.status=="PASS" and
  .soak.elapsedSeconds >= $minimum and .resources.elapsedSeconds >= $minimum and
  .gov5Upstream.elapsedSeconds >= $minimum and
  .rustLeaderCommitsFivePlusFive >= 2 and .equivocations.total==0 and
  .transactionsSent==0 and .failureEvidencePresent==false' "$summary" >/dev/null
cat "$summary"
