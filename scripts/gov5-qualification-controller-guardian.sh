#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_GUARD_RUNTIME:?runtime is required}"
qualification_dir="${N42_GUARD_LATEST_DIR:?latest-Reth directory is required}"
gov_repo="${N42_GUARD_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_GUARD_GOV_MAIN:?Gov5 main is required}"
expected_self_sha="${N42_GUARD_EXPECTED_SELF_SHA:?guardian SHA-256 is required}"
finalizer_pid="${N42_GUARD_FINALIZER_PID:?finalizer PID is required}"
strict_pid="${N42_GUARD_STRICT_PID:?strict verifier waiter PID is required}"
rollover_pid="${N42_GUARD_ROLLOVER_PID:?rollover controller PID is required}"
latest_pid="${N42_GUARD_LATEST_PID:?latest verifier waiter PID is required}"
stable_pid="${N42_GUARD_STABLE_PID:?stable monitor PID is required}"
immutable_pid="${N42_GUARD_IMMUTABLE_PID:?immutable waiter PID is required}"
gate_pid="${N42_GUARD_GATE_PID:?immutable gate PID is required}"
caffeinate_pid="${N42_GUARD_CAFFEINATE_PID:?caffeinate PID is required}"
interval="${N42_GUARD_INTERVAL_SECONDS:-60}"
expected_rust_sha="${N42_GUARD_RUST_SHA:-0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f}"

strict_summary="$runtime/evidence/gov5-906-final-qualification.json"
strict_independent="$runtime/evidence/gov5-906-independent-final-verification.json"
immutable="$runtime/evidence/gov5-906-immutable-final-log-verification.json"
gate="$runtime/evidence/gov5-906-immutable-final-log-gate.json"
latest_summary="$qualification_dir/latest-reth-final-qualification.json"
latest_independent="$qualification_dir/latest-reth-independent-final-verification.json"
heads="$runtime/evidence/mixed-soak-24h.jsonl"
status="$runtime/evidence/gov5-qualification-controller-guardian.jsonl"
failure="$runtime/evidence/gov5-qualification-controller-guardian-failures.jsonl"

test ! -e "$status"
test ! -e "$failure"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

fail() {
  local reason="$1"
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason "$reason" \
    '{at:$at,event:"gov5_qualification_controller_guardian_failure",
      status:"FAIL",reason:$reason}' >>"$failure"
  printf 'GOV5_CONTROLLER_GUARDIAN_FAILURE reason=%s\n' "$reason" >&2
  exit 1
}

require_process_any() {
  local pid="$1"
  shift
  local command expected
  kill -0 "$pid" 2>/dev/null || fail "missing process pid=$pid"
  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  test -n "$command" || fail "empty process command pid=$pid"
  for expected in "$@"; do
    if rg -F -- "$expected" <<<"$command" >/dev/null; then
      return 0
    fi
  done
  fail "PID reuse or command mismatch pid=$pid command=$command"
}

require_nodes() {
  local file pid command binary
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file" || fail "missing node PID file $file"
    pid="$(<"$file")"
    require_process_any "$pid" "$runtime/geth-live"
  done
  file="$runtime/pids/rust.pid"
  test -s "$file" || fail "missing Rust PID file"
  pid="$(<"$file")"
  kill -0 "$pid" 2>/dev/null || fail "missing Rust node pid=$pid"
  command="$(ps -p "$pid" -o command= 2>/dev/null || true)"
  case "$command" in
    "$runtime/n42-node node "*) binary="$runtime/n42-node" ;;
    "$qualification_dir/n42-node node "*) binary="$qualification_dir/n42-node" ;;
    *) fail "unexpected Rust binary command pid=$pid command=$command" ;;
  esac
  test "$(sha256 "$binary")" = "$expected_rust_sha" ||
    fail "Rust binary hash mismatch path=$binary"
}

