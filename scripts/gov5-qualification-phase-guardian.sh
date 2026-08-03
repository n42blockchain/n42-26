#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_GUARD_RUNTIME:?runtime is required}"
qualification_dir="${N42_GUARD_LATEST_DIR:?latest-Reth qualification directory is required}"
gov_repo="${N42_GUARD_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_GUARD_GOV_MAIN:?expected Gov5 main commit is required}"
expected_rust_binary_sha="${N42_GUARD_RUST_BINARY_SHA:?expected Rust binary SHA-256 is required}"
finalizer_pid="${N42_GUARD_FINALIZER_PID:?finalizer PID is required}"
strict_verifier_pid="${N42_GUARD_STRICT_VERIFIER_PID:?strict verifier PID is required}"
rollover_pid="${N42_GUARD_ROLLOVER_PID:?rollover PID is required}"
latest_verifier_pid="${N42_GUARD_LATEST_VERIFIER_PID:?latest verifier PID is required}"
stable_monitor_pid="${N42_GUARD_STABLE_MONITOR_PID:?stable monitor PID is required}"
caffeinate_pid="${N42_GUARD_CAFFEINATE_PID:?caffeinate PID is required}"
interval="${N42_GUARD_INTERVAL_SECONDS:-60}"
once="${N42_GUARD_ONCE:-0}"

strict_summary="${N42_GUARD_STRICT_SUMMARY:-$runtime/evidence/gov5-906-final-qualification.json}"
strict_independent="${N42_GUARD_STRICT_INDEPENDENT:-$runtime/evidence/gov5-906-independent-final-verification.json}"
strict_heads="${N42_GUARD_STRICT_HEADS:-$runtime/evidence/mixed-soak-24h.jsonl}"
strict_failures="${N42_GUARD_STRICT_FAILURES:-$runtime/evidence/gov5-906-finalizer-failures.jsonl}"
latest_summary="${N42_GUARD_LATEST_SUMMARY:-$qualification_dir/latest-reth-final-qualification.json}"
latest_independent="${N42_GUARD_LATEST_INDEPENDENT:-$qualification_dir/latest-reth-independent-final-verification.json}"
latest_heads="${N42_GUARD_LATEST_HEADS:-$qualification_dir/latest-reth-heads-1h.jsonl}"
latest_failures="${N42_GUARD_LATEST_FAILURES:-$qualification_dir/latest-reth-failures.jsonl}"
guardian_status="${N42_GUARD_STATUS_FILE:-$runtime/evidence/gov5-qualification-phase-guardian.jsonl}"
guardian_failures="${N42_GUARD_FAILURE_FILE:-$runtime/evidence/gov5-qualification-phase-guardian-failures.jsonl}"

case "$once" in 0|1) ;; *) echo "N42_GUARD_ONCE must be 0 or 1" >&2; exit 2 ;; esac
[[ "$interval" =~ ^[1-9][0-9]*$ ]]
[[ "$expected_rust_binary_sha" =~ ^[0-9a-f]{64}$ ]]

if test -e "$guardian_status"; then
  test -s "$guardian_status"
  test ! -s "$guardian_failures"
  jq -e -s 'length >= 1 and all(.[]; .status == "PASS")' \
    "$guardian_status" >/dev/null
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson pid "$$" \
    '{at:$at,event:"gov5_qualification_phase_guardian_resumed",status:"PASS",
      pid:$pid,evidenceContinuityVerified:true}' >>"$guardian_status"
else
  test ! -e "$guardian_failures"
fi

fail() {
  local reason="$1"
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason "$reason" \
    '{at:$at,event:"gov5_qualification_phase_guardian_failure",
      status:"FAIL",reason:$reason}' >>"$guardian_failures"
  printf 'GOV5_QUALIFICATION_PHASE_GUARDIAN_FAILURE reason=%s\n' "$reason" >&2
  exit 1
}

require_process() {
  local pid="$1" expected="$2" command
  kill -0 "$pid" 2>/dev/null ||
    fail "missing process pid=$pid expected=$expected"
  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  test -n "$command" || fail "empty command pid=$pid expected=$expected"
  rg -F -- "$expected" <<<"$command" >/dev/null ||
    fail "PID reuse or command mismatch pid=$pid expected=$expected actual=$command"
}

