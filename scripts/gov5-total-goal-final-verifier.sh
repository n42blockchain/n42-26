#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_TOTAL_RUNTIME:?runtime is required}"
qualification_dir="${N42_TOTAL_LATEST_DIR:?latest-Reth directory is required}"
primary_repo="${N42_TOTAL_PRIMARY_REPO:?primary repository is required}"
combo_repo="${N42_TOTAL_COMBO_REPO:?latest-Reth combination repository is required}"
reth_repo="${N42_TOTAL_RETH_REPO:?latest Reth repository is required}"
deps_repo="${N42_TOTAL_DEPS_REPO:?dependency repository is required}"
gov_repo="${N42_TOTAL_GOV_REPO:?Gov5 repository is required}"
expected_self_sha="${N42_TOTAL_EXPECTED_SELF_SHA:?verifier SHA-256 is required}"
expected_gov_main="${N42_TOTAL_GOV_MAIN:?Gov5 main is required}"
expected_gov_candidate="${N42_TOTAL_GOV_CANDIDATE:?Gov5 candidate is required}"
expected_gov_binary="${N42_TOTAL_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
expected_rust_binary="${N42_TOTAL_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
expected_combo="${N42_TOTAL_COMBO_COMMIT:?combination commit is required}"
expected_reth="${N42_TOTAL_RETH_COMMIT:?Reth commit is required}"
expected_deps="${N42_TOTAL_DEPS_COMMIT:?dependency commit is required}"
controller_pid="${N42_TOTAL_CONTROLLER_GUARDIAN_PID:?controller guardian PID is required}"
monitor_pid="${N42_TOTAL_MONITOR_GUARDIAN_PID:?monitor guardian PID is required}"
caffeinate_pid="${N42_TOTAL_CAFFEINATE_PID:?caffeinate PID is required}"

strict_summary="$runtime/evidence/gov5-906-final-qualification.json"
strict_independent="$runtime/evidence/gov5-906-independent-final-verification.json"
immutable="$runtime/evidence/gov5-906-immutable-final-log-verification.json"
immutable_gate="$runtime/evidence/gov5-906-immutable-final-log-gate.json"
latest_summary="$qualification_dir/latest-reth-final-qualification.json"
latest_independent="$qualification_dir/latest-reth-independent-final-verification.json"
heads="$runtime/evidence/mixed-soak-24h.jsonl"
resources="$runtime/evidence/rust-resource-24h.jsonl"
upstream="$runtime/evidence/gov5-upstream-24h.jsonl"
stable="$qualification_dir/official-reth-stable-monitor.jsonl"
copied="$runtime/evidence/runtime26-copied-chain-data-manifest.json"
canary="$runtime/evidence/runtime26-current-main-canary.json"
controller="$runtime/evidence/gov5-qualification-controller-guardian.jsonl"
monitor="$runtime/evidence/runtime22-monitor-pid-guardian.jsonl"
output="$runtime/evidence/gov5-906-total-goal-final-verification.json"
failure="$runtime/evidence/gov5-906-total-goal-final-verification-failure.json"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
ports=(28501 28502 28503 28504 28505 29545)

test ! -e "$output"
test ! -e "$failure"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_906_total_goal_final_verification_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

assert_branch_pushed() {
  local repo="$1" expected="$2" branch remote
  test "$(git -C "$repo" rev-parse HEAD)" = "$expected"
  test -z "$(git -C "$repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$repo" rev-parse --abbrev-ref HEAD)"
  test "$branch" != HEAD
  remote="$(git -C "$repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$expected"
}

check_wait_state() {
  local file remote
  test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
  kill -0 "$caffeinate_pid"
  if ! test -s "$latest_independent"; then
    kill -0 "$controller_pid"
  fi
  if ! jq -e -s '.[-1].event=="runtime22_monitor_pid_guardian_complete" and
      .[-1].status=="PASS"' "$monitor" >/dev/null 2>&1; then
    kill -0 "$monitor_pid"
  fi
  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-immutable-final-log-verification-failure.json" \
    "$runtime/evidence/gov5-906-immutable-final-log-gate-failure.json" \
    "$runtime/evidence/gov5-qualification-controller-guardian-failures.jsonl" \
    "$runtime/evidence/runtime22-monitor-pid-guardian-failures.jsonl" \
    "$qualification_dir/latest-reth-failures.jsonl" \
    "$qualification_dir/latest-reth-independent-final-verification-failure.json"; do
    test ! -s "$path"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$expected_gov_main"
}

while ! test -s "$latest_independent"; do
  check_wait_state
  sleep 60
done

for _ in $(seq 1 120); do
  controller_done=false
  monitor_done=false
  if jq -e -s '.[-1].event=="gov5_qualification_controller_guardian_complete" and
      .[-1].status=="PASS"' "$controller" >/dev/null 2>&1; then
    controller_done=true
  fi
  if jq -e -s '.[-1].event=="runtime22_monitor_pid_guardian_complete" and
      .[-1].status=="PASS"' "$monitor" >/dev/null 2>&1; then
    monitor_done=true
  fi
  test "$controller_done" = true && test "$monitor_done" = true && break
  check_wait_state
  sleep 1
