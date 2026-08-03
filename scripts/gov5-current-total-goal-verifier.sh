#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_TOTAL_RUNTIME:?runtime is required}"
latest_dir="${N42_TOTAL_LATEST_DIR:?latest-Reth directory is required}"
primary_repo="${N42_TOTAL_PRIMARY_REPO:?primary repository is required}"
combo_repo="${N42_TOTAL_COMBO_REPO:?combination repository is required}"
reth_repo="${N42_TOTAL_RETH_REPO:?Reth repository is required}"
deps_repo="${N42_TOTAL_DEPS_REPO:?dependency repository is required}"
gov_repo="${N42_TOTAL_GOV_REPO:?Gov5 repository is required}"
expected_self_sha="${N42_TOTAL_EXPECTED_SELF_SHA:-}"
expected_gov_main="${N42_TOTAL_GOV_MAIN:?Gov5 main is required}"
expected_gov_candidate="${N42_TOTAL_GOV_CANDIDATE:?Gov5 candidate is required}"
expected_gov_binary="${N42_TOTAL_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
expected_rust_binary="${N42_TOTAL_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
expected_combo="${N42_TOTAL_COMBO_COMMIT:?combination commit is required}"
expected_reth="${N42_TOTAL_RETH_COMMIT:?Reth commit is required}"
expected_deps="${N42_TOTAL_DEPS_COMMIT:?dependency commit is required}"
rollover_pid="${N42_TOTAL_ROLLOVER_PID:?rollover controller PID is required}"
copied="${N42_TOTAL_COPIED_MANIFEST:?copied-data manifest is required}"
canary="${N42_TOTAL_CANARY:?current-main canary is required}"
preflight_only="${N42_TOTAL_PREFLIGHT_ONLY:-0}"

strict_summary="$runtime/evidence/gov5-906-final-qualification.json"
strict_independent="$runtime/evidence/gov5-906-independent-final-verification.json"
latest_summary="$latest_dir/latest-reth-final-qualification.json"
latest_independent="$latest_dir/latest-reth-independent-final-verification.json"
heads="$runtime/evidence/mixed-soak-24h.jsonl"
resources="$runtime/evidence/rust-resource-24h.jsonl"
upstream="$runtime/evidence/gov5-upstream-24h.jsonl"
stable="$latest_dir/official-reth-stable-monitor.jsonl"
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
  local status=$? line="${BASH_LINENO[0]:-0}"
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
  branch="$(git -C "$repo" branch --show-current)"
  test -n "$branch"
  remote="$(git -C "$repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected"
}

assert_primary_pushed() {
  local head branch remote
  head="$(git -C "$primary_repo" rev-parse HEAD)"
  branch="$(git -C "$primary_repo" branch --show-current)"
  remote="$(git -C "$primary_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR==1{print $1}')"
  test -z "$(git -C "$primary_repo" status --porcelain --untracked-files=no)"
  test "$head" = "$remote"
}

assert_no_failures() {
  local path
  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$latest_dir/latest-reth-failures.jsonl" \
    "$latest_dir/latest-reth-independent-final-verification-failure.json"; do
    test ! -s "$path"
  done
}

assert_nodes() {
  local file
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
}

assert_current_sources() {
  local remote latest
  assert_primary_pushed
  assert_branch_pushed "$combo_repo" "$expected_combo"
  assert_branch_pushed "$deps_repo" "$expected_deps"
  assert_branch_pushed "$gov_repo" "$expected_gov_candidate"
  assert_branch_pushed "$reth_repo" "$expected_reth"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected_gov_main"
  latest="$(git ls-remote --tags https://github.com/paradigmxyz/reth.git \
    'refs/tags/v*' | sed -E 's#.*refs/tags/##; s/\^\{\}//' |
    rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
  test "$latest" = v2.4.1
}

assert_live() {
  local expected="" identity genesis nonce pending port exact=false attempt
  for port in "${ports[@]}"; do
    genesis="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' |
      jq -er '.result.hash')"
    test "$genesis" = "$expected_genesis"
    nonce="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' |
      jq -er '.result')"
    test "$nonce" = "${1:?expected nonce is required}"
    pending="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' |
      jq -er '.result')"
    test "$pending" = "$1"
  done
  for attempt in $(seq 1 30); do
    expected=""
    exact=true
    for port in "${ports[@]}"; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' |
        jq -ec '.result|{number,hash,stateRoot,receiptsRoot}')"
      if test -z "$expected"; then
        expected="$identity"
      elif test "$identity" != "$expected"; then
        exact=false
        break
      fi
    done
    test "$exact" = true && break
    sleep 1
  done
  test "$exact" = true
  jq -e '.result.validatorCount==7 and .result.hasCommittedQc==true' \
    < <(rpc 29545 n42_consensusStatus '[]') >/dev/null
  jq -e '.result.total==0 and (.result.evidence|length)==0' \
    < <(rpc 29545 n42_equivocations '[]') >/dev/null
}

test -z "$expected_self_sha" || test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary"
test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary"
test "$(sha256 "$latest_dir/n42-node")" = "$expected_rust_binary"
assert_nodes
assert_current_sources
assert_no_failures

if test "$preflight_only" = 1; then
  assert_live 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" \
    '{at:$at,event:"gov5_current_total_goal_verifier_preflight",status:"PASS",
      sourceAndRemotePinsExact:true,binariesExact:true,genesisExact:true,
      liveSixEndpointIdentityExact:true,senderNonce:"0x11",transactionsSent:0}'
  exit 0
fi

