#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME PRIMARY_REPO RESOURCE_AUDITOR EXPECTED_RESOURCE_AUDITOR_SHA}"
primary_repo="${2:?primary repository is required}"
resource_auditor="${3:?resource trend auditor is required}"
expected_resource_auditor_sha="${4:?resource trend auditor SHA-256 is required}"
preflight_only="${N42_COMPLETION_V2_PREFLIGHT_ONLY:-0}"
completion="$runtime/evidence/gov5-906-goal-completion-audit.json"
final_905="$runtime/evidence/runtime28-final-905-data-compatibility-audit.json"
producer="$runtime/evidence/runtime28-strict24h-six-producer-full-range.json"
producer_raw="${producer%.json}-raw"
producer_linkage="$runtime/evidence/runtime28-strict24h-six-producer-linkage.json"
resources="$runtime/evidence/rust-resource-24h.jsonl"
resource_trend="$runtime/evidence/runtime28-final-rust-resource-trend-audit.json"
burst="$runtime/artifacts/p4-signed-transaction-burst.json"
output="$runtime/evidence/gov5-906-goal-completion-audit-v2.json"
failure="$runtime/evidence/gov5-906-goal-completion-audit-v2-failure.json"
expected_burst_sha="6cf05cd0cfb4059c3000f589b9e77c74aa6bc14fcf8ea6b8465f9de8e63dd750"
ports=(28501 28502 28503 28504 28505 29545)

test -d "$runtime"
git -C "$primary_repo" rev-parse --git-dir >/dev/null
test -x "$resource_auditor"
test ! -e "$output"
test ! -e "$failure"
[[ "$expected_resource_auditor_sha" =~ ^[0-9a-f]{64}$ ]]

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

assert_primary_pushed() {
  local branch remote
  branch="$(git -C "$primary_repo" branch --show-current)"
  test -n "$branch"
  remote="$(git -C "$primary_repo" ls-remote origin "refs/heads/$branch" | \
    awk 'NR==1{print $1}')"
  test "$(git -C "$primary_repo" rev-parse HEAD)" = "$remote"
  test -z "$(git -C "$primary_repo" status --porcelain --untracked-files=no)"
}

assert_nodes_and_failures() {
  local item
  for item in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$item"
    kill -0 "$(<"$item")"
  done
  for item in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/official-reth-stable/latest-reth-failures.jsonl" \
    "$runtime/evidence/official-reth-stable/latest-reth-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-copied-boundary-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-final-network-verification-failure.json" \
    "$runtime/evidence/runtime28-strict24h-six-producer-full-range-failure.json" \
    "$runtime/evidence/runtime28-final-905-data-compatibility-audit-failure.json"; do
    test ! -s "$item"
  done
}

assert_burst_artifact() {
  test -s "$burst"
  test "$(sha256 "$burst")" = "$expected_burst_sha"
  jq -e '
    .chainId==1143 and .sender=="0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266" and
    (.transactions|length)==17 and .transactions[0].nonce==17 and
    .transactions[-1].nonce==33 and
    ([.transactions[].nonce]==[range(17;34)]) and
    all(.transactions[];(.raw|startswith("0x")) and
      (.hash|test("^0x[0-9a-f]{64}$")) and
      (.intendedIngress=="gov" or .intendedIngress=="rust"))
  ' "$burst" >/dev/null
}

assert_live_nonce() {
  local expected_nonce="$1" port nonce pending
  for port in "${ports[@]}"; do
    nonce="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')"
    pending="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')"
    test "$nonce" = "$expected_nonce"
    test "$pending" = "$expected_nonce"
  done
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_906_goal_completion_audit_v2_failure",status:"FAIL",
      statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

test "$(sha256 "$resource_auditor")" = "$expected_resource_auditor_sha"
assert_primary_pushed
assert_nodes_and_failures
assert_burst_artifact

if test "$preflight_only" = 1; then
  assert_live_nonce 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" \
    --arg resource_auditor "$expected_resource_auditor_sha" \
    --arg burst_sha "$expected_burst_sha" \
    --arg primary "$(git -C "$primary_repo" rev-parse HEAD)" \
    '{at:$at,event:"gov5_906_goal_completion_auditor_v2_preflight",
      status:"PASS",primaryHead:$primary,resourceAuditorSha256:$resource_auditor,
      burstArtifactSha256:$burst_sha,burstTransactions:17,
      strict24hRawProducerRequired:true,final905DataAuditRequired:true,
      finalResourceTrendRequired:true,nodesAlive:true,noFailureEvidence:true,
      latestAndPendingNonce:"0x11",completionNotClaimed:true,
      mutationPerformed:false}'
  exit 0
fi

for item in "$completion" "$final_905" "$producer" "$producer_linkage" \
  "$resources"; do
  test -s "$item"
