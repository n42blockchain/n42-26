#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_LATEST_VERIFY_RUNTIME:?runtime is required}"
qualification_dir="${N42_LATEST_VERIFY_QUAL_DIR:?qualification directory is required}"
source_repo="${N42_LATEST_VERIFY_SOURCE_REPO:?latest-Reth source repository is required}"
source_commit="${N42_LATEST_VERIFY_SOURCE_COMMIT:?latest-Reth source commit is required}"
primary_repo="${N42_LATEST_VERIFY_PRIMARY_REPO:?primary repository is required}"
gov_repo="${N42_LATEST_VERIFY_GOV_REPO:?Gov5 repository is required}"
expected_gov_candidate="${N42_LATEST_VERIFY_GOV_CANDIDATE:?Gov5 candidate is required}"
expected_gov_main="${N42_LATEST_VERIFY_GOV_MAIN:?Gov5 main is required}"
expected_self_sha="${N42_LATEST_VERIFY_WAITER_SHA:?waiter SHA-256 is required}"
expected_verifier_sha="${N42_LATEST_VERIFY_SCRIPT_SHA:?verifier SHA-256 is required}"
rollover_pid="${N42_LATEST_VERIFY_ROLLOVER_PID:?rollover controller PID is required}"
summary="$qualification_dir/latest-reth-final-qualification.json"
output="$qualification_dir/latest-reth-independent-final-verification.json"
failure="$qualification_dir/latest-reth-independent-final-verification-failure.json"
verifier="$qualification_dir/verify-latest-reth-final-qualification.sh"

test ! -e "$output"
test ! -e "$failure"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"latest_reth_independent_final_verification_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$verifier")" = "$expected_verifier_sha"

while ! test -s "$summary"; do
  kill -0 "$rollover_pid"
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    kill -0 "$(<"$file")"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_main"
  test ! -s "$qualification_dir/latest-reth-failures.jsonl"
  sleep 60
done

jq -e '.status == "PASS"' "$summary" >/dev/null
temporary="$(mktemp "$qualification_dir/.latest-independent.XXXXXX")"
env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_LATEST_RETH_QUAL_DIR="$qualification_dir" \
  N42_LATEST_RETH_SOURCE_COMMIT="$source_commit" \
  N42_LATEST_RETH_SOURCE_REPO="$source_repo" \
  N42_LATEST_RETH_PRIMARY_REPO="$primary_repo" \
  N42_LATEST_RETH_GOV_REPO="$gov_repo" \
  N42_LATEST_RETH_GOV_COMMIT="$expected_gov_candidate" \
  N42_LATEST_RETH_GOV_UPSTREAM="$expected_gov_main" \
  N42_LATEST_RETH_VERIFY_EXPECTED_SELF_SHA256="$expected_verifier_sha" \
  "$verifier" >"$temporary"
jq -e '.status == "PASS"' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"

