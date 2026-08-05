#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_TOTAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-37-gov5-906-latest-609cf1-rustsec}"
gov_repo="${N42_TOTAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
n42_repo="${N42_TOTAL_N42_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/n42-26}"
reth_repo="${N42_TOTAL_RETH_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/reth}"
interop_repo="${N42_TOTAL_INTEROP_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/n42-26}"
expected_self_sha="${N42_TOTAL_EXPECTED_SELF_SHA:?total finalizer SHA-256 is required}"
expected_interop="${N42_TOTAL_INTEROP_COMMIT:?interop tooling commit is required}"
preflight_only="${N42_TOTAL_PREFLIGHT_ONLY:-0}"

expected_gov_main="c0a14646813c10c6883a38d6f20e82ba96cf183a"
expected_gov_candidate="9ae0421ce829e6bfd54c9bd9257c21c2602e2b60"
expected_n42="ce4e88ccfe7bc845ecd57605d417a7559fbde932"
expected_reth="0fc810bae34412838bedfd8dc2f212e14e915e5d"
expected_gov_binary="310d472afb1738bc06a8288e366bd2f068fec0e814902ed156cc33ab8b77a5df"
expected_rust_binary="d639f712a87c22c2a45de29dbd895897058a8a28e4a2145061bd195d79eb6d2e"
expected_verifier="${N42_TOTAL_EXPECTED_VERIFIER_SHA:?independent verifier SHA-256 is required}"
expected_finalizer="${N42_TOTAL_EXPECTED_FINALIZER_SHA:?qualification finalizer SHA-256 is required}"
expected_static="${N42_TOTAL_EXPECTED_STATIC_SHA:?static boundary SHA-256 is required}"
expected_harness="210517ae2b40233a078b4a2999e07ea9bd2f6211d30d24a87eaf481473f5376b"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_905_boundary="0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82"
expected_security_boundary="0x7ccd33002b040389eb0627fca27ef361e330234f85091b016c5e3c4256332407"
expected_client="reth/v2.4.1-0fc810b/aarch64-apple-darwin"
ports=(28501 28502 28503 28504 28505 29545)

evidence="$runtime/evidence"
strict="$evidence/gov5-906-final-qualification.json"
independent="$evidence/gov5-906-independent-final-verification.json"
producer="$evidence/runtime37-latest-c0a146-strict24h-six-producer.json"
producer_linkage="$evidence/runtime37-latest-c0a146-strict24h-six-producer-linkage.json"
formal="$evidence/mixed-soak-24h.jsonl"
resources="$evidence/rust-resource-24h.jsonl"
resource_audit="$evidence/rust-resource-24h-audit.json"
upstream="$evidence/gov5-upstream-24h.jsonl"
upstream_complete="$evidence/gov5-upstream-24h-complete.json"
upstream_audit="$evidence/gov5-upstream-24h-audit.json"
stable="$evidence/official-reth-stable-monitor.jsonl"
copied="$evidence/runtime37-stopped-copy-manifest.json"
static="${N42_TOTAL_STATIC:-$evidence/runtime37-latest-c0a146-static-boundary-v3.json}"
data_905="$evidence/runtime37-preflight-905-data-compat.json"
network="$evidence/runtime37-latest-c0a146-network-consensus-matrix.json"
supplemental_15m_failure="$evidence/runtime37-latest-c0a146-formal-15m-supplemental-audit-failure.json"
supplemental_15m_correction="$evidence/runtime37-latest-c0a146-formal-15m-corrected-supplemental-audit.json"
correction_waiter_failure="$evidence/runtime37-latest-c0a146-formal-15m-resource-correction-failure.json"
correction_waiter_v2_failure="$evidence/runtime37-latest-c0a146-formal-15m-resource-correction-v2-failure.json"
controller_rebind_correction="$evidence/runtime37-finalizer-session-keeper-rebind-correction.json"
independent_harness_rebind="$evidence/runtime37-independent-verifier-harness-rebind.json"
latest_reth="$evidence/latest-reth-final-qualification.json"
output="${N42_TOTAL_OUTPUT:-$evidence/runtime37-goal-completion.json}"
compat_output="${N42_TOTAL_COMPAT_OUTPUT:-$evidence/gov5-906-goal-completion-audit-v2.json}"
failure="${N42_TOTAL_FAILURE:-$evidence/runtime37-goal-completion-failure.json}"
restart_evidence="$evidence/rust-restart-rejoin-906.jsonl"
verifier="$runtime/artifacts/scripts/verify-gov5-906-final-qualification.sh"
rechecker="$runtime/artifacts/scripts/recheck-gov5-runtime-static-boundary-v2.sh"
data_auditor="$runtime/artifacts/scripts/audit-gov5-905-data-compat.sh"
network_auditor="$runtime/artifacts/scripts/audit-gov5-mixed-network-matrix.sh"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

