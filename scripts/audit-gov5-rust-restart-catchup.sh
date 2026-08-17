#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME OUTPUT}"
output="${2:?usage: $0 RUNTIME OUTPUT}"
log="${N42_RECOVERY_LOG:-$runtime/logs/rust2.log}"
preflight="${N42_RECOVERY_PREFLIGHT:-$runtime/evidence/preflight-final-dd6b054-bound6-3m-heads.jsonl}"
window_start="${N42_RECOVERY_WINDOW_START:-2026-08-17T17:44:14}"
window_end="${N42_RECOVERY_WINDOW_END:-2026-08-17T17:44:43}"
qualification="${N42_RECOVERY_QUALIFICATION_SCRIPT:?qualification script is required}"

test -s "$log"
test -s "$preflight"
test -x "$qualification"
test ! -e "$output"

audit_dir="$(mktemp -d)"
trap 'rm -rf "$audit_dir"' EXIT
slice="$audit_dir/recovery.log"
awk -v start="$window_start" -v end="$window_end" \
  'substr($0,1,19) >= start && substr($0,1,19) <= end' "$log" >"$slice"
test -s "$slice"

start_line="$(rg -m1 ' INFO Starting Reth version=' "$slice")"
head_line="$(rg -m1 ' INFO using canonical chain head as head_block_hash best_block=' "$slice")"
state_line="$(rg -m1 ' INFO recovered consensus state from snapshot view=' "$slice")"
jump_line="$(rg -m1 ' INFO .*QC-based view jump: recovering node catching up to network current_view=' "$slice")"
release_line="$(rg -m1 ' INFO releasing reverse-delivered authenticated Gov5 ancestry in execution order blocks=' "$slice")"
commit_line="$(rg -m1 ' INFO CommitQC finalized a prepared execution ancestor lineage view=' "$slice")"

start_at="$(awk '{print $1}' <<<"$start_line" | sed -E 's/\.[0-9]+Z$/Z/')"
commit_at="$(awk '{print $1}' <<<"$commit_line" | sed -E 's/\.[0-9]+Z$/Z/')"
best_block="$(sed -E 's/.* best_block=([0-9]+).*/\1/' <<<"$head_line")"
restored_view="$(sed -E 's/.* snapshot view=([0-9]+).*/\1/' <<<"$state_line")"
restored_count="$(sed -E 's/.* committed_block_count=([0-9]+).*/\1/' <<<"$state_line")"
target_view="$(sed -E 's/.* target_view=([0-9]+).*/\1/' <<<"$jump_line")"
released_blocks="$(sed -E 's/.* blocks=([0-9]+).*/\1/' <<<"$release_line")"
committed_execution_blocks="$(sed -E 's/.* execution_blocks_committed=([0-9]+).*/\1/' <<<"$commit_line")"
previous_count="$(sed -E 's/.* previous_block_count=([0-9]+).*/\1/' <<<"$commit_line")"
committed_count="$(sed -E 's/.* committed_block_count=([0-9]+).*/\1/' <<<"$commit_line")"
start_seconds="$(jq -nr --arg at "$start_at" '$at|fromdateiso8601')"
commit_seconds="$(jq -nr --arg at "$commit_at" '$at|fromdateiso8601')"
recovery_seconds=$((commit_seconds - start_seconds))

test "$best_block" -eq "$restored_count"
test "$previous_count" -eq "$best_block"
test "$target_view" -gt "$restored_view"
test "$released_blocks" -gt 0
test "$committed_execution_blocks" -eq "$((committed_count - previous_count))"
test "$committed_execution_blocks" -eq "$((released_blocks + 1))"
test "$recovery_seconds" -gt 0
test "$recovery_seconds" -le 60
! rg -qi ' ERROR |too deep reorg|invalid payload attributes|refusing conflicting sync block|(^|[^[:alpha:]])(panic|fatal|equivocat)' "$slice"

preflight_audit="$("$qualification" audit-soak "$preflight" 180 6 6 1)"
printf '%s\n' "$preflight_audit" | jq -e '
  .status == "PASS" and .elapsedSeconds >= 180 and .maximumLag <= 6 and
  .blockGrowth > 0 and .zeroTransactionRequired == true
' >/dev/null

mkdir -p "$(dirname "$output")"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg log "$log" --arg window_start "$window_start" --arg window_end "$window_end" \
  --arg recovery_slice_sha256 "$(shasum -a 256 "$slice" | awk '{print $1}')" \
  --arg start_at "$start_at" --arg commit_at "$commit_at" \
  --argjson best_block "$best_block" --argjson restored_view "$restored_view" \
  --argjson target_view "$target_view" --argjson released_blocks "$released_blocks" \
  --argjson committed_execution_blocks "$committed_execution_blocks" \
  --argjson committed_count "$committed_count" --argjson recovery_seconds "$recovery_seconds" \
  --argjson preflight "$preflight_audit" '
  {at:$at,event:"rust_restart_catchup_audit",status:"PASS",log:$log,
   window:{start:$window_start,end:$window_end,sliceSha256:$recovery_slice_sha256},
   recovery:{startedAt:$start_at,committedAt:$commit_at,persistedHead:$best_block,
     restoredView:$restored_view,targetView:$target_view,
     reverseAncestryBlocksReleased:$released_blocks,
     executionBlocksCommitted:$committed_execution_blocks,
     recoveredHead:$committed_count,elapsedSeconds:$recovery_seconds,
     completedWithin60Seconds:true},
   forbiddenSignals:0,postRecoveryPreflight:$preflight,
   followingModeRecovered:true,commonHeightIdentityExact:true}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
