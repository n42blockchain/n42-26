#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME LABEL MIN_ELAPSED GOV_REPO EXPECTED_MAIN NETWORK_AUDITOR NETWORK_SHA DATA_AUDITOR DATA_SHA RESOURCE_AUDITOR RESOURCE_SHA}"
label="${2:?milestone label is required}"
minimum_elapsed="${3:?minimum elapsed seconds is required}"
gov_repo="${4:?Gov5 repository is required}"
expected_main="${5:?expected Gov5 main commit is required}"
network_auditor="${6:?network auditor is required}"
expected_network_sha="${7:?network auditor SHA-256 is required}"
data_auditor="${8:?905 data auditor is required}"
expected_data_sha="${9:?905 data auditor SHA-256 is required}"
resource_auditor="${10:?resource trend auditor is required}"
expected_resource_sha="${11:?resource trend auditor SHA-256 is required}"
preflight_only="${N42_SUPPLEMENTAL_PREFLIGHT_ONLY:-0}"
milestone="$runtime/evidence/gov5-906-$label-milestone.json"
resource_snapshot="$runtime/evidence/gov5-906-$label-resources.snapshot.jsonl"
evidence_prefix="${N42_SUPPLEMENTAL_EVIDENCE_PREFIX:-$runtime/evidence/runtime28-$label}"
archive="$evidence_prefix-archive-qmdb-parity.jsonl"
network="$evidence_prefix-network-consensus-matrix.json"
data="$evidence_prefix-905-data-compatibility-audit.json"
resource="$evidence_prefix-rust-resource-trend-audit.json"
output="$evidence_prefix-supplemental-audit.json"
failure="$evidence_prefix-supplemental-audit-failure.json"
harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
qmdb_verifier="$runtime/artifacts/binaries/n42-qmdb-proof-verify"
expected_harness_sha="037cc547eb958f0b993565b81aefe30b239e0ad061c27895e3287c6d23e95309"
expected_qmdb_sha="b329baa1e51435082b2bb2cf538a8d1a1ffd994b5c4ac73474e688ffbfc35c19"

[[ "$label" =~ ^[a-z0-9-]+$ ]]
[[ "$minimum_elapsed" =~ ^[0-9]+$ ]]
[[ "$expected_main" =~ ^[0-9a-f]{40}$ ]]
test -d "$runtime"
git -C "$gov_repo" rev-parse --git-dir >/dev/null
for path in "$network_auditor" "$data_auditor" "$resource_auditor" \
  "$harness" "$qmdb_verifier"; do
  test -x "$path"
done
for path in "$archive" "$network" "$data" "$resource" "$output" "$failure"; do
  test ! -e "$path"
done

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

assert_state() {
  local item remote
  test "$(sha256 "$network_auditor")" = "$expected_network_sha"
  test "$(sha256 "$data_auditor")" = "$expected_data_sha"
  test "$(sha256 "$resource_auditor")" = "$expected_resource_sha"
  test "$(sha256 "$harness")" = "$expected_harness_sha"
  test "$(sha256 "$qmdb_verifier")" = "$expected_qmdb_sha"
  for item in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$item"
    kill -0 "$(<"$item")"
  done
  for item in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json"; do
    test ! -s "$item"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | \
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected_main"
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --argjson status "$status" --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_milestone_supplemental_audit_failure",
      status:"FAIL",label:$label,statusCode:$status,line:$line,command:$command}' \
    >"$failure"
  exit "$status"
}
trap on_error ERR

assert_state
if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
    --arg network_sha "$expected_network_sha" --arg data_sha "$expected_data_sha" \
    --arg resource_sha "$expected_resource_sha" \
    --argjson milestone_present "$(test -s "$milestone" && echo true || echo false)" \
    '{at:$at,event:"gov5_milestone_supplemental_waiter_preflight",
      status:"PASS",label:$label,networkAuditorSha256:$network_sha,
      dataAuditorSha256:$data_sha,resourceAuditorSha256:$resource_sha,
      milestonePresent:$milestone_present,nodesAlive:true,noFailureEvidence:true,
      mutationPerformed:false}'
  exit 0