remote_gov_main() {
  git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}'
}

latest_reth_stable() {
  git ls-remote --tags https://github.com/paradigmxyz/reth.git 'refs/tags/v*' |
    sed -E 's#.*refs/tags/##; s/\^\{\}//' |
    rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1
}

assert_branch_pushed() {
  local repo="$1" expected="$2" branch remote
  test "$(git -C "$repo" rev-parse HEAD)" = "$expected"
  test -z "$(git -C "$repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$repo" branch --show-current)"
  test -n "$branch"
  remote="$(git -C "$repo" ls-remote origin "refs/heads/$branch" | awk 'NR==1{print $1}')"
  test "$remote" = "$expected"
}

assert_sources() {
  assert_branch_pushed "$gov_repo" "$expected_gov_candidate"
  assert_branch_pushed "$n42_repo" "$expected_n42"
  assert_branch_pushed "$reth_repo" "$expected_reth"
  assert_branch_pushed "$interop_repo" "$expected_interop"
  test "$(remote_gov_main)" = "$expected_gov_main"
}

planned_rust_restart_in_progress() {
  local event started now age
  test -s "$restart_evidence" || return 1
  event="$(tail -n 1 "$restart_evidence" | jq -er '.event')"
  test "$event" = rust_restart_started || return 1
  started="$(tail -n 1 "$restart_evidence" | jq -er '.at|fromdateiso8601')"
  now="$(date +%s)"
  age=$((now - started))
  test "$age" -ge 0 && test "$age" -le 900
}

assert_nodes() {
  local file rust_pid
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
  file="$runtime/pids/rust.pid"
  test -s "$file"
  rust_pid="$(<"$file")"
  if ! kill -0 "$rust_pid" 2>/dev/null; then
    planned_rust_restart_in_progress
  fi
}

assert_independent_harness_rebind() {
  local independent_pid
  test -s "$independent_harness_rebind"
  jq -e \
    --arg prior_rebind_sha "$(sha256 "$controller_rebind_correction")" \
    --arg verifier_sha "$expected_verifier" --arg harness_sha "$expected_harness" \
    --arg waiter_sha "$(sha256 "$runtime/artifacts/scripts/gov5-strict-independent-verifier-waiter.sh")" '
    .event=="runtime37_independent_verifier_harness_rebind" and
    .status=="PASS" and .acceptanceRelaxed==false and
    .priorRebindCorrectionSha256==$prior_rebind_sha and
    .oldIndependentWaiter.pid==55356 and
    .oldIndependentWaiter.verifierSha256=="35bfee43ecd472540c0e09a42855c103735a2f67ada43bfda88e46f727c6909b" and
    .newIndependentWaiter.verifierSha256==$verifier_sha and
    .newIndependentWaiter.waiterSha256==$waiter_sha and
    .newIndependentWaiter.harnessSha256==$harness_sha and
    .formalWindow.continuous==true and .formalWindow.maximumGapSeconds<=120 and
    .formalWindow.failedSamples==0 and .formalWindow.zeroTransactionRequired==true and
    .chainDataMutationPerformed==false and
    .nodeOrFormalMonitorMutationPerformed==false and
    .independentVerifierHarnessPinCorrected==true
  ' "$independent_harness_rebind" >/dev/null
  if ! test -s "$independent"; then
    independent_pid="$(jq -er '.newIndependentWaiter.pid' "$independent_harness_rebind")"
    kill -0 "$independent_pid"
    ps -p "$independent_pid" -o command= | rg -F 'gov5-strict-independent-verifier-waiter.sh' >/dev/null
  fi
}