done
test "$controller_done" = true
test "$monitor_done" = true
check_wait_state

for path in "$strict_summary" "$strict_independent" "$immutable" \
  "$immutable_gate" "$latest_summary" "$latest_independent" "$heads" \
  "$resources" "$upstream" "$stable" "$copied" "$canary"; do
  test -s "$path"
done

jq -e '.status=="PASS" and .acceptanceRelaxed==false and
  .soakAudit.elapsedSeconds>=86400 and .soakAudit.zeroTransactionRequired==true and
  .transactionBurst.transactions==17 and .transactionBurst.endpointCount==6 and
  .transactionBurst.allConfiguredEndpointsExact==true and
  .postBurstAudit.elapsedSeconds>=600 and .postRestartAudit.elapsedSeconds>=600 and
  .archiveParityPostBurst==true and .rustLeaderAudit.status=="PASS" and
  .rustLeaderAudit.leaderCommitLog.allVotesFivePlusFive==true and
  .timeoutRecoveryAudit.pendingTimeouts==0 and
  .runtimeLogAudit.unexpectedWarnings==0 and .runtimeLogAudit.criticalSignals==0 and
  .rustResourceAudit.elapsedSeconds>=86400 and .zeroEquivocations==true' \
  "$strict_summary" >/dev/null
jq -e '.status=="PASS" and .transactionsFinalized==17 and
  .finalSenderNonce=="0x22" and .independentRawAuditsReexecuted==true and
  .liveArchiveParityReexecuted==true' "$strict_independent" >/dev/null
jq -e '.status=="PASS" and .capturedBeforeLatestRethRollover==true and
  .canonicalLeaderEvidenceBound==true and .timeoutRecoveryPrefixExact==true and
  .allWarningsPartitionedExactly==true and
  .allEmbeddedLogHashesRecomputedExact==true' "$immutable" >/dev/null
jq -e '.status=="PASS" and .independentVerifierHeldBeforeSummary==true and
  .independentVerifierReleasedAfterImmutablePass==true and
  .latestRethRolloverWasTransitivelyGated==true' "$immutable_gate" >/dev/null
jq -e '.status=="PASS" and .rethVersion=="2.4.1" and
  .rethCommit=="91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b" and
  .officialStableTag=="v2.4.1" and .officialStableTagExact==true and
  .headAudit.elapsedSeconds>=3600 and .resourceAudit.elapsedSeconds>=3600 and
  .rustLeaderAudit.status=="PASS" and
  .rustLeaderAudit.leaderCommitLog.allVotesFivePlusFive==true and
  .timeoutAudit.pendingTimeouts==0 and .runtimeLogAudit.unexpectedWarnings==0 and
  .runtimeLogAudit.criticalSignals==0 and .equivocations.total==0 and
  .preRolloverDataSnapshot.byteExact==true and .latestBinaryStillRunning==true' \
  "$latest_summary" >/dev/null
jq -e '.status=="PASS" and .reexecutedAudits==true and
  .recomputedEvidenceHashes==true and .snapshotManifestsByteExact==true and
  .officialStableTag=="v2.4.1" and .liveSixEndpointIdentityExact==true and
  .genesisExact==true and .latestAndPendingNonce=="0x22"' \
  "$latest_independent" >/dev/null

jq -e -s 'length>=2 and all(.[];.ok==true and .zeroTxRequired==1) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400 and
  ([.[].lag]|max)<=6' "$heads" >/dev/null
jq -e -s --arg expected "$expected_gov_main" '
  length>=2 and all(.[];.remoteReachable==true and .baselineExact==true and
    .baseline==$expected and .remoteMain==$expected) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400' \
  "$upstream" >/dev/null
jq -e -s 'length>=2 and all(.[];.remoteReachable==true and
  .baselineExact==true and .expected=="v2.4.1" and .latest=="v2.4.1")' \
  "$stable" >/dev/null
jq -e '.status=="PASS" and .files==123 and
  .sourceManifestSha256==.targetManifestSha256 and
  .allPathsSizesAndHashesExact==true' "$copied" >/dev/null
jq -e '.status=="PASS" and .allSixLatestExact==true and .genesisExact==true and
  .rustFivePlusFive==true and (.rustCommits|length)>=2 and
  .equivocations.total==0' "$canary" >/dev/null

test "$(sha256 "$runtime/artifacts/genesis.json")" = \
  561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687
test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary"
test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary"
test "$(sha256 "$qualification_dir/n42-node")" = "$expected_rust_binary"

test -z "$(git -C "$primary_repo" status --porcelain --untracked-files=no)"
test "$(git -C "$primary_repo" rev-parse HEAD)" = \
  "$(git -C "$primary_repo" rev-parse '@{upstream}')"
