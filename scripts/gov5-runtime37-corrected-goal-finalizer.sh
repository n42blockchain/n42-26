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
expected_prior_failure_sha="${N42_TOTAL_PRIOR_FAILURE_SHA:?prior failure SHA-256 is required}"
expected_burst_correction_sha="${N42_TOTAL_BURST_CORRECTION_SHA:?burst correction SHA-256 is required}"
expected_controller_recovery_sha="${N42_TOTAL_CONTROLLER_RECOVERY_SHA:?controller recovery SHA-256 is required}"
expected_static_sha="${N42_TOTAL_STATIC_SHA:?corrected static boundary SHA-256 is required}"
ports=(28501 28502 28503 28504 28505 29545)
evidence="$runtime/evidence"
summary="$evidence/gov5-906-final-qualification.json"
independent="$evidence/gov5-906-independent-final-verification.json"
formal="$evidence/mixed-soak-24h.jsonl"
formal_audit="$evidence/mixed-soak-24h-audit.json"
resources="$evidence/rust-resource-24h.jsonl"
resource_audit="$evidence/rust-resource-24h-audit.json"
stable="$evidence/official-reth-stable-monitor.jsonl"
static="$evidence/runtime37-latest-c0a146-static-boundary-v5.json"
copied="$evidence/runtime37-stopped-copy-manifest.json"
data_905="$evidence/runtime37-preflight-905-data-compat.json"
network="$evidence/runtime37-latest-c0a146-network-consensus-matrix.json"
prior_failure="$evidence/gov5-906-finalizer-failures.jsonl"
resume_failure="$evidence/gov5-906-finalizer-resume-failures.jsonl"
burst_correction="$evidence/gov5-906-post-burst-correction.json"
controller_recovery="$evidence/runtime37-post-burst-controller-recovery.json"
correction_waiter_failure="$evidence/runtime37-latest-c0a146-formal-15m-resource-correction-v2-failure.json"
independent_failure="$evidence/gov5-906-independent-final-verification-failure.json"
output="${N42_TOTAL_OUTPUT:-$evidence/runtime37-corrected-goal-completion.json}"
compat_output="${N42_TOTAL_COMPAT_OUTPUT:-$evidence/gov5-906-goal-completion-audit-v3.json}"
failure="${N42_TOTAL_FAILURE:-$evidence/runtime37-corrected-goal-completion-failure.json}"
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
  git -C "$repo" ls-remote origin "$ref" | awk 'NR == 1 {print $1}'
}

assert_pushed() {
  local repo="$1" expected="$2" branch
  test "$(git -C "$repo" rev-parse HEAD)" = "$expected"
  test -z "$(git -C "$repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$repo" branch --show-current)"
  test -n "$branch"
  test "$(remote_head "$repo" "refs/heads/$branch")" = "$expected"
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"runtime37_corrected_goal_finalizer_failure",status:"FAIL",
      statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}

test ! -e "$output"
test ! -e "$compat_output"
test ! -e "$failure"
trap on_error ERR

for path in "$summary" "$independent" "$formal" "$formal_audit" "$resources" \
  "$resource_audit" "$stable" "$static" "$copied" "$data_905" "$network" \
  "$prior_failure" "$burst_correction" "$controller_recovery" \
  "$correction_waiter_failure" "$verifier"; do
  test -s "$path"
done
test ! -s "$resume_failure"
test ! -s "$independent_failure"
test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$verifier")" = "$expected_verifier_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" = \
  "$expected_finalizer_sha"
test "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" = \
  "$expected_harness_sha"
test "$(sha256 "$prior_failure")" = "$expected_prior_failure_sha"
test "$(sha256 "$burst_correction")" = "$expected_burst_correction_sha"
test "$(sha256 "$controller_recovery")" = "$expected_controller_recovery_sha"
test "$(sha256 "$static")" = "$expected_static_sha"

jq -e --arg prior "$expected_prior_failure_sha" --arg harness "$expected_harness_sha" '
  .status == "PASS" and .acceptanceRelaxed == false and
  .priorFinalizerFailure.sha256 == $prior and .priorFinalizerFailure.preserved == true and
  .tooling.newHarnessSha256 == $harness and .transactionsResent == 0 and
  .allSeventeenReceiptsExactAcrossEndpoints == true and
  .deployBlockLastBlockAndLatestStorageExact == true
' "$burst_correction" >/dev/null
jq -e --arg correction "$expected_burst_correction_sha" '
  .status == "PASS" and .acceptanceRelaxed == false and
  .burstCorrection.sha256 == $correction and
  .priorResourceCorrectionWaiterFailure.preserved == true and
  .priorResourceCorrectionWaiterFailure.controllerCascade == true and
  .priorControllers.allExited == true and .replacementControllers.allAlive == true and
  .formalWindow.elapsedSecondsAtLeast86400 == true and
  .transactionsResent == 0 and .controllerCascadeCorrected == true