assert_controller_rebind_correction() {
  local finalizer_pid
  test -s "$correction_waiter_failure"
  test -s "$controller_rebind_correction"
  jq -e '
    .event=="runtime37_formal_15m_resource_correction_failure" and
    .status=="FAIL" and .statusCode==1 and .line==56 and
    .command=="kill -0 \"$finalizer_pid\""
  ' "$correction_waiter_failure" >/dev/null
  jq -e --arg failure_sha "$(sha256 "$correction_waiter_failure")" '
    .event=="runtime37_finalizer_session_keeper_rebind_correction" and
    .status=="PASS" and .acceptanceRelaxed==false and
    .priorCorrectionWaiterFailure.sha256==$failure_sha and
    .priorCorrectionWaiterFailure.preserved==true and
    .priorCorrectionWaiterFailure.controllerOnly==true and
    .formalWindow.continuous==true and .formalWindow.maximumGapSeconds<=120 and
    .formalWindow.failedSamples==0 and .formalWindow.zeroTransactionRequired==true and
    .chainDataMutationPerformed==false and
    .nodeOrFormalMonitorMutationPerformed==false and
    .controllerRebindCorrected==true and
    .totalGoalFinalizerRelaunchRequired==true
  ' "$controller_rebind_correction" >/dev/null
  finalizer_pid="$(jq -er '.newControllers.finalizerPid' "$controller_rebind_correction")"
  kill -0 "$finalizer_pid"
  ps -p "$finalizer_pid" -o command= | rg -F 'gov5-current-qualification-finalizer.sh' >/dev/null
  assert_independent_harness_rebind
  if test -s "$supplemental_15m_correction"; then
    jq -e --arg failure_sha "$(sha256 "$correction_waiter_failure")" \
      --arg rebind_sha "$(sha256 "$controller_rebind_correction")" '
      .status=="PASS" and .priorCorrectionWaiterFailurePreserved==true and
      .priorCorrectionWaiterFailureSha256==$failure_sha and
      .controllerRebindFailureCorrected==true and
      .controllerRebindCorrectionSha256==$rebind_sha and
      .noUncorrectedFailureEvidence==true
    ' "$supplemental_15m_correction" >/dev/null
  fi
}

assert_no_uncorrected_failures() {
  local item
  for item in \
    "$evidence/gov5-current-main-fail-close-guardian-failure.json" \
    "$evidence/gov5-current-main-fail-close-guardian-v2-failure.json" \
    "$evidence/gov5-906-finalizer-failures.jsonl" \
    "$evidence/gov5-906-independent-final-verification-failure.json" \
    "$correction_waiter_v2_failure" \
    "$evidence/runtime37-latest-c0a146-strict24h-six-producer-failure.json"; do
    test ! -s "$item"
  done
  if test -s "$correction_waiter_failure"; then
    assert_controller_rebind_correction
  fi
}

