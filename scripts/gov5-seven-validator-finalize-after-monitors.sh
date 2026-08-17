#!/usr/bin/env bash
set -Eeuo pipefail

# Wait for the independently started long monitors, then perform the final
# read-only seven-validator audit.  A missing upstream completion record is a
# hard failure: it means Gov5 main moved or the guardian did not finish.

runtime="${N42_FINAL_RUNTIME:?runtime is required}"
gov_repo="${N42_FINAL_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_FINAL_EXPECTED_GOV_MAIN:?expected Gov5 main SHA is required}"
expected_gov_binary="${N42_FINAL_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
expected_rust_binary="${N42_FINAL_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
log_start="${N42_FINAL_LOG_START:-}"
head_monitor_pid="${N42_FINAL_HEAD_MONITOR_PID:?head monitor PID is required}"
upstream_monitor_pid="${N42_FINAL_UPSTREAM_MONITOR_PID:?upstream monitor PID is required}"
rust0_monitor_pid="${N42_FINAL_RUST0_MONITOR_PID:?Rust0 monitor PID is required}"
rust6_monitor_pid="${N42_FINAL_RUST6_MONITOR_PID:?Rust6 monitor PID is required}"
qualification="${N42_FINAL_QUALIFICATION_SCRIPT:-$runtime/artifacts/scripts/gov5-interop-qualification.sh}"
verifier="${N42_FINAL_VERIFIER:?seven-validator final verifier is required}"
evidence_dir="${N42_FINAL_EVIDENCE_DIR:-$runtime/evidence}"
heads="${N42_FINAL_HEADS:-$evidence_dir/runtime42-seven-validator-24h-head-monitor.jsonl}"
upstream="${N42_FINAL_UPSTREAM:-$evidence_dir/runtime42-gov5-upstream-24h.jsonl}"
upstream_complete="${N42_FINAL_UPSTREAM_COMPLETE:-${upstream%.jsonl}-complete.json}"
rust0_resources="${N42_FINAL_RUST0_RESOURCES:-$evidence_dir/runtime42-rust0-resource-24h.jsonl}"
rust6_resources="${N42_FINAL_RUST6_RESOURCES:-$evidence_dir/runtime42-rust6-resource-24h.jsonl}"
failure="${N42_FINAL_FAILURE:-$evidence_dir/runtime42-seven-validator-finalizer-failure.json}"
output="${N42_FINAL_OUTPUT:-$evidence_dir/runtime42-seven-validator-final-verification.json}"
rust0_leaders="${N42_FINAL_RUST0_LEADERS:-$evidence_dir/runtime42-rust0-final-leader-range.json}"
rust6_leaders="${N42_FINAL_RUST6_LEADERS:-$evidence_dir/runtime42-rust6-final-leader-range.json}"
execution_audit="${N42_FINAL_EXECUTION_AUDIT:-$evidence_dir/runtime42-seven-endpoint-evm-execution.json}"
recovery_audit="${N42_FINAL_RECOVERY_AUDIT:?Rust restart catch-up audit is required}"
execution_audit_script="${N42_FINAL_EXECUTION_AUDIT_SCRIPT:-$runtime/artifacts/scripts/audit-gov5-burst-readonly.sh}"
ports='28501 28502 28503 28504 28505 29545 29546'
minimum_duration="${N42_FINAL_MINIMUM_DURATION_SECONDS:-86400}"
current_stage='startup'

fail() {
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason "$1" \
    '{at:$at,event:"gov5_seven_validator_finalizer",status:"FAIL",reason:$reason}' \
    >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit 1
}
record_unexpected_failure() {
  local status="${1:?exit status required}"
  local command="${2:-unknown}"
  local line="${3:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason 'unexpected command failure' \
    --arg stage "$current_stage" --arg command "$command" \
    --argjson line "$line" --argjson exitStatus "$status" '
    {at:$at,event:"gov5_seven_validator_finalizer",status:"FAIL",reason:$reason,
     stage:$stage,command:$command,line:$line,exitStatus:$exitStatus}
  ' >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit "$status"
}
alive() { kill -0 "$1" 2>/dev/null; }
evidence_elapsed() {
  local evidence="${1:?evidence required}"
  if ! test -f "$evidence"; then
    printf '0\n'
    return
  fi
  jq -sr 'if length < 2 then 0 else
    ((.[-1].at | fromdateiso8601) - (.[0].at | fromdateiso8601)) end' \
    "$evidence"
}

trap 'record_unexpected_failure "$?" "$BASH_COMMAND" "$LINENO"' ERR

current_stage='validate_configuration'
[[ "$minimum_duration" =~ ^[1-9][0-9]*$ ]] || fail 'minimum duration must be positive'
test ! -e "$output" || fail "final verification output already exists: $output"
test ! -e "$failure" || exit 1
for script in "$qualification" "$verifier" "$execution_audit_script"; do
  test -x "$script" || fail "missing executable: $script"
done

