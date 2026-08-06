#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_TOTAL_RUNTIME:?runtime is required}"
interop_repo="${N42_TOTAL_INTEROP_REPO:?interop repository is required}"
gov_repo="${N42_TOTAL_GOV_REPO:?Gov5 repository is required}"
n42_repo="${N42_TOTAL_N42_REPO:?N42 repository is required}"
reth_repo="${N42_TOTAL_RETH_REPO:?Reth repository is required}"
expected_self_sha="${N42_TOTAL_EXPECTED_SELF_SHA:?self SHA-256 is required}"
expected_interop="${N42_TOTAL_INTEROP_COMMIT:?interop commit is required}"
expected_gov_main="${N42_TOTAL_GOV_MAIN:?Gov5 main commit is required}"
expected_gov_candidate="${N42_TOTAL_GOV_CANDIDATE:?Gov5 candidate commit is required}"
expected_n42="${N42_TOTAL_N42_COMMIT:?N42 commit is required}"
expected_reth="${N42_TOTAL_RETH_COMMIT:?Reth commit is required}"
expected_verifier_sha="${N42_TOTAL_VERIFIER_SHA:?verifier SHA-256 is required}"
expected_finalizer_sha="${N42_TOTAL_FINALIZER_SHA:?finalizer SHA-256 is required}"
expected_harness_sha="${N42_TOTAL_HARNESS_SHA:?harness SHA-256 is required}"
expected_rust_binary_sha="${N42_TOTAL_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
expected_gov_binary_sha="${N42_TOTAL_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
ports=(28501 28502 28503 28504 28505 29545)
evidence="$runtime/evidence"
summary="$evidence/gov5-906-final-qualification.json"
independent="$evidence/gov5-906-independent-final-verification.json"
formal="$evidence/mixed-soak-24h.jsonl"
formal_audit="$evidence/mixed-soak-24h-audit.json"
resources="$evidence/rust-resource-24h.jsonl"
resource_audit="$evidence/rust-resource-24h-audit.json"
stable="$evidence/official-reth-stable-monitor.jsonl"
copy_manifest="$evidence/runtime38-stopped-copy-manifest.json"
static_boundary="$evidence/runtime38-static-boundary-v2.json"
data_905="$evidence/runtime38-preflight-905-data-compat.json"
network="$evidence/runtime38-network-consensus-matrix.json"
archive_preflight="$evidence/runtime38-preflight-archive-qmdb-parity.jsonl"
transaction_preflight="$evidence/runtime38-transaction-preflight.jsonl"
runtime37_exclusion="$evidence/runtime37-final-log-warning-exclusion.json"
strict_producer="$evidence/runtime38-strict24h-six-producer-full-range.json"
strict_producer_linkage="$evidence/runtime38-strict24h-six-producer-linkage.json"
latest="$evidence/latest-reth-final-qualification.json"
latest_independent="$evidence/latest-reth-independent-final-verification.json"
output="${N42_TOTAL_OUTPUT:-$evidence/runtime38-goal-completion.json}"
compat_output="${N42_TOTAL_COMPAT_OUTPUT:-$evidence/gov5-906-goal-completion-audit-v4.json}"
failure="${N42_TOTAL_FAILURE:-$evidence/runtime38-goal-completion-failure.json}"
verifier="$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh"
expected_genesis=0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec
expected_905=0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82
expected_security=0x7ccd33002b040389eb0627fca27ef361e330234f85091b016c5e3c4256332407

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

remote_head() {
  local repo="$1" ref="$2"
  git -C "$repo" ls-remote origin "$ref" | awk 'NR==1{print $1}'
}

assert_pushed() {
  local repo="$1" expected="$2" branch
  test "$(git -C "$repo" rev-parse HEAD)" = "$expected"
  test -z "$(git -C "$repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$repo" branch --show-current)"
  test -n "$branch"
  test "$(remote_head "$repo" "refs/heads/$branch")" = "$expected"
}