assert_branch_pushed "$combo_repo" "$expected_combo"
assert_branch_pushed "$deps_repo" "$expected_deps"
assert_branch_pushed "$gov_repo" "$expected_gov_candidate"
test "$(git -C "$reth_repo" rev-parse HEAD)" = "$expected_reth"
test -z "$(git -C "$reth_repo" status --porcelain --untracked-files=no)"
git -C "$reth_repo" merge-base --is-ancestor "$expected_reth" \
  origin/chore/reth-upstream-20260726

versions='[]'
expected_identity=""
for port in "${ports[@]}"; do
  genesis="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' |
    jq -er '.result.hash')"
  test "$genesis" = "$expected_genesis"
  latest_nonce="$(rpc "$port" eth_getTransactionCount \
    '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')"
  pending_nonce="$(rpc "$port" eth_getTransactionCount \
    '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')"
  test "$latest_nonce" = 0x22
  test "$pending_nonce" = 0x22
  version="$(rpc "$port" web3_clientVersion '[]' | jq -er '.result')"
  versions="$(jq -nc --argjson existing "$versions" --argjson port "$port" \
    --arg version "$version" '$existing+[{port:$port,version:$version}]')"
done

live_exact=false
for _ in $(seq 1 30); do
  expected_identity=""
  live_exact=true
  for port in "${ports[@]}"; do
    identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' |
      jq -ec '.result|{number,hash,stateRoot,receiptsRoot}')"
    if test -z "$expected_identity"; then
      expected_identity="$identity"
    elif test "$identity" != "$expected_identity"; then
      live_exact=false
      break
    fi
  done
  test "$live_exact" = true && break
  sleep 1
done
test "$live_exact" = true
consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
jq -e '.validatorCount==7 and .hasCommittedQc==true' <<<"$consensus" >/dev/null
jq -e '.total==0 and (.evidence|length)==0' <<<"$equivocations" >/dev/null

strict_stats="$(jq -cs '
  {samples:length,firstAt:.[0].at,lastAt:.[-1].at,
   elapsedSeconds:((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)),
   startHeight:.[0].commonHeight,endHeight:.[-1].commonHeight,
   blockGrowth:(.[-1].commonHeight-.[0].commonHeight),
   maximumLag:([.[].lag]|max),failures:([.[]|select(.ok!=true)]|length)}' "$heads")"

temporary="$(mktemp "$runtime/evidence/.total-goal-final.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg verifier_sha "$expected_self_sha" \
  --argjson head "$expected_identity" --argjson versions "$versions" \
  --argjson consensus "$consensus" --argjson equivocations "$equivocations" \
  --argjson strict "$strict_stats" --arg gov_main "$expected_gov_main" \
  --arg gov_candidate "$expected_gov_candidate" \
  --arg primary_head "$(git -C "$primary_repo" rev-parse HEAD)" \
  --arg combo "$expected_combo" --arg reth "$expected_reth" --arg deps "$expected_deps" \
  --arg strict_sha "$(sha256 "$strict_summary")" \
  --arg strict_independent_sha "$(sha256 "$strict_independent")" \
  --arg immutable_sha "$(sha256 "$immutable")" \
  --arg gate_sha "$(sha256 "$immutable_gate")" \
  --arg latest_sha "$(sha256 "$latest_summary")" \
  --arg latest_independent_sha "$(sha256 "$latest_independent")" \
  --arg copied_sha "$(sha256 "$copied")" '
  {at:$at,event:"gov5_906_total_goal_final_verification",status:"PASS",
    acceptanceRelaxed:false,verifierScriptSha256:$verifier_sha,strict24h:$strict,
    finalCanonicalHead:$head,clientVersions:$versions,consensus:$consensus,
    equivocations:$equivocations,allSixEndpointsExact:true,genesisExact:true,
    latestAndPendingNonce:"0x22",transactionsFinalized:17,
    rustLeaderProductionExact:true,timeoutRecoveryExact:true,
    archiveAndQmdbParityExact:true,controlledRestartRejoined:true,
    latestRethExtraHourExact:true,
    copiedData:{files:123,evidenceSha256:$copied_sha,byteExact:true},
    sources:{primaryHead:$primary_head,govCandidate:$gov_candidate,govMain:$gov_main,
      latestCombination:$combo,reth:$reth,dependencyUpdate:$deps,allPushed:true},
    evidenceSha256:{strictSummary:$strict_sha,
      strictIndependent:$strict_independent_sha,immutableFinalLog:$immutable_sha,
      immutableGate:$gate_sha,latestRethSummary:$latest_sha,
      latestRethIndependent:$latest_independent_sha},
    controllerGuardianCompleted:true,monitorGuardianCompleted:true,
    sourceAndRemotePinsExact:true,binariesExact:true,noFailureEvidence:true}' \
  >"$temporary"
jq -e '.status=="PASS" and .strict24h.elapsedSeconds>=86400 and
  .transactionsFinalized==17 and .latestRethExtraHourExact==true and
  .noFailureEvidence==true' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