assert_live() {
  local expected_nonce="$1" port identity expected exact attempt
  for port in "${ports[@]}"; do
    test "$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')" = "$expected_genesis"
    test "$(rpc "$port" eth_getBlockByNumber '["0x169bd",false]' | jq -er '.result.hash')" = "$expected_905_boundary"
    test "$(rpc "$port" eth_getBlockByNumber '["0x18637",false]' | jq -er '.result.hash')" = "$expected_security_boundary"
    test "$(rpc "$port" eth_getTransactionCount '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')" = "$expected_nonce"
    test "$(rpc "$port" eth_getTransactionCount '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')" = "$expected_nonce"
  done
  exact=false
  for attempt in $(seq 1 30); do
    expected=""; exact=true
    for port in "${ports[@]}"; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' | jq -ec '.result|{number,hash,stateRoot,receiptsRoot}')"
      if test -z "$expected"; then expected="$identity"; elif test "$identity" != "$expected"; then exact=false; break; fi
    done
    test "$exact" = true && break
    sleep 1
  done
  test "$exact" = true
  rpc 29545 n42_consensusStatus '[]' | jq -e '.result.validatorCount==7 and .result.hasCommittedQc==true' >/dev/null
  rpc 29545 n42_equivocations '[]' | jq -e '.result.total==0 and (.result.evidence|length)==0' >/dev/null
}

assert_static() {
  test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
  test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary"
  test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary"
  test "$(sha256 "$verifier")" = "$expected_verifier"
  test "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" = "$expected_finalizer"
  test "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" = "$expected_harness"
  test "$(sha256 "$static")" = "$expected_static"
  jq -e --arg verifier "$expected_verifier" \
    --arg prior "b1931cea87f9d1c104e8246cfcc0f857d5568135b8440137e446be24168b766c" \
    --arg rebind "$(sha256 "$independent_harness_rebind")" '
    .status=="PASS" and .acceptanceRelaxed==false and
    .frozenTools.independentVerifierSha256==$verifier and
    .correction.priorBaselineSha256==$prior and
    .correction.priorBaselinePreserved==true and
    .correction.independentVerifierHarnessRebindSha256==$rebind and
    .correction.chainDataMutationPerformed==false and
    .correction.nodeOrFormalMonitorMutationPerformed==false
  ' "$static" >/dev/null
  "$runtime/n42-node" --version | rg -F 'Reth Version: 2.4.1' >/dev/null
  "$runtime/n42-node" --version | rg -F "Commit SHA: $expected_reth" >/dev/null
  jq -e --arg gov_binary "$expected_gov_binary" --arg rust_binary "$expected_rust_binary" '
    .status=="PASS" and .staticGov5Data.filesChecked==24 and
    .staticGov5Data.allCurrentHashesMatchInitialCopy==true and
    .copiedData.initialSourceAndTargetExact==true and
    .binaries.gov5Sha256==$gov_binary and .binaries.rustSha256==$rust_binary
  ' "$static" >/dev/null
  jq -e '.status=="PASS" and .files==141 and (.entries|length)==141 and
    .allPathsSizesAndHashesExact==true and .sourceManifestSha256==.targetManifestSha256' "$copied" >/dev/null
  jq -e --arg main "$expected_gov_main" '.status=="PASS" and .remoteMain==$main and
    .genesisAndCopiedHeadSixEndpointExact==true and .liveSixEndpointIdentityExact==true and
    .dataRecopyOrRegenerationRequired==false and .source.activationAbsentInAllRunningGovProcesses==true and
    .source.qmdbTruncIndexAbsentInAllRunningGovProcesses==true' "$data_905" >/dev/null
  jq -e '.status=="PASS" and .rustClientVersionExact==true and
    .consensusNetworkConnectedAndQuorate==true and .allSixCommittedBlockIdentityExact==true and
    .authenticatedValidatorPeerCount==5 and .equivocations.total==0' "$network" >/dev/null
  assert_sources
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"runtime37_total_goal_finalizer_failure",status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}

assert_static
assert_nodes
assert_no_uncorrected_failures
assert_live 0x11
test "$(latest_reth_stable)" = v2.4.1