current_stage='wait_for_monitors'
while alive "$head_monitor_pid" || alive "$upstream_monitor_pid" || \
  alive "$rust0_monitor_pid" || alive "$rust6_monitor_pid"; do
  # The upstream guardian must outlive the window.  If it stops without its
  # PASS artifact, do not wait for the other monitors or use their evidence.
  if ! alive "$upstream_monitor_pid" && ! test -f "$upstream_complete"; then
    fail 'Gov5 upstream guardian exited without PASS completion'
  fi
  if ! alive "$head_monitor_pid" && \
    test "$(evidence_elapsed "$heads")" -lt "$minimum_duration"; then
    fail 'head monitor exited before the complete qualification window'
  fi
  if ! alive "$rust0_monitor_pid" && \
    test "$(evidence_elapsed "$rust0_resources")" -lt "$minimum_duration"; then
    fail 'Rust0 resource monitor exited before the complete qualification window'
  fi
  if ! alive "$rust6_monitor_pid" && \
    test "$(evidence_elapsed "$rust6_resources")" -lt "$minimum_duration"; then
    fail 'Rust6 resource monitor exited before the complete qualification window'
  fi
  sleep 60
done

current_stage='validate_monitor_completion'
test -f "$upstream_complete" || \
  fail 'Gov5 upstream guardian completed without PASS artifact'
for evidence in "$heads" "$rust0_resources" "$rust6_resources"; do
  test "$(evidence_elapsed "$evidence")" -ge "$minimum_duration" || \
    fail "monitor evidence is shorter than the qualification window: $evidence"
done

# Give all endpoints a small finalized margin, then discover each Rust
# validator's current seven-slot phase from the canonical chain.  Do not carry
# a height anchor across runtimes: an epoch schedule transition can preserve
# the validator order while changing the height/view offset.
current_stage='discover_final_leader_ranges'
head_hex="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' \
  http://127.0.0.1:29545 | jq -er '.result')"
head=$((head_hex))
target=$((head - 80))
find_leader_start() {
  local miner="${1:?miner required}" candidate tag observed
  for candidate in $(seq "$target" -1 "$((target - 6))"); do
    tag="$(printf '0x%x' "$candidate")"
    observed="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
      --data "$(jq -nc --arg tag "$tag" \
        '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$tag,false]}')" \
      http://127.0.0.1:29545 | jq -er '.result.miner | ascii_downcase')"
    if test "$observed" = "$miner"; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}
rust0_start="$(find_leader_start 0x81d4c1f92ddb837cb46f82280d9b491b101fa582)" || \
  fail 'could not discover Rust0 leader slot in canonical seven-block cycle'
rust6_start="$(find_leader_start 0x853b2026deebc83fb79ac7d0c48efea595c22578)" || \
  fail 'could not discover Rust6 leader slot in canonical seven-block cycle'
rust0_end=$((rust0_start + 70))
rust6_end=$((rust6_start + 70))
test "$rust0_end" -lt "$head" || fail 'insufficient final head margin for Rust0 audit'
test "$rust6_end" -lt "$head" || fail 'insufficient final head margin for Rust6 audit'

current_stage='audit_rust0_leaders'
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT=29545 N42_QUAL_RUST_MINER=0x81d4c1f92ddb837cb46f82280d9b491b101fa582 \
  N42_QUAL_RUST_LEADER_STRIDE=7 "$qualification" audit-rust-leaders \
  "$rust0_start" "$rust0_end" "$rust0_leaders" >/dev/null
current_stage='audit_rust6_leaders'
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT=29546 N42_QUAL_RUST_MINER=0x853b2026deebc83fb79ac7d0c48efea595c22578 \
  N42_QUAL_RUST_LEADER_STRIDE=7 "$qualification" audit-rust-leaders \
  "$rust6_start" "$rust6_end" "$rust6_leaders" >/dev/null

current_stage='audit_readonly_evm_execution'
test ! -e "$execution_audit" || fail "execution audit output already exists: $execution_audit"
"$execution_audit_script" "$runtime" "$execution_audit" >/dev/null || \
  fail 'seven-endpoint EVM execution audit failed'

current_stage='run_final_verifier'
env N42_FINAL_RUNTIME="$runtime" N42_FINAL_GOV_REPO="$gov_repo" \
  N42_FINAL_EXPECTED_GOV_MAIN="$expected_gov_main" \
  N42_FINAL_GOV_BINARY_SHA="$expected_gov_binary" N42_FINAL_RUST_BINARY_SHA="$expected_rust_binary" \
  N42_FINAL_LOG_START="$log_start" \
  N42_FINAL_MAX_LAG="${N42_FINAL_MAX_LAG:-6}" \
  N42_FINAL_QUALIFICATION_SCRIPT="$qualification" N42_FINAL_VERIFIER="$verifier" \
  N42_FINAL_EVIDENCE_DIR="$evidence_dir" N42_FINAL_HEADS="$heads" \
  N42_FINAL_UPSTREAM="$upstream" N42_FINAL_UPSTREAM_COMPLETE="$upstream_complete" \
  N42_FINAL_RUST0_RESOURCES="$rust0_resources" N42_FINAL_RUST6_RESOURCES="$rust6_resources" \
  N42_FINAL_EXECUTION_AUDIT="$execution_audit" \
  N42_FINAL_RECOVERY_AUDIT="$recovery_audit" \
  N42_FINAL_RUST0_LEADERS="$rust0_leaders" N42_FINAL_RUST6_LEADERS="$rust6_leaders" \
  N42_FINAL_OUTPUT="$output" "$verifier"
