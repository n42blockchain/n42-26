#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
expected="c0a14646813c10c6883a38d6f20e82ba96cf183a"
temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
count_file="$temporary_dir/count"
mode=eventual

git() {
  local count=0
  test -s "$count_file" && count="$(<"$count_file")"
  count=$((count + 1))
  printf '%s\n' "$count" >"$count_file"
  if test "$mode" = eventual && test "$count" -ge 3; then
    printf '%s\trefs/heads/main\n' "$expected"
    return 0
  fi
  return 128
}

for script in \
  "$script_dir/gov5-qualification-milestone-waiter.sh" \
  "$script_dir/gov5-milestone-supplemental-audit-waiter.sh" \
  "$script_dir/gov5-runtime-milestone-deep-audit.sh"; do
  definition="$(awk '/^remote_main_with_retry\(\)/,/^}/' "$script")"
  test -n "$definition"
  eval "$definition"
  gov_repo="$temporary_dir"
  N42_GOV_REMOTE_RETRY_ATTEMPTS=4
  N42_GOV_REMOTE_RETRY_DELAY_SECONDS=0

  : >"$count_file"
  mode=eventual
  test "$(remote_main_with_retry)" = "$expected"
  test "$(<"$count_file")" = 3

  : >"$count_file"
  mode=never
  N42_GOV_REMOTE_RETRY_ATTEMPTS=3
  if remote_main_with_retry >/dev/null; then
    echo "remote pin retry unexpectedly accepted an exhausted remote" >&2
    exit 1
  fi
  test "$(<"$count_file")" = 3
  unset -f remote_main_with_retry
done

for script in \
  "$script_dir/gov5-current-qualification-finalizer.sh" \
  "$script_dir/gov5-strict-independent-verifier-waiter.sh" \
  "$script_dir/verify-gov5-906-final-qualification.sh" \
  "$script_dir/gov5-runtime37-total-goal-finalizer.sh"; do
  definition="$(awk '/^git_ls_remote_with_retry\(\)/,/^}/' "$script")"
  test -n "$definition"
  eval "$definition"
  N42_GOV_REMOTE_RETRY_ATTEMPTS=4
  N42_GOV_REMOTE_RETRY_DELAY_SECONDS=0

  : >"$count_file"
  mode=eventual
  test "$(git_ls_remote_with_retry -C "$temporary_dir" ls-remote origin refs/heads/main)" = \
    "$expected"$'\trefs/heads/main'
  test "$(<"$count_file")" = 3

  : >"$count_file"
  mode=never
  N42_GOV_REMOTE_RETRY_ATTEMPTS=3
  if git_ls_remote_with_retry -C "$temporary_dir" ls-remote origin refs/heads/main \
    >/dev/null; then
    echo "generic remote retry unexpectedly accepted an exhausted remote" >&2
    exit 1
  fi
  test "$(<"$count_file")" = 3
  unset -f git_ls_remote_with_retry
done

jq -nc '{status:"PASS",scriptsChecked:7,transientFailuresRetried:true,
  exactRemoteRequired:true,retryExhaustionFailsClosed:true}'