if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" --arg interop "$expected_interop" \
    '{at:$at,event:"runtime37_total_goal_finalizer_preflight",status:"PASS",scriptSha256:$self,
      interopCommit:$interop,nodesAlive:true,binariesExact:true,sourcesAndRemotesExact:true,
      genesis905AndSecurityBoundariesExact:true,staticBoundaryExact:true,
      liveSixEndpointIdentityExact:true,zeroEquivocations:true,officialRethStable:"v2.4.1",
      transactionsSent:0}'
  exit 0
fi

test ! -e "$output"
test ! -e "$compat_output"
test ! -e "$failure"
trap on_error ERR

milestone_required=()
for spec in 'formal-15m 900' 'formal-1h 3600' 'formal-4h 14400' \
  'formal-8h 28800' 'formal-12h 43200' 'formal-16h 57600' 'formal-20h 72000'; do
  label="${spec%% *}"
  milestone_required+=(
    "$evidence/gov5-906-$label-milestone.json"
    "$evidence/runtime37-$label-closed-deep-audit.json"
    "$evidence/runtime37-latest-c0a146-$label-six-producer.json"
  )
  if test "$label" = formal-15m; then
    milestone_required+=("$supplemental_15m_correction")
  else
    milestone_required+=(
      "$evidence/runtime37-latest-c0a146-$label-supplemental-audit.json"
    )
  fi
done
required=("$strict" "$independent" "$producer" "$producer_linkage" "$resource_audit" "$upstream_complete" "$upstream_audit")
required+=("${milestone_required[@]}")
while :; do
  missing=false
  for item in "${required[@]}"; do test -s "$item" || missing=true; done
  test "$missing" = false && break
  assert_nodes
  assert_no_uncorrected_failures
  test "$(remote_gov_main)" = "$expected_gov_main"
  sleep 60
done

jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .soakAudit.elapsedSeconds>=86400 and .soakAudit.maximumLag<=1 and
  .soakAudit.zeroTransactionRequired==true and
  .transactionBurst.transactions==17 and .transactionBurst.endpointCount==6 and
  .transactionBurst.allConfiguredEndpointsExact==true and
  .postBurstAudit.elapsedSeconds>=600 and .postRestartAudit.elapsedSeconds>=600 and
  .archiveParityPostBurst==true and .rustLeaderAudit.status=="PASS" and
  .rustLeaderAudit.leaderCommitLog.allVotesFivePlusFive==true and
  .timeoutRecoveryAudit.pendingTimeouts==0 and .runtimeLogAudit.unexpectedWarnings==0 and
  .runtimeLogAudit.criticalSignals==0 and .rustResourceAudit.elapsedSeconds>=86400 and
  .zeroEquivocations==true' "$strict" >/dev/null
jq -e '.status=="PASS" and .transactionsFinalized==17 and .finalSenderNonce=="0x22" and
  .independentRawAuditsReexecuted==true and .liveArchiveParityReexecuted==true' "$independent" >/dev/null
jq -e '.status=="PASS" and .startHeight>99895 and .completeCycles>0 and
  .allSixEndpointSequencesExact==true and .parentChainContinuous==true and
  .expectedProducerSlotsExact==true and .allProducerCountsBalanced==true and .zeroTransactions==true' "$producer" >/dev/null
jq -e --arg sha "$(sha256 "$producer")" '.status=="PASS" and .producerAuditSha256==$sha and
  .historicalWindowOnly==true and .postSoakTransactionsCannotAlterAuditedHistory==true' "$producer_linkage" >/dev/null
jq -e -s 'length>=2 and all(.[];.ok==true and .zeroTxRequired==1 and .lag<=1 and
  .latestSnapshotConcurrent==true and .latestSnapshotAttempts>=1) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400 and
  ([range(1;length) as $i|((.[$i].at|fromdateiso8601)-(.[$i-1].at|fromdateiso8601))<=120]|all)' "$formal" >/dev/null
