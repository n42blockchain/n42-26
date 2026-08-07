#!/usr/bin/env bash
set -euo pipefail

runtime37="${N42_EXCLUSION_RUNTIME37:?runtime37 is required}"
runtime38="${N42_EXCLUSION_RUNTIME38:?runtime38 is required}"
n42_repo="${N42_EXCLUSION_N42_REPO:?N42 repository is required}"
expected_fix="${N42_EXCLUSION_FIX_COMMIT:?fix commit is required}"
output="${N42_EXCLUSION_OUTPUT:-$runtime37/evidence/runtime37-final-log-warning-exclusion.json}"
old_failure="$runtime37/evidence/gov5-906-finalizer-failures.jsonl"
resume_failure="$runtime37/evidence/gov5-906-finalizer-resume-failures.jsonl"
correction="$runtime37/evidence/gov5-906-post-burst-correction.json"
post_burst_audit="$runtime37/evidence/mixed-post-burst-10m-audit.json"
post_restart_audit="$runtime37/evidence/mixed-post-restart-10m-audit.json"
restart="$runtime37/evidence/rust-restart-rejoin-906.jsonl"
rust_log="$runtime37/evidence/final-log-snapshot/logs/rust.log"
copy_manifest="$runtime38/evidence/runtime38-stopped-copy-manifest.json"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

test ! -e "$output"
jq -e -s 'length == 1 and .[0].statusCode == 1 and
  (.[0].command | contains("transaction-burst"))' "$old_failure" >/dev/null
jq -e -s 'length == 1 and .[0].statusCode == 1 and
  (.[0].command | contains("audit-runtime-logs"))' "$resume_failure" >/dev/null
jq -e '.status == "PASS" and .transactionsResent == 0' "$correction" >/dev/null
jq -e '.status == "PASS" and .elapsedSeconds >= 600' "$post_burst_audit" >/dev/null
jq -e '.status == "PASS" and .elapsedSeconds >= 600' "$post_restart_audit" >/dev/null
jq -e -s 'length == 2 and .[0].event == "rust_restart_started" and
  .[1].event == "rust_restart_rejoined" and .[1].pidAfter != .[0].pidBefore' "$restart" >/dev/null
warning_count="$(rg -c ' WARN leader peer not found for tx forward leader_idx=6 buf_len=1 peers=5$' "$rust_log")"
test "$warning_count" = 9426
first_warning="$(rg -m1 ' WARN leader peer not found for tx forward leader_idx=6 buf_len=1 peers=5$' "$rust_log")"
last_warning="$(rg ' WARN leader peer not found for tx forward leader_idx=6 buf_len=1 peers=5$' "$rust_log" | tail -n 1)"
test "$(git -C "$n42_repo" rev-parse HEAD)" = "$expected_fix"
test "$(git -C "$n42_repo" rev-parse '@{upstream}')" = "$expected_fix"
jq -e '.status == "PASS" and .files == 141 and .bytes == 17325704613 and
  .sourceManifestSha256 == .targetManifestSha256 and
  .allPathsSizesAndHashesExact == true and
  (.source | contains("runtime-34-")) and (.target | contains("runtime-38-"))' \
  "$copy_manifest" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg runtime37 "$runtime37" --arg runtime38 "$runtime38" \
  --arg old_failure "$old_failure" --arg old_failure_sha "$(sha256 "$old_failure")" \
  --arg resume_failure "$resume_failure" --arg resume_failure_sha "$(sha256 "$resume_failure")" \
  --arg correction "$correction" --arg correction_sha "$(sha256 "$correction")" \
  --arg rust_log "$rust_log" --arg rust_log_sha "$(sha256 "$rust_log")" \
  --argjson warning_count "$warning_count" --arg first_warning "$first_warning" \
  --arg last_warning "$last_warning" --arg fix "$expected_fix" \
  --arg manifest "$copy_manifest" --arg manifest_sha "$(sha256 "$copy_manifest")" '
  {at:$at,event:"runtime37_final_log_warning_exclusion",status:"EXCLUDED",
   acceptanceRelaxed:false,runtime:$runtime37,
   priorFailures:{transactionBurst:{path:$old_failure,sha256:$old_failure_sha,preserved:true},
     finalRuntimeLogAudit:{path:$resume_failure,sha256:$resume_failure_sha,preserved:true}},
   burstCorrection:{path:$correction,sha256:$correction_sha,transactionsResent:0},
   passedBeforeExclusion:{strict24h:true,transactionBurst17:true,postBurst10m:true,
     archiveAndQmdbParity:true,rustRestartRejoin:true,postRestart10m:true},
   exclusionReason:"missing-validator tx-forward warning was emitted every 50 ms instead of once per view",
   frozenRustLog:{path:$rust_log,sha256:$rust_log_sha,
     repeatedMissingLeaderWarnings:$warning_count,first:$first_warning,last:$last_warning},
   sourceFix:{commit:$fix,pushed:true,warningRateLimitedToOncePerView:true},
   replacement:{runtime:$runtime38,sourceRuntime34StoppedData:true,
     stoppedCopyManifest:$manifest,stoppedCopyManifestSha256:$manifest_sha,
     files:141,bytes:17325704613,sourceAndTargetExact:true,
     runtime37AdvancedDataReused:false,strictQualificationRestartsFromZero:true},
   finalQualificationCredited:false}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
