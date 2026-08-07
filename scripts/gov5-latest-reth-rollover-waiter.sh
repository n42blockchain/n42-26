#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_ROLLOVER_RUNTIME:?runtime is required}"
qualification_dir="${N42_ROLLOVER_QUAL_DIR:?qualification directory is required}"
finalizer_pid="${N42_ROLLOVER_FINALIZER_PID:?finalizer PID is required}"
stable_monitor_pid="${N42_ROLLOVER_STABLE_MONITOR_PID:?stable monitor PID is required}"
expected_self_sha="${N42_ROLLOVER_WAITER_SHA:?waiter SHA-256 is required}"
expected_script_sha="${N42_ROLLOVER_SCRIPT_SHA:?rollover SHA-256 is required}"
gov_repo="${N42_ROLLOVER_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_ROLLOVER_GOV_MAIN:?Gov5 main commit is required}"
expected_gov_candidate="${N42_ROLLOVER_GOV_CANDIDATE:?Gov5 candidate commit is required}"
source_repo="${N42_ROLLOVER_SOURCE_REPO:?latest-Reth source repository is required}"
source_commit="${N42_ROLLOVER_SOURCE_COMMIT:?latest-Reth source commit is required}"
independent="$runtime/evidence/gov5-906-independent-final-verification.json"
strict_summary="$runtime/evidence/gov5-906-final-qualification.json"
strict_heads="$runtime/evidence/mixed-soak-24h.jsonl"
script="$qualification_dir/qualify-gov5-latest-reth-rollover.sh"
latest_binary="$qualification_dir/n42-node"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_process() {
  local pid="$1" expected="$2" command
  kill -0 "$pid"
  command="$(ps -p "$pid" -o command=)"
  test -n "$command"
  rg -F -- "$expected" <<<"$command" >/dev/null
}

check_nodes() {
  local file pid command
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    pid="$(<"$file")"
    require_process "$pid" "$runtime/geth-live"
  done
  pid="$(<"$runtime/pids/rust.pid")"
  kill -0 "$pid"
  command="$(ps -p "$pid" -o command=)"
  [[ "$command" == "$runtime/n42-node node "* ]]
}

test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$script")" = "$expected_script_sha"
test "$(sha256 "$latest_binary")" =   0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f

while ! test -s "$independent"; do
  check_nodes
  require_process "$stable_monitor_pid" "monitor-official-reth-stable.sh"
  if ! test -s "$strict_summary"; then
    require_process "$finalizer_pid" "gov5-current-qualification-finalizer.sh"
  fi
  test ! -s "$runtime/evidence/gov5-906-finalizer-failures.jsonl"
  test ! -s "$runtime/evidence/gov5-906-independent-final-verification-failure.json"
  if test -s "$strict_heads"; then
    tail -n 1 "$strict_heads" |
      jq -e '.ok == true and .zeroTxRequired == 1' >/dev/null
  fi
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_main"
  sleep 60
done

jq -e '.status == "PASS"' "$independent" >/dev/null
exec env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_LATEST_RETH_QUAL_DIR="$qualification_dir" \
  N42_LATEST_RETH_BINARY="$latest_binary" \
  N42_LATEST_RETH_EXPECTED_SELF_SHA256="$expected_script_sha" \
  N42_LATEST_RETH_SOURCE_COMMIT="$source_commit" \
  N42_LATEST_RETH_SOURCE_REPO="$source_repo" \
  N42_LATEST_RETH_GOV_REPO="$gov_repo" \
  N42_LATEST_RETH_GOV_COMMIT="$expected_gov_candidate" \
  N42_LATEST_RETH_GOV_UPSTREAM="$expected_gov_main" \
  "$script" run