require_live_nodes() {
  local file pid command binary
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file" || fail "missing Gov5 PID file $file"
    pid="$(<"$file")"
    require_process "$pid" "$runtime/geth-live"
  done
  file="$runtime/pids/rust.pid"
  test -s "$file" || fail "missing Rust PID file $file"
  pid="$(<"$file")"
  kill -0 "$pid" 2>/dev/null || fail "missing Rust process pid=$pid"
  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  case "$command" in
    "$runtime/n42-node node "*) binary="$runtime/n42-node" ;;
    "$qualification_dir/n42-node node "*) binary="$qualification_dir/n42-node" ;;
    *) fail "Rust PID reuse or unapproved binary path pid=$pid actual=$command" ;;
  esac
  test "$(shasum -a 256 "$binary" | awk '{print $1}')" = \
    "$expected_rust_binary_sha" || fail "Rust binary hash mismatch path=$binary"
}

check_phase() {
  local remote phase
  require_live_nodes
  require_process "$caffeinate_pid" "caffeinate -dimsu -t"
  test ! -s "$strict_failures" || fail "strict finalizer failure evidence is non-empty"
  test ! -s "$latest_failures" || fail "latest-Reth failure evidence is non-empty"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$expected_gov_main" ||
    fail "Gov5 main moved expected=$expected_gov_main actual=$remote"

  if ! test -s "$strict_summary"; then
    phase=strict
    require_process "$finalizer_pid" "gov5-current-qualification-finalizer.sh"
    require_process "$strict_verifier_pid" "verify-gov5-906-final-qualification.sh"
    require_process "$rollover_pid" "qualify-gov5-latest-reth-rollover.sh"
    require_process "$latest_verifier_pid" "verify-latest-reth-final-qualification.sh"
    require_process "$stable_monitor_pid" "monitor-official-reth-stable.sh"
    test -s "$strict_heads" || fail "strict head evidence is empty"
    tail -n 1 "$strict_heads" | jq -e '.ok==true and .zeroTxRequired==1' >/dev/null ||
      fail "latest strict head sample is unhealthy"
  elif ! test -s "$strict_independent"; then
    phase=strict-independent
    jq -e '.status=="PASS"' "$strict_summary" >/dev/null ||
      fail "strict summary is not PASS"
    require_process "$strict_verifier_pid" "verify-gov5-906-final-qualification.sh"
    require_process "$rollover_pid" "qualify-gov5-latest-reth-rollover.sh"
    require_process "$latest_verifier_pid" "verify-latest-reth-final-qualification.sh"
    require_process "$stable_monitor_pid" "monitor-official-reth-stable.sh"
  elif ! test -s "$latest_summary"; then
    phase=latest-reth
    jq -e '.status=="PASS"' "$strict_independent" >/dev/null ||
      fail "strict independent verification is not PASS"
    require_process "$rollover_pid" "qualify-gov5-latest-reth-rollover.sh"
    require_process "$latest_verifier_pid" "verify-latest-reth-final-qualification.sh"
    require_process "$stable_monitor_pid" "monitor-official-reth-stable.sh"
    if test -s "$latest_heads"; then
      tail -n 1 "$latest_heads" | jq -e '.ok==true' >/dev/null ||
        fail "latest-Reth head sample is unhealthy"
    fi
  else
    phase=latest-independent
    jq -e '.status=="PASS"' "$latest_summary" >/dev/null ||
      fail "latest-Reth summary is not PASS"
    require_process "$latest_verifier_pid" "verify-latest-reth-final-qualification.sh"
  fi
  printf '%s\n' "$phase:$remote"
}

iteration=0
while ! test -s "$latest_independent"; do
  state="$(check_phase)"
  phase="${state%%:*}"
  remote="${state#*:}"
  if test "$once" = 1 || test $((iteration % 5)) -eq 0; then
    jq -nc --arg at "$(date -u +%FT%TZ)" --arg phase "$phase" \
      --arg remote "$remote" --argjson pid "$$" \
      '{at:$at,event:"gov5_qualification_phase_guardian_snapshot",status:"PASS",
        pid:$pid,phase:$phase,govMain:$remote,nodesAlive:true,
        approvedRustBinaryPathAndHashExact:true,exactControllerPidsAlive:true}' \
      >>"$guardian_status"
  fi
  if test "$once" = 1; then
    cat "$guardian_status"
    exit 0
  fi
  iteration=$((iteration + 1))
  sleep "$interval"
done

jq -e '.status=="PASS"' "$strict_independent" >/dev/null ||
  fail "strict independent verification is not PASS at completion"
jq -e '.status=="PASS"' "$latest_independent" >/dev/null ||
  fail "latest-Reth independent verification is not PASS at completion"
jq -nc --arg at "$(date -u +%FT%TZ)" --argjson pid "$$" \
  '{at:$at,event:"gov5_qualification_phase_guardian_complete",
    status:"PASS",pid:$pid}' >>"$guardian_status"
printf 'GOV5_QUALIFICATION_PHASE_GUARDIAN_COMPLETE at=%s\n' "$(date -u +%FT%TZ)"
