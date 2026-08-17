#!/usr/bin/env bash
set -Eeuo pipefail

runtime="${N42_MILESTONE_RUNTIME:?runtime is required}"
qualification="${N42_MILESTONE_QUALIFICATION_SCRIPT:?qualification script is required}"
head_pid="${N42_MILESTONE_HEAD_PID:?head monitor PID is required}"
upstream_pid="${N42_MILESTONE_UPSTREAM_PID:?upstream monitor PID is required}"
rust0_pid="${N42_MILESTONE_RUST0_PID:?Rust0 resource monitor PID is required}"
rust6_pid="${N42_MILESTONE_RUST6_PID:?Rust6 resource monitor PID is required}"
formal_failure="${N42_MILESTONE_FORMAL_FAILURE:?formal failure artifact is required}"
prefix="${N42_MILESTONE_OUTPUT_PREFIX:?output prefix is required}"
failure="${N42_MILESTONE_FAILURE:-${prefix}-failure.json}"
log_start="${N42_MILESTONE_LOG_START:?formal log start is required}"
milestones="${N42_MILESTONE_SECONDS:-3600 21600 43200 64800}"
heads="${N42_MILESTONE_HEADS:?head evidence is required}"
upstream="${N42_MILESTONE_UPSTREAM:?upstream evidence is required}"
rust0_resources="${N42_MILESTONE_RUST0_RESOURCES:?Rust0 resources are required}"
rust6_resources="${N42_MILESTONE_RUST6_RESOURCES:?Rust6 resources are required}"
ports='28501 28502 28503 28504 28505 29545 29546'
current_milestone=0
current_stage='startup'

record_failure() {
  local status="${1:?exit status is required}"
  local command="${2:-unknown}"
  local line="${3:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson exitStatus "$status" \
    --argjson milestone "$current_milestone" --arg stage "$current_stage" \
    --arg command "$command" --argjson line "$line" '
    {at:$at,event:"gov5_seven_validator_milestone_failure",status:"FAIL",
     exitStatus:$exitStatus,milestoneSeconds:$milestone,stage:$stage,command:$command,
     line:$line}
  ' >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit "$status"
}

trap 'record_failure "$?" "$BASH_COMMAND" "$LINENO"' ERR

test -x "$qualification"
test ! -e "$failure"

elapsed() {
  local evidence="${1:?evidence required}"
  if ! test -s "$evidence"; then printf '0\n'; return; fi
  jq -sr 'if length < 2 then 0 else
    ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)) end' "$evidence"
}

for milestone in $milestones; do
  current_milestone="$milestone"
  current_stage='validate_milestone'
  [[ "$milestone" =~ ^[1-9][0-9]*$ ]]
  output="${prefix}-${milestone}s.json"
  test ! -e "$output"
  current_stage='wait_for_evidence'
  while test "$(elapsed "$heads")" -lt "$milestone" || \
    test "$(elapsed "$upstream")" -lt "$milestone" || \
    test "$(elapsed "$rust0_resources")" -lt "$milestone" || \
    test "$(elapsed "$rust6_resources")" -lt "$milestone"; do
    test ! -f "$formal_failure"
    kill -0 "$head_pid" 2>/dev/null
    kill -0 "$upstream_pid" 2>/dev/null
    kill -0 "$rust0_pid" 2>/dev/null
    kill -0 "$rust6_pid" 2>/dev/null
    sleep 60
  done

  current_stage='audit_heads'
  head_audit="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    "$qualification" audit-soak "$heads" "$milestone" 120 6 1)"
  current_stage='audit_rust0_resources'
  rust0_audit="$(env N42_QUAL_RUNTIME="$runtime" \
    "$qualification" audit-rust-resources "$rust0_resources" "$milestone")"
  current_stage='audit_rust6_resources'
  rust6_audit="$(env N42_QUAL_RUNTIME="$runtime" \
    "$qualification" audit-rust-resources "$rust6_resources" "$milestone")"
  current_stage='audit_rust0_log'
  rust0_log="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_LOG_START="$log_start" \
    N42_QUAL_REQUIRE_TIMEOUTS=0 N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=1 \
    "$qualification" audit-runtime-logs "$runtime/logs/rust.log")"
  current_stage='audit_rust6_log'
  rust6_log="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_LOG_START="$log_start" \
    N42_QUAL_REQUIRE_TIMEOUTS=0 N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=1 \
    "$qualification" audit-runtime-logs "$runtime/logs/rust2.log")"
  current_stage='audit_gov5_upstream'
  upstream_audit="$(jq -sc --argjson minimum "$milestone" '
    {samples:length,firstAt:.[0].at,lastAt:.[-1].at,
     elapsedSeconds:((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)),
     allExact:(all(.[];.baselineExact and .remoteReachable)),
     remoteMains:([.[].remoteMain]|unique)} |
    select(.samples>=2 and .elapsedSeconds >= $minimum and .allExact and
      (.remoteMains|length)==1)
  ' "$upstream")"
  test -n "$upstream_audit"

  consensus='[]'
  equivocations='[]'
  current_stage='audit_consensus'
  for port in 29545 29546; do
    node_status="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"n42_consensusStatus","params":[]}' \
      "http://127.0.0.1:$port" | jq -ec --argjson port "$port" '
        {port:$port,view:.result.latestCommittedView,
         hash:.result.latestCommittedBlockHash,
         validatorCount:.result.validatorCount,hasCommittedQc:.result.hasCommittedQc} |
        select(.validatorCount==7 and .hasCommittedQc)
      ')"
    node_equivocations="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","id":1,"method":"n42_equivocations","params":[]}' \
      "http://127.0.0.1:$port" | jq -ec --argjson port "$port" '
        {port:$port,total:.result.total} | select(.total==0)
      ')"
    consensus="$(jq -nc --argjson a "$consensus" --argjson b "$node_status" '$a+[$b]')"
    equivocations="$(jq -nc --argjson a "$equivocations" \
      --argjson b "$node_equivocations" '$a+[$b]')"
  done

  current_stage='write_pass_artifact'
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson milestone "$milestone" \
    --argjson head "$head_audit" --argjson rust0 "$rust0_audit" \
    --argjson rust6 "$rust6_audit" --argjson rust0_log "$rust0_log" \
    --argjson rust6_log "$rust6_log" --argjson upstream "$upstream_audit" \
    --argjson consensus "$consensus" --argjson equivocations "$equivocations" '
    {at:$at,event:"gov5_seven_validator_milestone_audit",status:"PASS",
     milestoneSeconds:$milestone,head:$head,rust0Resource:$rust0,rust6Resource:$rust6,
     rust0Log:$rust0_log,rust6Log:$rust6_log,gov5Upstream:$upstream,
     consensus:$consensus,equivocations:$equivocations}
  ' >"$output.pending"
  mv "$output.pending" "$output"
done
