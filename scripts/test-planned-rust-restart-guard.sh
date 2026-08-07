#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
scripts=(
  "$script_dir/gov5-strict-independent-verifier-waiter.sh"
  "$script_dir/gov5-current-main-fail-close-guardian.sh"
  "$script_dir/gov5-runtime37-total-goal-finalizer.sh"
)

definition=""
for script in "${scripts[@]}"; do
  current="$(awk '/^planned_rust_restart_in_progress\(\)/,/^}/' "$script")"
  test -n "$current"
  if test -z "$definition"; then
    definition="$current"
  else
    test "$current" = "$definition"
  fi
done

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
restart_evidence="$temporary_dir/restart.jsonl"
eval "$definition"

now="$(date -u +%FT%TZ)"
jq -nc --arg at "$now" '{at:$at,event:"rust_restart_started"}' >"$restart_evidence"
planned_rust_restart_in_progress

jq -nc --arg at "$now" '{at:$at,event:"rust_restart_rejoined"}' >"$restart_evidence"
if planned_rust_restart_in_progress; then
  echo "completed restart was incorrectly treated as in progress" >&2
  exit 1
fi

old="$(date -u -r "$(( $(date +%s) - 901 ))" +%FT%TZ)"
jq -nc --arg at "$old" '{at:$at,event:"rust_restart_started"}' >"$restart_evidence"
if planned_rust_restart_in_progress; then
  echo "stale restart window was incorrectly accepted" >&2
  exit 1
fi

jq -nc '{status:"PASS",identicalGuardImplementation:true,
  activeRestartAccepted:true,completedRestartRejected:true,staleRestartRejected:true}'