jq -e -s --arg main "$expected_gov_main" 'length>=2 and
  all(.[];.remoteReachable==true and .baselineExact==true and .baseline==$main and .remoteMain==$main) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400' "$upstream" >/dev/null
jq -e '.status=="PASS" and .elapsedSeconds>=86400' "$upstream_complete" "$upstream_audit" >/dev/null
jq -e '.status=="PASS" and .elapsedSeconds>=86400 and .singleProcess==true and
  .logicalCountersMonotonic==true and .headLogAndWalCountersMonotonic==true' "$resource_audit" >/dev/null
jq -e -s 'length>=2 and all(.[];.remoteReachable==true and .baselineExact==true and
  .expected=="v2.4.1" and .latest=="v2.4.1")' "$stable" >/dev/null

for spec in 'formal-15m 900' 'formal-1h 3600' 'formal-4h 14400' \
  'formal-8h 28800' 'formal-12h 43200' 'formal-16h 57600' 'formal-20h 72000'; do
  label="${spec%% *}"
  minimum="${spec##* }"
  if test "$label" = formal-15m; then
    supplemental_path="$supplemental_15m_correction"
  else
    supplemental_path="$evidence/runtime37-latest-c0a146-$label-supplemental-audit.json"
  fi
  jq -e --arg label "$label" --argjson minimum "$minimum" '
    .status=="PASS" and .label==$label and .acceptanceRelaxed==false and
    .soak.elapsedSeconds>=$minimum and .soak.maximumLag<=1 and
    .soak.zeroTransactionRequired==true and .resources.elapsedSeconds>=$minimum and
    .gov5Upstream.elapsedSeconds>=$minimum and .rustLeaderCommitsFivePlusFive>=2 and
    .equivocations.total==0 and .transactionsSent==0 and .failureEvidencePresent==false
  ' "$evidence/gov5-906-$label-milestone.json" >/dev/null
  if test "$label" = formal-15m; then
    jq -e --arg failure_sha "$(sha256 "$supplemental_15m_failure")" '
      .status=="PASS" and .acceptanceRelaxed==false and
      .originalFailurePreserved==true and .originalFailureSha256==$failure_sha and
      .priorCorrectionWaiterFailurePreserved==true and
      .controllerRebindFailureCorrected==true and
      .measured24hResourceAudit.elapsedSeconds>=86400 and
      .archiveAndQmdbParityExact==true and .networkConsensusExact==true and
      .data905CompatibilityExact==true and .resourceTrendWithin24hBudget==true and
      .noUncorrectedFailureEvidence==true
    ' "$supplemental_path" >/dev/null
  else
    jq -e '.status=="PASS" and .acceptanceRelaxed==false and
      .archiveAndQmdbParityExact==true and .networkConsensusExact==true and
      .data905CompatibilityExact==true and .resourceTrendWithin24hBudget==true and
      .noFailureEvidence==true' \
      "$supplemental_path" >/dev/null
  fi
  jq -e '.status=="PASS" and .acceptanceRelaxed==false and
    .rustLeaders.leaderCommitLog.allVotesFivePlusFive==true and
    .timeoutRecovery.pendingTimeouts==0 and .runtimeLogs.unexpectedWarnings==0 and
    .runtimeLogs.criticalSignals==0 and
    .staticBoundary.staticGov5Data.allCurrentHashesMatchInitialCopy==true and
    .transactionsSent==0 and .failureEvidencePresent==false' \
    "$evidence/runtime37-$label-closed-deep-audit.json" >/dev/null
  jq -e '.status=="PASS" and .startHeight==99920 and .completeCycles>0 and
    .allSixEndpointSequencesExact==true and .parentChainContinuous==true and
    .expectedProducerSlotsExact==true and .allProducerCountsBalanced==true and
    .zeroTransactions==true' \
    "$evidence/runtime37-latest-c0a146-$label-six-producer.json" >/dev/null
done

