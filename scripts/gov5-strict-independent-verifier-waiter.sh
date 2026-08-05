#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_INDEPENDENT_RUNTIME:?runtime is required}"
finalizer_pid="${N42_INDEPENDENT_FINALIZER_PID:?finalizer PID is required}"
verify_repo="${N42_INDEPENDENT_VERIFY_REPO:?verification repository is required}"
gov_repo="${N42_INDEPENDENT_GOV_REPO:?Gov5 repository is required}"
deps_repo="${N42_INDEPENDENT_DEPS_REPO:?dependency repository is required}"
reth_repo="${N42_INDEPENDENT_RETH_REPO:?Reth repository is required}"
expected_self_sha="${N42_INDEPENDENT_EXPECTED_SELF_SHA:?waiter SHA-256 is required}"
expected_verifier_sha="${N42_INDEPENDENT_VERIFIER_SHA:?verifier SHA-256 is required}"
expected_finalizer_sha="${N42_INDEPENDENT_FINALIZER_SHA:?finalizer SHA-256 is required}"
expected_harness_sha="${N42_INDEPENDENT_HARNESS_SHA:?qualification harness SHA-256 is required}"
expected_gov_main="${N42_INDEPENDENT_GOV_MAIN:?Gov5 main commit is required}"
expected_gov_candidate="${N42_INDEPENDENT_GOV_CANDIDATE:?Gov5 candidate commit is required}"
expected_deps_head="${N42_INDEPENDENT_DEPS_HEAD:?dependency commit is required}"
expected_reth_head="${N42_INDEPENDENT_RETH_HEAD:?Reth commit is required}"
expected_gov_binary_sha="${N42_INDEPENDENT_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
expected_rust_binary_sha="${N42_INDEPENDENT_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
ports="${N42_INDEPENDENT_PORTS:-28501 28502 28503 28504 28505 29545}"
summary="$runtime/evidence/gov5-906-final-qualification.json"
output="$runtime/evidence/gov5-906-independent-final-verification.json"
failure="$runtime/evidence/gov5-906-independent-final-verification-failure.json"
heads="$runtime/evidence/mixed-soak-24h.jsonl"
restart_evidence="$runtime/evidence/rust-restart-rejoin-906.jsonl"
finalizer="$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh"
verifier="$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh"

test ! -e "$output"
test ! -e "$failure"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_process() {
  local pid="$1"
  local expected="$2"
  local command
  kill -0 "$pid"
  command="$(ps -p "$pid" -o command=)"
  test -n "$command"
  rg -F -- "$expected" <<<"$command" >/dev/null
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

check_nodes() {
  local file pid command
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    pid="$(<"$file")"
    require_process "$pid" "$runtime/geth-live"
  done
  pid="$(<"$runtime/pids/rust.pid")"
  if ! kill -0 "$pid" 2>/dev/null; then
    planned_rust_restart_in_progress
    return 0
  fi
  command="$(ps -p "$pid" -o command=)"
  case "$command" in
    "$runtime/n42-node node "*|*/n42-node\ node\ *) ;;
    *) return 1 ;;
  esac
}

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    --argjson finalizer_pid "$finalizer_pid" \
    '{at:$at,event:"gov5_906_independent_final_verification_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command,
      finalizerPid:$finalizer_pid}' >"$failure"
  exit "$status"
}
trap on_error ERR

test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$verifier")" = "$expected_verifier_sha"
test "$(sha256 "$finalizer")" = "$expected_finalizer_sha"

while ! test -s "$summary"; do
  check_nodes
  require_process "$finalizer_pid" "gov5-current-qualification-finalizer.sh"
  test "$(sha256 "$finalizer")" = "$expected_finalizer_sha"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_main"
  test ! -s "$runtime/evidence/gov5-906-finalizer-failures.jsonl"
  if test -s "$heads"; then
    tail -n 1 "$heads" |
      jq -e '.ok == true and .zeroTxRequired == 1' >/dev/null
  fi
  sleep 60
done

jq -e '.status == "PASS"' "$summary" >/dev/null
temporary="$(mktemp "$runtime/evidence/.independent-final.XXXXXX")"
env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_VERIFY_REPO="$verify_repo" \
  N42_QUAL_GOV_REPO="$gov_repo" \
  N42_QUAL_DEPS_REPO="$deps_repo" \
  N42_QUAL_RETH_REPO="$reth_repo" \
  N42_QUAL_PAIRED_RETH_REPO="$reth_repo" \
  N42_QUAL_PORTS="$ports" \
  N42_VERIFY_EXPECTED_SELF_SHA="$expected_verifier_sha" \
  N42_VERIFY_GOV_UPSTREAM="$expected_gov_main" \
  N42_VERIFY_GOV_CANDIDATE="$expected_gov_candidate" \
  N42_VERIFY_DEPS_HEAD="$expected_deps_head" \
  N42_VERIFY_RETH_HEAD="$expected_reth_head" \
  N42_VERIFY_GOV_BINARY_SHA="$expected_gov_binary_sha" \
  N42_VERIFY_RUST_BINARY_SHA="$expected_rust_binary_sha" \
  N42_VERIFY_FINALIZER_SHA="$expected_finalizer_sha" \
  N42_VERIFY_HARNESS_SHA="$expected_harness_sha" \
  "$verifier" >"$temporary"
test -s "$temporary"
jq -e '.status == "PASS"' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