done
jq -e '.status=="PASS" and .objectiveRequirementsIndependentlyClosed and
  .strict24hExact and .transactionsFinalized==17 and
  .controlledRestartAndCatchupExact and .latestStableRethExtraHourExact and
  .copied905BoundaryAndGenesisExact and .postRolloverNetworkExact and
  .latestAndPendingNonce=="0x22" and .noFailureEvidence' "$completion" >/dev/null
jq -e '.status=="PASS" and .latestAndPendingNonce=="0x22" and
  .allFiveProcessesAlive and .allFiveChaindataPresent and
  .allFiveTxindexRangesAbsent and .genesisAndCopiedHeadSixEndpointExact and
  .liveSixEndpointIdentityExact and .dataRecopyOrRegenerationRequired==false' \
  "$final_905" >/dev/null
jq -e '.status=="PASS" and .blocksScanned>0 and .completeCycles>0 and
  .allSixEndpointSequencesExact and .parentChainContinuous and
  .expectedProducerSlotsExact and .allProducerCountsBalanced and
  .zeroTransactions and (.producerCounts|length)==6 and
  ([.producerCounts[]]|unique|length)==1' "$producer" >/dev/null
jq -e --arg producer_sha "$(sha256 "$producer")" '
  .status=="PASS" and .producerAuditSha256==$producer_sha and
  .historicalWindowOnly and .postSoakTransactionsCannotAlterAuditedHistory and
  .mutationPerformed==false' "$producer_linkage" >/dev/null

expected_sequence="$(jq -er '.sequenceSha256' "$producer")"
expected_blocks="$(jq -er '.blocksScanned' "$producer")"
test -d "$producer_raw"
for port in "${ports[@]}"; do
  raw="$producer_raw/port-${port}.jsonl"
  test -s "$raw"
  test "$(sha256 "$raw")" = "$expected_sequence"
  test "$(wc -l <"$raw" | tr -d ' ')" = "$expected_blocks"
done

if ! test -s "$resource_trend"; then
  "$resource_auditor" "$resources" "$resource_trend" 86400 86400 1048576 >/dev/null
fi
jq -e '.status=="PASS" and .elapsedSeconds>=86400 and .singleProcess and
  .timestampsStrictlyIncreasing and .headsMonotonic and
  .rssKiB.max<=.rssKiB.limit and .rssKiB.projectionWithinLimit and
  .threads.max<=.threads.limit and
  .fileDescriptors.max<=.fileDescriptors.limit and
  .resourceProjectionWithin24hBudget' "$resource_trend" >/dev/null

assert_primary_pushed
assert_nodes_and_failures
assert_burst_artifact
assert_live_nonce 0x22

temporary="$(mktemp "$runtime/evidence/.goal-completion-v2.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" \
  --arg primary "$(git -C "$primary_repo" rev-parse HEAD)" \
  --arg completion_sha "$(sha256 "$completion")" \
  --arg final_905_sha "$(sha256 "$final_905")" \
  --arg producer_sha "$(sha256 "$producer")" \
  --arg linkage_sha "$(sha256 "$producer_linkage")" \
  --arg resource_sha "$(sha256 "$resource_trend")" \
  --arg burst_sha "$expected_burst_sha" \
  --arg sequence_sha "$expected_sequence" \
  --argjson producer "$(jq -c '{startHeight,endHeight,blocksScanned,
    completeCycles,producerCounts}' "$producer")" \
  --argjson resource "$(jq -c '{samples,elapsedSeconds,pid,headGrowth,rssKiB,
    threads,fileDescriptors,growth}' "$resource_trend")" '
  {at:$at,event:"gov5_906_goal_completion_audit_v2",status:"PASS",
   acceptanceRelaxed:false,mutationPerformed:false,primaryHead:$primary,
   evidenceSha256:{baseCompletion:$completion_sha,final905Data:$final_905_sha,
     strict24hSixProducer:$producer_sha,strict24hLinkage:$linkage_sha,
     finalResourceTrend:$resource_sha,burstArtifact:$burst_sha},
   strict24hProducerSequenceSha256:$sequence_sha,
   strict24hSixProducer:$producer,finalResourceTrend:$resource,
   burstTransactions:17,latestAndPendingNonce:"0x22",
   strict24hRawSixEndpointSequencesExact:true,
   final905DataCompatibilityExact:true,finalResourceTrendExact:true,
   originalCompletionAuditExact:true,noFailureEvidence:true,
   objectiveRequirementsExtendedClosure:true}' >"$temporary"
jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .mutationPerformed==false and .burstTransactions==17 and
  .latestAndPendingNonce=="0x22" and
  .strict24hRawSixEndpointSequencesExact and .final905DataCompatibilityExact and
  .finalResourceTrendExact and .originalCompletionAuditExact and
  .noFailureEvidence and .objectiveRequirementsExtendedClosure' \
  "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
