#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_TOTAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-36-gov5-906-latest-9ae042-rustsec}"
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
expected_verifier="fa0f06dbadd23e2c662ac2790edbbad5405b0297c424c051bcc096bd61084529"
expected_finalizer="c48c5f3a94e361ce7cb81b41c586e12907f7a2adff688cb661062d9d21692fa0"
expected_static="c381dfeef85458373bac42f51bbb7019c414ceba4f03ff4ac374faff806b8161"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_905_boundary="0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82"
expected_security_boundary="0x7ccd33002b040389eb0627fca27ef361e330234f85091b016c5e3c4256332407"
expected_client="reth/v2.4.1-0fc810b/aarch64-apple-darwin"
ports=(28501 28502 28503 28504 28505 29545)

evidence="$runtime/evidence"
strict="$evidence/gov5-906-final-qualification.json"
independent="$evidence/gov5-906-independent-final-verification.json"
producer="$evidence/runtime36-latest-c0a146-strict24h-six-producer.json"
producer_linkage="$evidence/runtime28-strict24h-six-producer-linkage.json"
formal="$evidence/mixed-soak-24h.jsonl"
resources="$evidence/rust-resource-24h.jsonl"
resource_audit="$evidence/rust-resource-24h-audit.json"
upstream="$evidence/gov5-upstream-24h.jsonl"
upstream_complete="$evidence/gov5-upstream-24h-complete.json"
upstream_audit="$evidence/gov5-upstream-24h-audit.json"
stable="$evidence/official-reth-stable-monitor.jsonl"
copied="$evidence/runtime36-stopped-copy-manifest.json"
static="$evidence/runtime36-latest-c0a146-static-boundary.json"
data_905="$evidence/runtime36-latest-c0a146-905-data-compatibility.json"
network="$evidence/runtime36-latest-c0a146-network-consensus-matrix.json"
supplemental_launch_failure="$evidence/runtime36-latest-c0a146-formal-15m-supplemental-audit-failure.json"
supplemental_launch_correction="$evidence/runtime36-latest-c0a146-formal-15m-supplemental-launch-correction.json"
latest_reth="$evidence/latest-reth-final-qualification.json"
output="${N42_TOTAL_OUTPUT:-$evidence/runtime36-goal-completion.json}"
failure="${N42_TOTAL_FAILURE:-$evidence/runtime36-goal-completion-failure.json}"
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

assert_nodes() {
  local file
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    kill -0 "$(<"$file")"
  done
}

assert_no_failures() {
  local item
  for item in \
    "$evidence/gov5-current-main-fail-close-guardian-failure.json" \
    "$evidence/gov5-906-finalizer-failures.jsonl" \
    "$evidence/gov5-906-independent-final-verification-failure.json" \
    "$evidence/runtime28-strict24h-six-producer-full-range-failure.json"; do
    test ! -s "$item"
  done
  if test -s "$supplemental_launch_failure"; then
    test -s "$supplemental_launch_correction"
    jq -e --arg failure_sha "$(sha256 "$supplemental_launch_failure")" '
      .status=="PASS" and .originalFailurePreserved==true and
      .originalFailureSha256==$failure_sha and .chainOrDataMutation==false and
      .correctedAuditStatus=="PASS" and .acceptanceRelaxed==false and
      .transactionsSent==0 and .nodesRestarted==false and
      .rawFormalEvidenceEdited==false
    ' "$supplemental_launch_correction" >/dev/null
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
  test "$(sha256 "$static")" = "$expected_static"
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
    '{at:$at,event:"runtime36_total_goal_finalizer_failure",status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}

assert_static
assert_nodes
assert_no_failures
assert_live 0x11
test "$(latest_reth_stable)" = v2.4.1

if test "$preflight_only" = 1; then
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" --arg interop "$expected_interop" \
    '{at:$at,event:"runtime36_total_goal_finalizer_preflight",status:"PASS",scriptSha256:$self,
      interopCommit:$interop,nodesAlive:true,binariesExact:true,sourcesAndRemotesExact:true,
      genesis905AndSecurityBoundariesExact:true,staticBoundaryExact:true,
      liveSixEndpointIdentityExact:true,zeroEquivocations:true,officialRethStable:"v2.4.1",
      transactionsSent:0}'
  exit 0