fi

while ! test -s "$milestone"; do
  assert_state
  sleep 60
done
jq -e --arg label "$label" --argjson elapsed "$minimum_elapsed" '
  .status=="PASS" and .label==$label and .acceptanceRelaxed==false and
  .soak.elapsedSeconds>=$elapsed and .soak.zeroTransactionRequired==true and
  .soak.maximumLag<=1 and .transactionsSent==0 and
  .failureEvidencePresent==false and .equivocations.total==0
' "$milestone" >/dev/null
test -s "$resource_snapshot"
assert_state

N42_NETWORK_AUDIT_LABEL="$label" \
  "$network_auditor" "$runtime" "$network" >/dev/null
N42_QUAL_RUNTIME="$runtime" N42_QUAL_QMDB_PROOF_VERIFY="$qmdb_verifier" \
  "$harness" archive-rpc-parity http://127.0.0.1:28501 \
  http://127.0.0.1:29545 "$archive"
"$data_auditor" "$runtime" "$gov_repo" "$expected_main" "$data" 0x11 >/dev/null
"$resource_auditor" "$resource_snapshot" "$resource" "$minimum_elapsed" \
  86400 1048576 >/dev/null

jq -e -s '
  length==12 and
  (map(select(.event=="archive_qmdb_reference_parity" and
    .govRustProofRootsExact and .govRustProofBytesExact and
    .govRustProofsOfflineVerified))|length)==1 and
  (map(select(.event=="archive_rpc_parity" and .govRustRpcExact and
    .qmdbProofRootExact and .qmdbProofOfflineVerified))|length)==11
' "$archive" >/dev/null
jq -e '.status=="PASS" and .consensusNetworkConnectedAndQuorate and
  .rustConsensusSockets.allFiveEstablished and .authenticatedValidatorPeerCount==5 and
  .quorumEvidence.connectedValidatorPeers==5 and
  .directPushEvidence.directValidatorPeers==5 and
  .allSixCommittedBlockIdentityExact and .equivocations.total==0' "$network" >/dev/null
jq -e '.status=="PASS" and .latestAndPendingNonce=="0x11" and
  .allFiveTxindexRangesAbsent and .genesisAndCopiedHeadSixEndpointExact and
  .liveSixEndpointIdentityExact and .dataRecopyOrRegenerationRequired==false' \
  "$data" >/dev/null
jq -e --argjson elapsed "$minimum_elapsed" '
  .status=="PASS" and .elapsedSeconds>=$elapsed and .singleProcess and
  .rssKiB.projectionWithinLimit and .threads.max<=.threads.limit and
  .fileDescriptors.max<=.fileDescriptors.limit and
  .resourceProjectionWithin24hBudget' "$resource" >/dev/null
assert_state

temporary="$(mktemp "$runtime/evidence/.supplemental.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$label" \
  --arg milestone_sha "$(sha256 "$milestone")" \
  --arg archive_sha "$(sha256 "$archive")" --arg network_sha "$(sha256 "$network")" \
  --arg data_sha "$(sha256 "$data")" --arg resource_sha "$(sha256 "$resource")" \
  --slurpfile network "$network" --slurpfile data "$data" \
  --slurpfile resource "$resource" '
  {at:$at,event:"gov5_milestone_supplemental_audit",status:"PASS",
   label:$label,acceptanceRelaxed:false,mutationPerformed:false,
   evidenceSha256:{milestone:$milestone_sha,archiveQmdb:$archive_sha,
     networkMatrix:$network_sha,data905Compatibility:$data_sha,
     resourceTrend:$resource_sha},network:$network[0],data905:$data[0],
   resourceTrend:$resource[0],archiveAndQmdbParityExact:true,
   networkConsensusExact:true,data905CompatibilityExact:true,
   resourceTrendWithin24hBudget:true,noFailureEvidence:true}' >"$temporary"
jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .mutationPerformed==false and .archiveAndQmdbParityExact and
  .networkConsensusExact and .data905CompatibilityExact and
  .resourceTrendWithin24hBudget and .noFailureEvidence' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