assert_nodes() {
  local file pid command
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file"
    pid="$(<"$file")"
    kill -0 "$pid"
    command="$(ps -p "$pid" -o command=)"
    [[ "$command" == "$runtime/geth-live "* ]]
  done
  file="$runtime/pids/rust.pid"
  test -s "$file"
  pid="$(<"$file")"
  kill -0 "$pid"
  command="$(ps -p "$pid" -o command=)"
  [[ "$command" == "$runtime/n42-node node "* ]]
}

assert_no_failures() {
  local path
  for path in \
    "$evidence/gov5-906-finalizer-failures.jsonl" \
    "$evidence/gov5-906-finalizer-resume-failures.jsonl" \
    "$evidence/gov5-906-independent-final-verification-failure.json" \
    "$evidence/runtime38-goal-completion-failure.json" \
    "$evidence/gov5-current-main-fail-close-guardian-failure.json" \
    "$evidence/runtime38-one-hour-six-producer-failure.json" \
    "$evidence/runtime38-strict24h-six-producer-failure.json" \
    "$evidence/runtime38-one-hour-supplemental-audit-failure.json" \
    "$evidence/runtime38-strict24h-supplemental-audit-failure.json"; do
    test ! -s "$path"
  done
  for path in "$evidence"/gov5-906-*-milestone-failure.json \
    "$evidence"/runtime38-*-closed-deep-audit-failure.json; do
    test ! -e "$path" || test ! -s "$path"
  done
}

assert_live_boundaries() {
  local port expected_identity="" identity exact=false attempt
  for port in "${ports[@]}"; do
    test "$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')" = \
      "$expected_genesis"
    test "$(rpc "$port" eth_getBlockByNumber '["0x169bd",false]' | jq -er '.result.hash')" = \
      "$expected_905"
    test "$(rpc "$port" eth_getBlockByNumber '["0x18637",false]' | jq -er '.result.hash')" = \
      "$expected_security"
    test "$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')" = 0x22
    test "$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')" = 0x22
  done
  for attempt in $(seq 1 30); do
    expected_identity=""
    exact=true
    for port in "${ports[@]}"; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' | \
        jq -ecS '.result|{number,hash,stateRoot,receiptsRoot,transactionsRoot}')"
      if test -z "$expected_identity"; then
        expected_identity="$identity"
      elif test "$identity" != "$expected_identity"; then
        exact=false
        break
      fi
    done
    test "$exact" = true && break
    sleep 1
  done
  test "$exact" = true
  printf '%s\n' "$expected_identity"
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"runtime38_goal_finalizer_failure",status:"FAIL",
      statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}

test ! -e "$output"
test ! -e "$compat_output"
test ! -e "$latest"
test ! -e "$latest_independent"
test ! -e "$failure"
trap on_error ERR

required=(
  "$summary" "$independent" "$formal" "$formal_audit" "$resources"
  "$resource_audit" "$stable" "$copy_manifest" "$static_boundary" "$data_905"
  "$network" "$archive_preflight" "$transaction_preflight" "$runtime37_exclusion"
  "$strict_producer" "$strict_producer_linkage" "$verifier"
  "$evidence/gov5-906-one-hour-milestone.json"
  "$evidence/gov5-906-six-hour-milestone.json"
  "$evidence/gov5-906-twelve-hour-milestone.json"
  "$evidence/gov5-906-strict24h-milestone.json"
  "$evidence/runtime38-one-hour-six-producer-full-range.json"
  "$evidence/runtime38-one-hour-supplemental-audit.json"
  "$evidence/runtime38-strict24h-supplemental-audit.json"
  "$evidence/runtime38-one-hour-closed-deep-audit.json"
  "$evidence/runtime38-twelve-hour-closed-deep-audit.json"
  "$evidence/runtime38-strict24h-closed-deep-audit.json"
)
for path in "${required[@]}"; do test -s "$path"; done

assert_no_failures
assert_nodes
test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$verifier")" = "$expected_verifier_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" = \
  "$expected_finalizer_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" = \
  "$expected_harness_sha"
test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary_sha"
test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary_sha"
test "$(sha256 "$runtime37_exclusion")" = \
  c1f6d72cfe323dfd6273e4e4aa5d6d60fd33b0f049c163dd5d3b3288e4c0e289

jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .correctedPostBurstFailure==null and
  .transactionBurst.transactions==17 and
  .transactionBurst.resumedFromFinalizedTransactionsOnly==false and
  .transactionBurst.noTransactionsResentDuringResume==false and
  .transactionBurst.endpointCount==6 and .transactionBurst.allConfiguredEndpointsExact and
  .transactionBurst.receiptAndLogParity and .transactionBurst.stateAndStorageParity and
  .soakAudit.elapsedSeconds>=86400 and .soakAudit.zeroTransactionRequired and
  .gov5UpstreamAudit.elapsedSeconds>=86400 and
  .postBurstAudit.elapsedSeconds>=600 and .postRestartAudit.elapsedSeconds>=600 and
  (.restart|length)==2 and .restart[1].pidAfter!=.restart[0].pidBefore and
  .archiveParityPostBurst and .rustLeaderAudit.expectedLeaderSlotsExact and
  .timeoutRecoveryAudit.pendingTimeouts==0 and
  .runtimeLogAudit.warningPartitionExact and .runtimeLogAudit.unexpectedWarnings==0 and
  .runtimeLogAudit.criticalSignals==0 and
  .rustResourceAudit.elapsedSeconds>=86400 and .zeroEquivocations' "$summary" >/dev/null
jq -e --arg sha "$(sha256 "$summary")" '.status=="PASS" and
  .summarySha256==$sha and .allEvidenceHashesRecomputedExact and
  .independentRawAuditsReexecuted and .liveArchiveParityReexecuted and
  .finalSenderNonce=="0x22" and .transactionsFinalized==17' "$independent" >/dev/null

jq -e '.status=="PASS" and .elapsedSeconds>=86400 and
  .zeroTransactionRequired and .maximumLag<=6' "$formal_audit" >/dev/null
jq -e '.status=="PASS" and .elapsedSeconds>=86400 and .singleProcess and
  .logicalCountersMonotonic' "$resource_audit" >/dev/null
jq -e -s 'length>=2 and all(.[];.remoteReachable and .baselineExact and
  .expected=="v2.4.1" and .latest=="v2.4.1") and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)>=86400)' "$stable" >/dev/null

jq -e '.status=="PASS" and .files==141 and (.entries|length)==141 and
  .allPathsSizesAndHashesExact and .sourceManifestSha256==.targetManifestSha256' \
  "$copy_manifest" >/dev/null
jq -e --arg self "$expected_self_sha" '.status=="PASS" and
  .copiedData.initialSourceAndTargetExact and
  .staticGov5Data.filesChecked==24 and .staticGov5Data.allCurrentHashesMatchInitialCopy and
  .frozenTools.totalGoalVerifierSha256==$self and
  .binaries.gov5Sha256=="310d472afb1738bc06a8288e366bd2f068fec0e814902ed156cc33ab8b77a5df" and
  .binaries.rustSha256=="e3ce3278e9be89418f726f41f9d1f0814cfddba7d869407f30d3ec39a519f533"' \
  "$static_boundary" >/dev/null
jq -e '.status=="PASS" and .dataRecopyOrRegenerationRequired==false and
  .genesisAndCopiedHeadSixEndpointExact and .liveSixEndpointIdentityExact' "$data_905" >/dev/null
jq -e '.status=="PASS" and .rustClientVersionExact and
  .allSixCommittedBlockIdentityExact and .authenticatedValidatorPeerCount==5 and
  .equivocations.total==0' "$network" >/dev/null
jq -e -s 'length==12 and
  (map(select(.event=="archive_qmdb_reference_parity" and .govRustProofRootsExact and
    .govRustProofBytesExact and .govRustProofsOfflineVerified))|length)==1 and
  (map(select(.event=="archive_rpc_parity" and .govRustRpcExact and
    .qmdbProofRootExact and .qmdbProofOfflineVerified))|length)==11' \
  "$archive_preflight" >/dev/null
jq -e -s 'length==1 and .[0].event=="p4_transaction_burst_preflight" and
  .[0].expectedNonce=="0x11" and .[0].allConfiguredEndpointNoncesExact and
  .[0].transactionsSent==0' "$transaction_preflight" >/dev/null
