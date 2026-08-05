#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_CORRECTION_RUNTIME:?runtime is required}"
finalizer_pid="${N42_CORRECTION_FINALIZER_PID:?qualification finalizer PID is required}"
failure="$runtime/evidence/runtime37-latest-c0a146-formal-15m-supplemental-audit-failure.json"
milestone="$runtime/evidence/gov5-906-formal-15m-milestone.json"
archive="$runtime/evidence/runtime37-latest-c0a146-formal-15m-archive-qmdb-parity.jsonl"
network="$runtime/evidence/runtime37-latest-c0a146-formal-15m-network-consensus-matrix.json"
data="$runtime/evidence/runtime37-latest-c0a146-formal-15m-905-data-compatibility-audit.json"
resource="$runtime/evidence/rust-resource-24h-audit.json"
output="$runtime/evidence/runtime37-latest-c0a146-formal-15m-corrected-supplemental-audit.json"
waiter_failure="$runtime/evidence/runtime37-latest-c0a146-formal-15m-resource-correction-failure.json"

test -s "$failure"
test -s "$milestone"
test ! -e "$output"
test ! -e "$waiter_failure"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
original_failure_sha="$(sha256 "$failure")"

assert_original_failure() {
  test "$(sha256 "$failure")" = "$original_failure_sha"
  jq -e '
    .status=="FAIL" and .label=="formal-15m" and .statusCode==1 and
    (.command|contains("audit-rust-resource-trend"))
  ' "$failure" >/dev/null
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"runtime37_formal_15m_resource_correction_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$waiter_failure"
  exit "$status"
}
trap on_error ERR

assert_original_failure
jq -e '.status=="PASS" and .label=="formal-15m" and
  .soak.elapsedSeconds>=900 and .soak.maximumLag<=1 and
  .soak.zeroTransactionRequired==true and .resources.elapsedSeconds>=900 and
  .gov5Upstream.elapsedSeconds>=900 and .transactionsSent==0 and
  .failureEvidencePresent==false' "$milestone" >/dev/null

while ! test -s "$resource"; do
  assert_original_failure
  kill -0 "$finalizer_pid"
  command="$(ps -p "$finalizer_pid" -o command=)"
  rg -F 'gov5-current-qualification-finalizer.sh' <<<"$command" >/dev/null
  test ! -s "$runtime/evidence/gov5-906-finalizer-failures.jsonl"
  sleep 60
done

assert_original_failure
jq -e -s '
  length==12 and
  (map(select(.event=="archive_qmdb_reference_parity" and
    .govRustProofRootsExact and .govRustProofBytesExact and
    .govRustProofsOfflineVerified))|length)==1 and
  (map(select(.event=="archive_rpc_parity" and .govRustRpcExact and
    .qmdbProofRootExact and .qmdbProofOfflineVerified))|length)==11
' "$archive" >/dev/null
jq -e '.status=="PASS" and .consensusNetworkConnectedAndQuorate and
  .authenticatedValidatorPeerCount==5 and .allSixCommittedBlockIdentityExact and
  .equivocations.total==0' "$network" >/dev/null
jq -e '.status=="PASS" and .latestAndPendingNonce=="0x11" and
  .genesisAndCopiedHeadSixEndpointExact and .liveSixEndpointIdentityExact and
  .dataRecopyOrRegenerationRequired==false' "$data" >/dev/null
jq -e '.status=="PASS" and .elapsedSeconds>=86400 and .singleProcess==true and
  .logicalCountersMonotonic==true and .headLogAndWalCountersMonotonic==true and
  .rssKiB.maximum<=.rssKiB.limit and .threads.maximum<=.threads.limit and
  .fileDescriptors.maximum<=.fileDescriptors.limit' "$resource" >/dev/null

temporary="$(mktemp "$runtime/evidence/.runtime37-15m-correction.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg failure_sha "$original_failure_sha" --arg milestone_sha "$(sha256 "$milestone")" \
  --arg archive_sha "$(sha256 "$archive")" --arg network_sha "$(sha256 "$network")" \
  --arg data_sha "$(sha256 "$data")" --arg resource_sha "$(sha256 "$resource")" \
  --slurpfile resource "$resource" '
  {at:$at,event:"runtime37_formal_15m_resource_projection_correction",status:"PASS",
   label:"formal-15m",acceptanceRelaxed:false,mutationPerformed:false,
   originalFailurePreserved:true,originalFailureSha256:$failure_sha,
   originalFailureScope:"short-window 24h RSS projection only",
   rootCause:"five 5-minute samples over 1200 seconds amplified RSS warm-up noise in an OLS 24h forecast",
   correction:"replace the failed forecast only after the strict measured 24h resource audit passes",
   originalMilestoneTransactionsSent:0,correctionTransactionsSent:0,
   archiveAndQmdbParityExact:true,networkConsensusExact:true,
   data905CompatibilityExact:true,resourceTrendWithin24hBudget:true,
   measured24hResourceAudit:$resource[0],noUncorrectedFailureEvidence:true,
   evidenceSha256:{milestone:$milestone_sha,archiveQmdb:$archive_sha,
     networkMatrix:$network_sha,data905Compatibility:$data_sha,
     measured24hResourceAudit:$resource_sha}}' >"$temporary"
jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .mutationPerformed==false and .originalFailurePreserved==true and
  .measured24hResourceAudit.elapsedSeconds>=86400 and
  .archiveAndQmdbParityExact and .networkConsensusExact and
  .data905CompatibilityExact and .resourceTrendWithin24hBudget and
  .noUncorrectedFailureEvidence' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