' "$controller_recovery" >/dev/null
jq -e '.event == "runtime37_formal_15m_resource_correction_failure" and
  .status == "FAIL" and .command == "kill -0 \"$finalizer_pid\""' \
  "$correction_waiter_failure" >/dev/null

jq -e '.status == "PASS" and .acceptanceRelaxed == false and
  .transactionBurst.transactions == 17 and
  .transactionBurst.resumedFromFinalizedTransactionsOnly == true and
  .transactionBurst.noTransactionsResentDuringResume == true and
  .correctedPostBurstFailure.priorFailurePreserved == true and
  .soakAudit.elapsedSeconds >= 86400 and .soakAudit.zeroTransactionRequired == true and
  .gov5UpstreamAudit.elapsedSeconds >= 86400 and
  .postBurstAudit.elapsedSeconds >= 600 and .postRestartAudit.elapsedSeconds >= 600 and
  (.restart | length) == 2 and .restart[1].pidAfter != .restart[0].pidBefore and
  .rustLeaderAudit.expectedLeaderSlotsExact == true and
  .timeoutRecoveryAudit.pendingTimeouts == 0 and
  .runtimeLogAudit.unexpectedWarnings == 0 and
  .rustResourceAudit.elapsedSeconds >= 86400 and .zeroEquivocations == true' \
  "$summary" >/dev/null
jq -e --arg summary_sha "$(sha256 "$summary")" '
  .status == "PASS" and .summarySha256 == $summary_sha and
  .allEvidenceHashesRecomputedExact == true and
  .independentRawAuditsReexecuted == true and
  .liveArchiveParityReexecuted == true and .finalSenderNonce == "0x22" and
  .transactionsFinalized == 17
' "$independent" >/dev/null

jq -e '.status == "PASS" and .elapsedSeconds >= 86400 and
  .zeroTransactionRequired == true and .maximumLag <= 6' "$formal_audit" >/dev/null
jq -e '.status == "PASS" and .elapsedSeconds >= 86400 and
  .singleProcess == true and .logicalCountersMonotonic == true' "$resource_audit" >/dev/null
jq -e -s 'length >= 2 and all(.[]; .baselineExact == true and
  .remoteReachable == true and .expected == "v2.4.1" and .latest == "v2.4.1") and
  ((.[-1].at|fromdateiso8601) - (.[0].at|fromdateiso8601) >= 86400)' "$stable" >/dev/null
jq -e --arg harness "$expected_harness_sha" --arg finalizer "$expected_finalizer_sha" \
  --arg verifier "$expected_verifier_sha" --arg self "$expected_self_sha" '
  .status == "PASS" and .acceptanceRelaxed == false and
  .staticGov5Data.filesChecked == 24 and .staticGov5Data.allCurrentHashesMatchInitialCopy == true and
  .copiedData.initialSourceAndTargetExact == true and
  .copiedData.dataRecopyOrRegenerationRequired == false and
  .frozenTools.harnessSha256 == $harness and .frozenTools.finalizerSha256 == $finalizer and
  .frozenTools.independentVerifierSha256 == $verifier and .frozenTools.totalGoalVerifierSha256 == $self and
  .correction.transactionsResent == 0 and .correction.priorFailurePreserved == true and
  .binaries.gov5Sha256 == "310d472afb1738bc06a8288e366bd2f068fec0e814902ed156cc33ab8b77a5df" and
  .binaries.rustSha256 == "d639f712a87c22c2a45de29dbd895897058a8a28e4a2145061bd195d79eb6d2e"' "$static" >/dev/null
jq -e '.status == "PASS" and .files == 141 and (.entries|length) == 141 and
  .allPathsSizesAndHashesExact == true and .sourceManifestSha256 == .targetManifestSha256' "$copied" >/dev/null
jq -e '.status == "PASS" and .dataRecopyOrRegenerationRequired == false and
  .genesisAndCopiedHeadSixEndpointExact == true' "$data_905" >/dev/null
jq -e '.status == "PASS" and .rustClientVersionExact == true and
  .allSixCommittedBlockIdentityExact == true and .authenticatedValidatorPeerCount == 5 and
  .equivocations.total == 0' "$network" >/dev/null

assert_pushed "$interop_repo" "$expected_interop"
assert_pushed "$gov_repo" "$expected_gov_candidate"
assert_pushed "$n42_repo" "$expected_n42"
assert_pushed "$reth_repo" "$expected_reth"
test "$(remote_head "$gov_repo" refs/heads/main)" = "$expected_gov_main"