fi

test ! -e "$output"
test ! -e "$failure"
trap on_error ERR

required=("$strict" "$independent" "$producer" "$producer_linkage" "$resource_audit" "$upstream_complete" "$upstream_audit")
while :; do
  missing=false
  for item in "${required[@]}"; do test -s "$item" || missing=true; done
  test "$missing" = false && break
  assert_nodes
  assert_no_failures
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
jq -e -s 'length>=2 and all(.[];.ok==true and .zeroTxRequired==1 and .lag<=1) and
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

audit_dir="$(mktemp -d /tmp/n42-runtime36-total.XXXXXX)"
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
  "$verifier" >"$audit_dir/independent.json"
jq -e '.status=="PASS" and .transactionsFinalized==17' "$audit_dir/independent.json" >/dev/null

assert_static
assert_nodes
assert_no_failures
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

temporary="$(mktemp "$evidence/.runtime36-total.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
  --argjson strict "$head_stats" --argjson head "$latest_head" --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" --arg gov_main "$expected_gov_main" \
  --arg gov_candidate "$expected_gov_candidate" --arg n42 "$expected_n42" --arg reth "$expected_reth" \
  --arg interop "$expected_interop" --arg strict_sha "$(sha256 "$strict")" \
  --arg independent_sha "$(sha256 "$independent")" --arg producer_sha "$(sha256 "$producer")" \
  --arg static_sha "$(sha256 "$static")" --arg copy_sha "$(sha256 "$copied")" \
  --arg supplemental_correction_sha "$(sha256 "$supplemental_launch_correction")" \
  --arg latest_reth_sha "$(sha256 "$latest_reth")" \
  '{at:$at,event:"runtime36_goal_completion",status:"PASS",acceptanceRelaxed:false,
    objectiveRequirementsExtendedClosure:true,verifierScriptSha256:$self,strict24h:$strict,
    finalCanonicalHead:$head,consensus:$consensus,equivocations:$equivocations,
    genesis905AndSecurityBoundariesExact:true,copiedDataExact:true,allSixEndpointsExact:true,
    strict24hZeroTransactionExact:true,sixProducerRotationExact:true,transactionsFinalized:17,
    archiveAndQmdbParityExact:true,controlledRustRestartRejoined:true,postRestartStabilityExact:true,
    staticDataAndToolsExact:true,zeroEquivocations:true,officialRethStable:"v2.4.1",
    correctedNonAcceptanceToolingFailureEvidence:true,noUncorrectedFailureEvidence:true,
    sourceAndRemotePinsExact:true,binariesExact:true,
    sources:{govMain:$gov_main,govCandidate:$gov_candidate,n42:$n42,reth:$reth,interopTooling:$interop,allPushed:true},
    evidenceSha256:{strictSummary:$strict_sha,independentVerification:$independent_sha,
      strict24hSixProducer:$producer_sha,staticBoundary:$static_sha,stoppedDataCopy:$copy_sha,
      supplementalLaunchCorrection:$supplemental_correction_sha,
      latestRethQualification:$latest_reth_sha},noFailureEvidence:true}' >"$temporary"
jq -e '.status=="PASS" and .strict24h.elapsedSeconds>=86400 and .strict24h.maximumLag<=1 and
  .transactionsFinalized==17 and .sixProducerRotationExact==true and
  .controlledRustRestartRejoined==true and .sourceAndRemotePinsExact==true and
  .objectiveRequirementsExtendedClosure==true and .noFailureEvidence==true' "$temporary" >/dev/null
mv "$temporary" "$output"
trap - EXIT
rm -rf "$audit_dir"
cat "$output"