jq -e '.status=="EXCLUDED" and .acceptanceRelaxed==false and
  .frozenRustLog.repeatedMissingLeaderWarnings==9426 and
  .sourceFix.commit=="40d1ce69bd9a2102e51e96a77b2348a6bec915ea" and
  .sourceFix.warningRateLimitedToOncePerView and
  .replacement.sourceRuntime34StoppedData and .replacement.sourceAndTargetExact and
  .replacement.runtime37AdvancedDataReused==false and
  .replacement.strictQualificationRestartsFromZero and .finalQualificationCredited==false' \
  "$runtime37_exclusion" >/dev/null

for label in one-hour six-hour twelve-hour strict24h; do
  jq -e --arg label "$label" '.status=="PASS" and .label==$label and
    .acceptanceRelaxed==false and .transactionsSent==0 and
    .failureEvidencePresent==false and .equivocations.total==0' \
    "$evidence/gov5-906-$label-milestone.json" >/dev/null
done
for path in \
  "$evidence/runtime38-one-hour-closed-deep-audit.json" \
  "$evidence/runtime38-twelve-hour-closed-deep-audit.json" \
  "$evidence/runtime38-strict24h-closed-deep-audit.json"; do
  jq -e '.status=="PASS" and .acceptanceRelaxed==false and
    .transactionsSent==0 and .failureEvidencePresent==false and
    .runtimeLogs.unexpectedWarnings==0 and .runtimeLogs.criticalSignals==0 and
    .timeoutRecovery.pendingTimeouts==0 and
    .staticBoundary.staticGov5Data.allCurrentHashesMatchInitialCopy' "$path" >/dev/null
done
for path in \
  "$evidence/runtime38-one-hour-supplemental-audit.json" \
  "$evidence/runtime38-strict24h-supplemental-audit.json"; do
  jq -e '.status=="PASS" and .acceptanceRelaxed==false and
    .archiveAndQmdbParityExact and .networkConsensusExact and
    .data905CompatibilityExact and .resourceTrendWithin24hBudget and
    .noFailureEvidence' "$path" >/dev/null
done
for path in "$evidence/runtime38-one-hour-six-producer-full-range.json" \
  "$strict_producer"; do
  jq -e '.status=="PASS" and .allSixEndpointSequencesExact and
    .parentChainContinuous and .expectedProducerSlotsExact and
    .allProducerCountsBalanced and .zeroTransactions and (.mutationPerformed|not)' \
    "$path" >/dev/null
done
jq -e '.status=="PASS" and .historicalWindowOnly and
  .postSoakTransactionsCannotAlterAuditedHistory and (.mutationPerformed|not)' \
  "$strict_producer_linkage" >/dev/null

assert_pushed "$interop_repo" "$expected_interop"
assert_pushed "$gov_repo" "$expected_gov_candidate"
assert_pushed "$n42_repo" "$expected_n42"
assert_pushed "$reth_repo" "$expected_reth"
test "$(remote_head "$gov_repo" refs/heads/main)" = "$expected_gov_main"

live_identity="$(assert_live_boundaries)"
rpc 29545 n42_consensusStatus '[]' | jq -e \
  '.result.hasCommittedQc and .result.validatorCount==7' >/dev/null
rpc 29545 n42_equivocations '[]' | jq -e \
  '.result.total==0 and (.result.evidence|length)==0' >/dev/null
assert_nodes

