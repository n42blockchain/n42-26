#!/usr/bin/env bash
set -euo pipefail

duration_seconds="${1:?duration seconds required}"
interval_seconds="${2:-600}"
evidence_file="${3:?evidence file required}"
expected_main="${N42_QUAL_EXPECTED_GOV_UPSTREAM_SHA:-f65ef9d92426e29087687d0d13d43cde19a42706}"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
completion_file="${evidence_file%.jsonl}-complete.json"

test ! -e "$evidence_file"
test ! -e "$completion_file"
mkdir -p "$(dirname "$evidence_file")"

started_at="$(date -u +%FT%TZ)"
started_seconds="$(date +%s)"
samples=0

while true; do
  now="$(date -u +%FT%TZ)"
  now_seconds="$(date +%s)"
  remote_main=""
  remote_reachable=true
  if ! remote_main="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')" || test -z "$remote_main"; then
    remote_reachable=false
  fi
  baseline_exact=false
  if test "$remote_reachable" = true && test "$remote_main" = "$expected_main"; then
    baseline_exact=true
  fi

  jq -nc \
    --arg at "$now" \
    --arg baseline "$expected_main" \
    --arg remote_main "$remote_main" \
    --argjson remote_reachable "$remote_reachable" \
    --argjson baseline_exact "$baseline_exact" \
    '{at:$at,event:"gov5_upstream_snapshot",baseline:$baseline,
      remoteMain:$remote_main,remoteReachable:$remote_reachable,
      baselineExact:$baseline_exact}' >>"$evidence_file"
  samples=$((samples + 1))

  test "$baseline_exact" = true
  if test $((now_seconds - started_seconds)) -ge "$duration_seconds"; then
    break
  fi
  sleep "$interval_seconds"
done

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg started_at "$started_at" \
  --arg expected_main "$expected_main" \
  --arg evidence "$evidence_file" \
  --arg evidence_sha256 "$(shasum -a 256 "$evidence_file" | awk '{print $1}')" \
  --argjson duration_seconds "$duration_seconds" \
  --argjson elapsed_seconds "$(( $(date +%s) - started_seconds ))" \
  --argjson samples "$samples" \
  '{at:$at,event:"gov5_upstream_monitor_complete",status:"PASS",
    startedAt:$started_at,expectedMain:$expected_main,
    requestedDurationSeconds:$duration_seconds,elapsedSeconds:$elapsed_seconds,
    samples:$samples,evidence:$evidence,evidenceSha256:$evidence_sha256}' \
  >"$completion_file.pending"
mv "$completion_file.pending" "$completion_file"