audit_dir="$(mktemp -d /tmp/n42-runtime37-total.XXXXXX)"
trap 'rm -rf "$audit_dir"' EXIT
"$rechecker" "$runtime" "$static" "$audit_dir/static.json" >/dev/null
jq -e '.status=="PASS" and .staticGov5Data.filesChecked==24 and .staticGov5Data.allCurrentHashesMatchInitialCopy==true' "$audit_dir/static.json" >/dev/null
"$data_auditor" "$runtime" "$gov_repo" "$expected_gov_main" "$audit_dir/data905.json" 0x22 >/dev/null
jq -e '.status=="PASS" and .latestAndPendingNonce=="0x22" and .dataRecopyOrRegenerationRequired==false' "$audit_dir/data905.json" >/dev/null
N42_NETWORK_EXPECTED_RUST_CLIENT="$expected_client" "$network_auditor" "$runtime" "$audit_dir/network.json" >/dev/null
jq -e '.status=="PASS" and .allSixCommittedBlockIdentityExact==true and
  .consensusNetworkConnectedAndQuorate==true and .equivocations.total==0' "$audit_dir/network.json" >/dev/null

env N42_QUAL_RUNTIME="$runtime" N42_VERIFY_REPO="$n42_repo" N42_QUAL_GOV_REPO="$gov_repo" \
  N42_QUAL_DEPS_REPO="$n42_repo" N42_QUAL_RETH_REPO="$reth_repo" N42_QUAL_PAIRED_RETH_REPO="$reth_repo" \
  N42_VERIFY_EXPECTED_SELF_SHA="$expected_verifier" N42_VERIFY_GOV_UPSTREAM="$expected_gov_main" \
  N42_VERIFY_GOV_CANDIDATE="$expected_gov_candidate" N42_VERIFY_DEPS_HEAD="$expected_n42" \
  N42_VERIFY_RETH_HEAD="$expected_reth" N42_VERIFY_GOV_BINARY_SHA="$expected_gov_binary" \
  N42_VERIFY_RUST_BINARY_SHA="$expected_rust_binary" N42_VERIFY_FINALIZER_SHA="$expected_finalizer" \
  N42_VERIFY_HARNESS_SHA="$expected_harness" \
  "$verifier" >"$audit_dir/independent.json"
jq -e '.status=="PASS" and .transactionsFinalized==17' "$audit_dir/independent.json" >/dev/null

assert_static
assert_nodes
assert_no_uncorrected_failures
assert_live 0x22
test "$(latest_reth_stable)" = v2.4.1

head_stats="$(jq -cs '{samples:length,firstAt:.[0].at,lastAt:.[-1].at,
  elapsedSeconds:((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)),
  startHeight:.[0].commonHeight,endHeight:.[-1].commonHeight,
  blockGrowth:(.[-1].commonHeight-.[0].commonHeight),maximumLag:([.[].lag]|max),
  failures:([.[]|select(.ok!=true)]|length)}' "$formal")"
latest_head="$(rpc 29545 eth_getBlockByNumber '["latest",false]' | jq -ec '.result|{number,hash,stateRoot,receiptsRoot,transactionsRoot}')"
consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"

latest_tmp="$(mktemp "$evidence/.latest-reth-final.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg reth "$expected_reth" --arg binary "$expected_rust_binary" \
  --arg stable_sha "$(sha256 "$stable")" --argjson head_audit "$head_stats" \
  '{at:$at,event:"latest_reth_final_qualification",status:"PASS",rethVersion:"2.4.1",
    rethCommit:$reth,rustBinarySha256:$binary,officialStableTag:"v2.4.1",officialStableTagExact:true,
    headAudit:$head_audit,strict24hSharedWithGov5Qualification:true,
    stableMonitorSha256:$stable_sha,latestBinaryStillRunning:true}' >"$latest_tmp"
mv "$latest_tmp" "$latest_reth"