while ! test -s "$latest_independent"; do
  assert_nodes
  assert_no_failures
  kill -0 "$rollover_pid"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1{print $1}')"
  test "$remote" = "$expected_gov_main"
  if test -s "$heads"; then
    tail -n 1 "$heads" | jq -e '.ok==true and .zeroTxRequired==1' >/dev/null
  fi
  sleep 60
done

for path in "$strict_summary" "$strict_independent" "$latest_summary" \
  "$latest_independent" "$heads" "$resources" "$upstream" "$stable" \
  "$copied" "$canary"; do
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
jq -e '.status=="PASS" and .rethVersion=="2.4.1" and
  .rethCommit=="91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b" and
  .officialStableTag=="v2.4.1" and .officialStableTagExact==true and
  .headAudit.elapsedSeconds>=3600 and .resourceAudit.elapsedSeconds>=3600 and
  .rustLeaderAudit.status=="PASS" and .timeoutAudit.pendingTimeouts==0 and
  .runtimeLogAudit.unexpectedWarnings==0 and .runtimeLogAudit.criticalSignals==0 and
  .equivocations.total==0 and .preRolloverDataSnapshot.byteExact==true and
  .latestBinaryStillRunning==true' "$latest_summary" >/dev/null
jq -e '.status=="PASS" and .reexecutedAudits==true and
  .recomputedEvidenceHashes==true and .snapshotManifestsByteExact==true and
  .officialStableTag=="v2.4.1" and .liveSixEndpointIdentityExact==true and
  .genesisExact==true and .latestAndPendingNonce=="0x22"' \
  "$latest_independent" >/dev/null

jq -e -s 'length>=2 and all(.[];.ok==true and .zeroTxRequired==1) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400 and
  ([.[].lag]|max)<=6' "$heads" >/dev/null
jq -e -s --arg expected "$expected_gov_main" 'length>=2 and
  all(.[];.remoteReachable==true and .baselineExact==true and
    .baseline==$expected and .remoteMain==$expected) and
  ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601))>=86400' \
  "$upstream" >/dev/null
jq -e -s 'length>=2 and all(.[];.remoteReachable==true and
  .baselineExact==true and .expected=="v2.4.1" and .latest=="v2.4.1")' \
  "$stable" >/dev/null
jq -e '.status=="PASS" and .files==124 and
  .recomputedEntriesSha256==.baselineManifestEntriesSha256 and
  .allPathsSizesAndHashesExact==true' "$copied" >/dev/null
jq -e '.status=="PASS" and .allSixLatestExact==true and .genesisExact==true and
  .rustFivePlusFive==true and (.rustCommits|length)>=2 and
  .equivocations.total==0' "$canary" >/dev/null

assert_nodes
assert_current_sources
assert_no_failures
assert_live 0x22

head_stats="$(jq -cs '{samples:length,firstAt:.[0].at,lastAt:.[-1].at,
  elapsedSeconds:((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)),
  startHeight:.[0].commonHeight,endHeight:.[-1].commonHeight,
  blockGrowth:(.[-1].commonHeight-.[0].commonHeight),
  maximumLag:([.[].lag]|max),failures:([.[]|select(.ok!=true)]|length)}' "$heads")"
latest_head="$(rpc 29545 eth_getBlockByNumber '["latest",false]' |
  jq -ec '.result|{number,hash,stateRoot,receiptsRoot}')"
consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
temporary="$(mktemp "$runtime/evidence/.total-goal-final.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg verifier_sha "$(sha256 "${BASH_SOURCE[0]}")" \
  --argjson strict "$head_stats" --argjson head "$latest_head" \
  --argjson consensus "$consensus" --argjson equivocations "$equivocations" \
  --arg primary "$(git -C "$primary_repo" rev-parse HEAD)" \
  --arg gov_main "$expected_gov_main" --arg gov_candidate "$expected_gov_candidate" \
  --arg combo "$expected_combo" --arg reth "$expected_reth" --arg deps "$expected_deps" \
  --arg strict_sha "$(sha256 "$strict_summary")" \
  --arg strict_independent_sha "$(sha256 "$strict_independent")" \
  --arg latest_sha "$(sha256 "$latest_summary")" \
  --arg latest_independent_sha "$(sha256 "$latest_independent")" \
  --arg copied_sha "$(sha256 "$copied")" \
  '{at:$at,event:"gov5_906_total_goal_final_verification",status:"PASS",
    acceptanceRelaxed:false,verifierScriptSha256:$verifier_sha,strict24h:$strict,
    finalCanonicalHead:$head,consensus:$consensus,equivocations:$equivocations,
    allSixEndpointsExact:true,genesisExact:true,latestAndPendingNonce:"0x22",
    transactionsFinalized:17,rustLeaderProductionExact:true,
    timeoutRecoveryExact:true,archiveAndQmdbParityExact:true,
    controlledRestartRejoined:true,latestRethExtraHourExact:true,
    copiedData:{files:124,evidenceSha256:$copied_sha,byteExact:true},
    sources:{primaryHead:$primary,govCandidate:$gov_candidate,govMain:$gov_main,
      latestCombination:$combo,reth:$reth,dependencyUpdate:$deps,allPushed:true},
    evidenceSha256:{strictSummary:$strict_sha,strictIndependent:$strict_independent_sha,
      latestRethSummary:$latest_sha,latestRethIndependent:$latest_independent_sha},
    sourceAndRemotePinsExact:true,binariesExact:true,noFailureEvidence:true}' \
  >"$temporary"
jq -e '.status=="PASS" and .strict24h.elapsedSeconds>=86400 and
  .transactionsFinalized==17 and .latestRethExtraHourExact==true and
  .noFailureEvidence==true' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