for port in "${ports[@]}"; do
  test "$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')" = "$expected_genesis"
  test "$(rpc "$port" eth_getBlockByNumber '["0x169bd",false]' | jq -er '.result.hash')" = "$expected_905"
  test "$(rpc "$port" eth_getBlockByNumber '["0x18637",false]' | jq -er '.result.hash')" = "$expected_security"
  test "$(rpc "$port" eth_getTransactionCount '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')" = 0x22
done
exact=false
for _ in $(seq 1 30); do
  expected_identity=""; exact=true
  for port in "${ports[@]}"; do
    identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' |
      jq -ecS '.result|{number,hash,stateRoot,receiptsRoot}')"
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
rpc 29545 n42_consensusStatus '[]' | jq -e '.result.hasCommittedQc == true and .result.validatorCount == 7' >/dev/null
rpc 29545 n42_equivocations '[]' | jq -e '.result.total == 0 and (.result.evidence|length) == 0' >/dev/null
for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
  test -s "$file"; kill -0 "$(<"$file")"
done

env N42_QUAL_RUNTIME="$runtime" N42_VERIFY_REPO="$n42_repo" \
  N42_QUAL_GOV_REPO="$gov_repo" N42_QUAL_DEPS_REPO="$n42_repo" \
  N42_QUAL_RETH_REPO="$reth_repo" N42_QUAL_PAIRED_RETH_REPO="$reth_repo" \
  N42_VERIFY_EXPECTED_SELF_SHA="$expected_verifier_sha" \
  N42_VERIFY_GOV_UPSTREAM="$expected_gov_main" N42_VERIFY_GOV_CANDIDATE="$expected_gov_candidate" \
  N42_VERIFY_DEPS_HEAD="$expected_n42" N42_VERIFY_RETH_HEAD="$expected_reth" \
  N42_VERIFY_GOV_BINARY_SHA=310d472afb1738bc06a8288e366bd2f068fec0e814902ed156cc33ab8b77a5df \
  N42_VERIFY_RUST_BINARY_SHA=d639f712a87c22c2a45de29dbd895897058a8a28e4a2145061bd195d79eb6d2e \
  N42_VERIFY_FINALIZER_SHA="$expected_finalizer_sha" N42_VERIFY_HARNESS_SHA="$expected_harness_sha" \
  N42_VERIFY_PRIOR_FINALIZER_FAILURE_SHA="$expected_prior_failure_sha" \
  N42_VERIFY_BURST_CORRECTION_SHA="$expected_burst_correction_sha" \
  "$verifier" >/dev/null

jq -nc --arg at "$(date -u +%FT%TZ)" --arg runtime "$runtime" \
  --arg interop "$expected_interop" --arg gov_main "$expected_gov_main" \
  --arg gov_candidate "$expected_gov_candidate" --arg n42 "$expected_n42" --arg reth "$expected_reth" \
  --arg summary "$summary" --arg summary_sha "$(sha256 "$summary")" \
  --arg independent "$independent" --arg independent_sha "$(sha256 "$independent")" \
  --arg correction "$burst_correction" --arg correction_sha "$expected_burst_correction_sha" \
  --arg controllers "$controller_recovery" --arg controllers_sha "$expected_controller_recovery_sha" \
  --arg identity "$expected_identity" '
  {at:$at,event:"runtime37_corrected_goal_completion",status:"PASS",acceptanceRelaxed:false,
   runtime:$runtime,commits:{interop:$interop,gov5Main:$gov_main,gov5Candidate:$gov_candidate,n42:$n42,reth:$reth},
   officialRethStable:"v2.4.1",gov5Version:"5.7.906",
   genesis905AndSecurityBoundariesExact:true,stoppedDataCopyExact:true,
   dataRecopyOrRegenerationRequired:false,liveSixEndpointIdentity:$identity,
   mixedClientConsensusExact:true,rustLeaderProductionExact:true,
   strictZeroTransactionSoakSecondsAtLeast86400:true,
   transactionBurstFinalized:17,transactionsResentDuringCorrection:0,
   archiveAndQmdbParityPostBurst:true,rustRestartRejoinAndPostWindowExact:true,
   resourceWindowSecondsAtLeast86400:true,zeroEquivocations:true,
   summary:{path:$summary,sha256:$summary_sha},
   independentVerification:{path:$independent,sha256:$independent_sha},
   burstCorrection:{path:$correction,sha256:$correction_sha,priorFailurePreserved:true},
   controllerRecovery:{path:$controllers,sha256:$controllers_sha,priorFailuresPreserved:true},
   allRequiredEvidenceIndependentlyReverified:true,sourcesCommittedAndPushed:true}' >"$output.pending"
mv "$output.pending" "$output"
cp "$output" "$compat_output"
cat "$output"