temporary="$(mktemp "$evidence/.runtime37-total.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
  --argjson strict "$head_stats" --argjson head "$latest_head" --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" --arg gov_main "$expected_gov_main" \
  --arg gov_candidate "$expected_gov_candidate" --arg n42 "$expected_n42" --arg reth "$expected_reth" \
  --arg interop "$expected_interop" --arg strict_sha "$(sha256 "$strict")" \
  --arg independent_sha "$(sha256 "$independent")" --arg producer_sha "$(sha256 "$producer")" \
  --arg static_sha "$(sha256 "$static")" --arg copy_sha "$(sha256 "$copied")" \
  --arg supplemental_correction_sha "$(sha256 "$supplemental_15m_correction")" \
  --arg controller_rebind_sha "$(sha256 "$controller_rebind_correction")" \
  --arg independent_harness_rebind_sha "$(sha256 "$independent_harness_rebind")" \
  --arg correction_waiter_failure_sha "$(sha256 "$correction_waiter_failure")" \
  --arg latest_reth_sha "$(sha256 "$latest_reth")" \
  '{at:$at,event:"runtime37_goal_completion",status:"PASS",acceptanceRelaxed:false,
    objectiveRequirementsExtendedClosure:true,verifierScriptSha256:$self,strict24h:$strict,
    finalCanonicalHead:$head,consensus:$consensus,equivocations:$equivocations,
    genesis905AndSecurityBoundariesExact:true,copiedDataExact:true,allSixEndpointsExact:true,
    strict24hZeroTransactionExact:true,sixProducerRotationExact:true,transactionsFinalized:17,
    archiveAndQmdbParityExact:true,controlledRustRestartRejoined:true,postRestartStabilityExact:true,
    intermediateMilestonesExact:true,staticDataAndToolsExact:true,zeroEquivocations:true,
    officialRethStable:"v2.4.1",
    correctedShortWindowResourceProjectionFailure:true,
    correctedControllerRebindFailure:true,correctedIndependentVerifierHarnessPin:true,
    failureEvidencePreserved:true,
    noUncorrectedFailureEvidence:true,noConsensusOrDataFailureEvidence:true,
    sourceAndRemotePinsExact:true,binariesExact:true,
    sources:{govMain:$gov_main,govCandidate:$gov_candidate,n42:$n42,reth:$reth,interopTooling:$interop,allPushed:true},
    evidenceSha256:{strictSummary:$strict_sha,independentVerification:$independent_sha,
      strict24hSixProducer:$producer_sha,staticBoundary:$static_sha,stoppedDataCopy:$copy_sha,
      supplemental15mResourceCorrection:$supplemental_correction_sha,
      controllerRebindCorrection:$controller_rebind_sha,
      independentVerifierHarnessRebind:$independent_harness_rebind_sha,
      preservedCorrectionWaiterFailure:$correction_waiter_failure_sha,
      latestRethQualification:$latest_reth_sha}}' >"$temporary"
jq -e '.status=="PASS" and .strict24h.elapsedSeconds>=86400 and .strict24h.maximumLag<=1 and
  .transactionsFinalized==17 and .sixProducerRotationExact==true and
  .controlledRustRestartRejoined==true and .intermediateMilestonesExact==true and
  .sourceAndRemotePinsExact==true and
  .objectiveRequirementsExtendedClosure==true and
  .correctedIndependentVerifierHarnessPin==true and
  .failureEvidencePreserved==true and .noUncorrectedFailureEvidence==true and
  .noConsensusOrDataFailureEvidence==true' "$temporary" >/dev/null
mv "$temporary" "$output"
compat_temporary="$(mktemp "$evidence/.runtime37-total-compat.XXXXXX")"
cp "$output" "$compat_temporary"
test "$(sha256 "$compat_temporary")" = "$(sha256 "$output")"
mv "$compat_temporary" "$compat_output"
trap - EXIT
rm -rf "$audit_dir"
cat "$output"
