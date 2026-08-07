#!/usr/bin/env bash
set -euo pipefail

expected="${N42_EXPECTED_RETH_STABLE:-v2.4.1}"
interval="${N42_RETH_STABLE_MONITOR_INTERVAL_SECONDS:-600}"
qualification_dir="${N42_LATEST_RETH_QUALIFICATION_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}"
output="$qualification_dir/official-reth-stable-monitor.jsonl"
final="$qualification_dir/latest-reth-final-qualification.json"
remote="https://github.com/paradigmxyz/reth.git"
consecutive_failures=0

test ! -e "$output"
printf 'OFFICIAL_RETH_STABLE_MONITOR_READY pid=%s expected=%s at=%s\n' \
  "$$" "$expected" "$(date -u +%FT%TZ)"

while ! test -f "$final"; do
  at="$(date -u +%FT%TZ)"
  if tags="$(git ls-remote --tags "$remote" 'refs/tags/v*')"; then
    latest="$(sed -E 's#.*refs/tags/##; s/\^\{\}//' <<<"$tags" |
      rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
    test -n "$latest"
    consecutive_failures=0
    jq -nc --arg at "$at" --arg expected "$expected" --arg latest "$latest" \
      '{at:$at,event:"official_reth_stable_snapshot",remoteReachable:true,
        expected:$expected,latest:$latest,baselineExact:($expected==$latest)}' \
      >>"$output"
    if test "$latest" != "$expected"; then
      printf 'OFFICIAL_RETH_STABLE_CHANGED expected=%s latest=%s at=%s\n' \
        "$expected" "$latest" "$at" >&2
      exit 42
    fi
  else
    consecutive_failures=$((consecutive_failures + 1))
    jq -nc --arg at "$at" --arg expected "$expected" \
      --argjson failures "$consecutive_failures" \
      '{at:$at,event:"official_reth_stable_snapshot",remoteReachable:false,
        expected:$expected,latest:null,baselineExact:false,
        consecutiveFailures:$failures}' >>"$output"
    if test "$consecutive_failures" -ge 6; then
      printf 'OFFICIAL_RETH_STABLE_REMOTE_UNREACHABLE failures=%s at=%s\n' \
        "$consecutive_failures" "$at" >&2
      exit 43
    fi
  fi
  sleep "$interval"
done

jq -e '.status=="PASS" and .rethVersion=="2.4.1"' "$final" >/dev/null
printf 'OFFICIAL_RETH_STABLE_MONITOR_COMPLETE at=%s\n' "$(date -u +%FT%TZ)"
