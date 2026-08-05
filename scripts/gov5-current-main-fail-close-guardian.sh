#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_FAIL_CLOSE_RUNTIME:?runtime is required}"
gov_repo="${N42_FAIL_CLOSE_GOV_REPO:?Gov5 repository is required}"
expected_main="${N42_FAIL_CLOSE_GOV_MAIN:?expected Gov5 main is required}"
interval="${N42_FAIL_CLOSE_INTERVAL_SECONDS:-60}"
output="${N42_FAIL_CLOSE_OUTPUT:-$runtime/evidence/gov5-current-main-fail-close-guardian.jsonl}"
failure="${N42_FAIL_CLOSE_FAILURE:-$runtime/evidence/gov5-current-main-fail-close-guardian-failure.json}"
completion="${N42_FAIL_CLOSE_COMPLETION:-$runtime/evidence/gov5-906-goal-completion-audit-v2.json}"
restart_evidence="$runtime/evidence/rust-restart-rejoin-906.jsonl"
preflight_only="${N42_FAIL_CLOSE_PREFLIGHT_ONLY:-0}"

[[ "$expected_main" =~ ^[0-9a-f]{40}$ ]]
[[ "$interval" =~ ^[1-9][0-9]*$ ]]
test -d "$runtime"
git -C "$gov_repo" rev-parse --git-dir >/dev/null
test ! -e "$failure"

node_pids() {
  local file
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    cat "$file"
  done
}

planned_rust_restart_in_progress() {
  local event started now age
  test -s "$restart_evidence" || return 1
  event="$(tail -n 1 "$restart_evidence" | jq -er '.event')"
  test "$event" = rust_restart_started || return 1
  started="$(tail -n 1 "$restart_evidence" | jq -er '.at|fromdateiso8601')"
  now="$(date +%s)"
  age=$((now - started))
  test "$age" -ge 0 && test "$age" -le 900
}

assert_nodes() {
  local file pid command
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file"
    pid="$(<"$file")"
    kill -0 "$pid"
    command="$(ps -p "$pid" -o command=)"
    [[ "$command" == "$runtime/geth-live "* ]]
  done
  file="$runtime/pids/rust.pid"
  test -s "$file"
  pid="$(<"$file")"
  if ! kill -0 "$pid" 2>/dev/null; then
    planned_rust_restart_in_progress
    return 0
  fi
  command="$(ps -p "$pid" -o command=)"
  [[ "$command" == *"/n42-node node "* ]]
}

stop_exact_nodes() {
  local pid
  while IFS= read -r pid; do
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid"
    fi
  done < <(node_pids)
}

remote_main() {
  git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}'
}

assert_nodes
remote="$(remote_main)"
test "$remote" = "$expected_main"

if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_main" \
    --arg remote "$remote" \
    '{at:$at,event:"gov5_current_main_fail_close_guardian_preflight",
      status:"PASS",expectedMain:$expected,remoteMain:$remote,nodesAlive:true,
      mutationPerformed:false}'
  exit 0
fi

iteration=0
while ! test -s "$completion"; do
  assert_nodes
  if remote="$(remote_main)" && test -n "$remote"; then
    if test "$remote" != "$expected_main"; then
      pids="$(node_pids | jq -R 'tonumber' | jq -sc '.')"
      jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_main" \
        --arg remote "$remote" --argjson pids "$pids" \
        '{at:$at,event:"gov5_current_main_fail_close_guardian_failure",
          status:"FAIL",reason:"Gov5 main moved",expectedMain:$expected,
          remoteMain:$remote,targetedNodePids:$pids,nodesStopped:true}' >"$failure"
      stop_exact_nodes
      exit 42
    fi
    if test $((iteration % 5)) -eq 0; then
      jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_main" \
        --arg remote "$remote" \
        '{at:$at,event:"gov5_current_main_fail_close_guardian_snapshot",
          status:"PASS",expectedMain:$expected,remoteMain:$remote,
          remoteReachable:true,nodesAlive:true}' >>"$output"
    fi
  else
    jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_main" \
      '{at:$at,event:"gov5_current_main_fail_close_guardian_snapshot",
        status:"WARN",expectedMain:$expected,remoteMain:null,
        remoteReachable:false,nodesAlive:true}' >>"$output"
  fi
  iteration=$((iteration + 1))
  sleep "$interval"
done

jq -e '.status=="PASS" and .objectiveRequirementsExtendedClosure' \
  "$completion" >/dev/null
jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected "$expected_main" \
  '{at:$at,event:"gov5_current_main_fail_close_guardian_complete",
    status:"PASS",expectedMain:$expected}' >>"$output"