iteration=0
while ! test -s "$latest_independent"; do
  test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha" ||
    fail "guardian script hash changed"
  require_nodes
  require_process_any "$caffeinate_pid" "caffeinate -dimsu -t 108000"

  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-immutable-final-log-verification-failure.json" \
    "$runtime/evidence/gov5-906-immutable-final-log-gate-failure.json" \
    "$qualification_dir/latest-reth-failures.jsonl" \
    "$qualification_dir/latest-reth-independent-final-verification-failure.json"; do
    test ! -s "$path" || fail "failure evidence is non-empty: $path"
  done

  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$expected_gov_main" ||
    fail "Gov5 main moved expected=$expected_gov_main actual=$remote"

  if ! test -s "$strict_summary"; then
    phase="strict"
    require_process_any "$finalizer_pid" "gov5-current-qualification-finalizer.sh"
    require_process_any "$strict_pid" "gov5-strict-independent-verifier-waiter.sh"
    require_process_any "$immutable_pid" "runtime22-immutable-final-log-waiter.sh"
    require_process_any "$gate_pid" "runtime22-immutable-gate-controller.sh"
  elif ! test -s "$immutable"; then
    phase="immutable-log"
    require_process_any "$strict_pid" "gov5-strict-independent-verifier-waiter.sh"
    require_process_any "$immutable_pid" "runtime22-immutable-final-log-waiter.sh"
    require_process_any "$gate_pid" "runtime22-immutable-gate-controller.sh"
  elif ! test -s "$strict_independent"; then
    phase="strict-independent"
    jq -e '.status=="PASS"' "$strict_summary" >/dev/null ||
      fail "strict summary is not PASS"
    jq -e '.status=="PASS"' "$immutable" >/dev/null ||
      fail "immutable verification is not PASS"
    require_process_any "$strict_pid" \
      "gov5-strict-independent-verifier-waiter.sh" \
      "verify-gov5-906-final-qualification.sh"
  elif ! test -s "$latest_summary"; then
    phase="latest-reth"
    jq -e '.status=="PASS"' "$strict_independent" >/dev/null ||
      fail "strict independent verification is not PASS"
    require_process_any "$rollover_pid" \
      "gov5-latest-reth-rollover-waiter.sh" \
      "qualify-gov5-latest-reth-rollover.sh"
    require_process_any "$stable_pid" "monitor-official-reth-stable.sh"
  else
    phase="latest-independent"
    jq -e '.status=="PASS"' "$latest_summary" >/dev/null ||
      fail "latest-Reth summary is not PASS"
  fi

  if ! test -s "$latest_summary"; then
    require_process_any "$rollover_pid" \
      "gov5-latest-reth-rollover-waiter.sh" \
      "qualify-gov5-latest-reth-rollover.sh"
    require_process_any "$stable_pid" "monitor-official-reth-stable.sh"
  fi
  require_process_any "$latest_pid" \
    "gov5-latest-reth-independent-verifier-waiter.sh" \
    "verify-latest-reth-final-qualification.sh"

  test -s "$heads" || fail "strict head evidence is empty"
  tail -n 1 "$heads" | jq -e '.ok==true and .zeroTxRequired==1' >/dev/null ||
    fail "latest strict head sample is unhealthy"

  if test $((iteration % 5)) -eq 0; then
    jq -nc --arg at "$(date -u +%FT%TZ)" --arg phase "$phase" \
      --arg gov_main "$remote" \
      '{at:$at,event:"gov5_qualification_controller_guardian_snapshot",
        status:"PASS",phase:$phase,govMain:$gov_main,nodesAlive:true,
        controllerPidsExact:true}' >>"$status"
  fi
  iteration=$((iteration + 1))
  sleep "$interval"
done

jq -e '.status=="PASS"' "$strict_independent" >/dev/null
jq -e '.status=="PASS"' "$latest_independent" >/dev/null
jq -nc --arg at "$(date -u +%FT%TZ)" \
  '{at:$at,event:"gov5_qualification_controller_guardian_complete",status:"PASS"}' \
  >>"$status"