env N42_QUAL_RUNTIME="$runtime" N42_VERIFY_REPO="$interop_repo" \
  N42_QUAL_GOV_REPO="$gov_repo" N42_QUAL_DEPS_REPO="$n42_repo" \
  N42_QUAL_RETH_REPO="$reth_repo" N42_QUAL_PAIRED_RETH_REPO="$reth_repo" \
  N42_VERIFY_EXPECTED_SELF_SHA="$expected_verifier_sha" \
  N42_VERIFY_GOV_UPSTREAM="$expected_gov_main" \
  N42_VERIFY_GOV_CANDIDATE="$expected_gov_candidate" \
  N42_VERIFY_DEPS_HEAD="$expected_n42" N42_VERIFY_RETH_HEAD="$expected_reth" \
  N42_VERIFY_GOV_BINARY_SHA="$expected_gov_binary_sha" \
  N42_VERIFY_RUST_BINARY_SHA="$expected_rust_binary_sha" \
  N42_VERIFY_FINALIZER_SHA="$expected_finalizer_sha" \
  N42_VERIFY_HARNESS_SHA="$expected_harness_sha" "$verifier" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" --arg summary "$summary" \
  --arg summary_sha "$(sha256 "$summary")" --arg independent "$independent" \
  --arg independent_sha "$(sha256 "$independent")" --arg identity "$live_identity" \
  --arg n42 "$expected_n42" --arg reth "$expected_reth" \
  '{at:$at,event:"latest_reth_final_qualification",status:"PASS",rethVersion:"2.4.1",
    rethCommit:$reth,n42Commit:$n42,strict24hSharedWithGov5Qualification:true,
    strictZeroTransactionSoakSecondsAtLeast86400:true,sixProducerRotationExact:true,
    transactionsFinalized:17,archiveAndQmdbParityExact:true,
    controlledRustRestartRejoined:true,postRestartStabilityExact:true,
    finalLiveSixEndpointIdentity:$identity,summary:{path:$summary,sha256:$summary_sha},
    independentVerification:{path:$independent,sha256:$independent_sha}}' >"$latest.pending"
mv "$latest.pending" "$latest"

jq -nc --arg at "$(date -u +%FT%TZ)" --arg latest "$latest" \
  --arg latest_sha "$(sha256 "$latest")" --arg verifier_sha "$expected_verifier_sha" \
  '{at:$at,event:"latest_reth_independent_final_verification",status:"PASS",
    latestQualification:$latest,latestQualificationSha256:$latest_sha,
    verifierSha256:$verifier_sha,strictIndependentVerificationReused:false,
    verifierReexecutedAfterFinalQualification:true,liveBoundariesReexecuted:true}' \
  >"$latest_independent.pending"
mv "$latest_independent.pending" "$latest_independent"

jq -nc --arg at "$(date -u +%FT%TZ)" --arg runtime "$runtime" \
  --arg interop "$expected_interop" --arg gov_main "$expected_gov_main" \
  --arg gov_candidate "$expected_gov_candidate" --arg n42 "$expected_n42" \
  --arg reth "$expected_reth" --arg summary "$summary" \
  --arg summary_sha "$(sha256 "$summary")" --arg independent "$independent" \
  --arg independent_sha "$(sha256 "$independent")" --arg identity "$live_identity" \
  --arg exclusion_sha "$(sha256 "$runtime37_exclusion")" \
  '{at:$at,event:"runtime38_goal_completion",status:"PASS",acceptanceRelaxed:false,
    objectiveRequirementsExtendedClosure:true,runtime:$runtime,
    commits:{interop:$interop,gov5Main:$gov_main,gov5Candidate:$gov_candidate,n42:$n42,reth:$reth},
    officialRethStable:"v2.4.1",gov5Version:"5.7.906",
    genesis905AndSecurityBoundariesExact:true,stoppedDataCopyExact:true,
    dataRecopyOrRegenerationRequired:false,liveSixEndpointIdentity:$identity,
    mixedClientConsensusExact:true,rustLeaderProductionExact:true,
    strictZeroTransactionSoakSecondsAtLeast86400:true,transactionBurstFinalized:17,
    freshTransactionBurstNoResume:true,archiveAndQmdbParityPostBurst:true,
    rustRestartRejoinAndPostWindowExact:true,resourceWindowSecondsAtLeast86400:true,
    zeroEquivocations:true,milestonesHours:[1,6,12,24],
    fullSixProducerHistoricalWindowExact:true,
    runtime37:{excluded:true,finalQualificationCredited:false,
      exclusionEvidenceSha256:$exclusion_sha},
    summary:{path:$summary,sha256:$summary_sha},
    independentVerification:{path:$independent,sha256:$independent_sha},
    allRequiredEvidenceIndependentlyReverified:true,sourcesCommittedAndPushed:true}' \
  >"$output.pending"
mv "$output.pending" "$output"
cp "$output" "$compat_output"
cat "$output"
