#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_COMPLETION_RUNTIME:?runtime is required}"
primary_repo="${N42_COMPLETION_PRIMARY_REPO:?primary repository is required}"
gov_repo="${N42_COMPLETION_GOV_REPO:?Gov5 repository is required}"
combo_repo="${N42_COMPLETION_COMBO_REPO:?combination repository is required}"
reth_repo="${N42_COMPLETION_RETH_REPO:?Reth repository is required}"
deps_repo="${N42_COMPLETION_DEPS_REPO:?dependency repository is required}"
expected_self_sha="${N42_COMPLETION_EXPECTED_SELF_SHA:?verifier SHA-256 is required}"
preflight_only="${N42_COMPLETION_PREFLIGHT_ONLY:-0}"

expected_gov_main="${N42_COMPLETION_GOV_MAIN:-39db96184cd0d4a8745057e2733b1cea421f9983}"
expected_gov_candidate="${N42_COMPLETION_GOV_CANDIDATE:-c3da82738dfb3a7cf13814e863551af0a16aa2da}"
expected_combo="${N42_COMPLETION_COMBO_COMMIT:-ab05838691e6ec71f5df0faa1d3eefb1fc9d3d9e}"
expected_reth="${N42_COMPLETION_RETH_COMMIT:-91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b}"
expected_deps="${N42_COMPLETION_DEPS_COMMIT:-aec34a0cd465e8fdbb598b90bc778fe96e25d6c0}"
expected_gov_binary="${N42_COMPLETION_GOV_BINARY_SHA:-de89a17768b8711b50820104f2e9f77b7dd8f03b689261bffa7eb9bd8b8b60f0}"
expected_rust_binary="${N42_COMPLETION_RUST_BINARY_SHA:-0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f}"
expected_genesis=0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec
sender=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
ports=(28501 28502 28503 28504 28505 29545)

total="$runtime/evidence/gov5-906-total-goal-final-verification.json"
boundary="$runtime/evidence/gov5-906-copied-boundary-final-verification.json"
network="$runtime/evidence/gov5-906-final-network-verification.json"
output="$runtime/evidence/gov5-906-goal-completion-audit.json"
failure="$runtime/evidence/gov5-906-goal-completion-audit-failure.json"

test ! -e "$output"
test ! -e "$failure"

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

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_906_goal_completion_audit_failure",status:"FAIL",
      statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

assert_pushed_exact() {
  local repo="$1" expected="$2" include_untracked="${3:-yes}"
  local branch remote status
  test "$(git -C "$repo" rev-parse HEAD)" = "$expected"
  branch="$(git -C "$repo" branch --show-current)"
  test -n "$branch"
  remote="$(git -C "$repo" ls-remote origin "refs/heads/$branch" | awk 'NR==1{print $1}')"
  test "$remote" = "$expected"
  if test "$include_untracked" = no; then
    status="$(git -C "$repo" status --porcelain --untracked-files=no)"
  else
    status="$(git -C "$repo" status --porcelain)"
  fi
  test -z "$status"
}

assert_sources() {
  local primary branch remote latest
  primary="$(git -C "$primary_repo" rev-parse HEAD)"
  branch="$(git -C "$primary_repo" branch --show-current)"
  remote="$(git -C "$primary_repo" ls-remote origin "refs/heads/$branch" | awk 'NR==1{print $1}')"
  test "$primary" = "$remote"
  test -z "$(git -C "$primary_repo" status --porcelain --untracked-files=no)"
  assert_pushed_exact "$gov_repo" "$expected_gov_candidate"
  assert_pushed_exact "$combo_repo" "$expected_combo"
  assert_pushed_exact "$reth_repo" "$expected_reth"
  assert_pushed_exact "$deps_repo" "$expected_deps"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}')"
  test "$remote" = "$expected_gov_main"
  latest="$(git ls-remote --tags https://github.com/paradigmxyz/reth.git \
    'refs/tags/v*' | sed -E 's#.*refs/tags/##; s/\^\{\}//' | \
    rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
  test "$latest" = v2.4.1
}

assert_nodes_and_failures() {
  local node_file evidence_item
  for node_file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$node_file"
    kill -0 "$(<"$node_file")"
  done
  for evidence_item in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/official-reth-stable/latest-reth-failures.jsonl" \
    "$runtime/evidence/official-reth-stable/latest-reth-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-copied-boundary-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-final-network-verification-failure.json"; do
    test ! -s "$evidence_item"
  done
}

assert_live() {
  local expected_nonce="$1" port genesis latest pending client chain
  local expected="" identity exact=false attempt
  for port in "${ports[@]}"; do
    genesis="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')"
    test "$genesis" = "$expected_genesis"
    chain="$(rpc "$port" eth_chainId '[]' | jq -er '.result')"
    test "$chain" = 0x477
    client="$(rpc "$port" web3_clientVersion '[]' | jq -er '.result')"
    if test "$port" -eq 29545; then
      test "$client" = reth/v2.4.1-91725e3/aarch64-apple-darwin
    else
      test "$client" = N42/5.7.906
    fi
    latest="$(rpc "$port" eth_getTransactionCount \
      "[\"$sender\",\"latest\"]" | jq -er '.result')"
    pending="$(rpc "$port" eth_getTransactionCount \
      "[\"$sender\",\"pending\"]" | jq -er '.result')"
    test "$latest" = "$expected_nonce"
    test "$pending" = "$expected_nonce"
  done
  for attempt in $(seq 1 30); do
    expected=""
    exact=true
    for port in "${ports[@]}"; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' | \
        jq -ec '.result|{number,hash,parentHash,stateRoot,receiptsRoot,transactionsRoot}')"
      if test -z "$expected"; then expected="$identity";
      elif test "$identity" != "$expected"; then exact=false; break; fi
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

test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary"
test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary"
assert_sources
assert_nodes_and_failures

if test "$preflight_only" = 1; then
  assert_live 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
    --arg primary "$(git -C "$primary_repo" rev-parse HEAD)" \
    '{at:$at,event:"gov5_906_goal_completion_auditor_preflight",status:"PASS",
      auditorSha256:$self,primaryHead:$primary,sourcesAndRemotesExact:true,
      binariesExact:true,liveSixEndpointIdentityExact:true,genesisExact:true,
      latestAndPendingNonce:"0x11",completionNotClaimed:true,mutationPerformed:false}'
  exit 0
fi

for evidence_item in "$total" "$boundary" "$network"; do
  test -s "$evidence_item"
done
jq -e '
  .status=="PASS" and .strict24h.elapsedSeconds>=86400 and
  .transactionsFinalized==17 and .latestRethExtraHourExact and
  .latestAndPendingNonce=="0x22" and .sourceAndRemotePinsExact and
  .noFailureEvidence
' "$total" >/dev/null
jq -e '
  .status=="PASS" and .sourcePersistedHead==92605 and
  .historicalBlocksReexecuted==7 and .allSixHistoricalBoundariesExact and
  .genesisExact and .latestRethPostRolloverExact and
  .latestAndPendingNonce=="0x22" and .equivocations.total==0 and
  .noFailureEvidence
' "$boundary" >/dev/null
jq -e '
  .status=="PASS" and .latestRethConsensusNetworkReestablished and
  .latestAndPendingNonce=="0x22" and .matrix.status=="PASS" and
  (.matrix.rustConsensusSockets.unexpectedEstablishedTcpRemotePorts|length)==0 and
  .matrix.rustConsensusSockets.directionalObservationOnly and
  .matrix.authenticatedValidatorOverlayConnected and
  .matrix.transportDirectionAgnostic and
  .matrix.authenticatedValidatorPeerCount==5 and
  .matrix.quorumEvidence.connectedValidatorPeers==5 and
  .matrix.latestRustFivePlusFiveCommit.blockHash!=null and
  .matrix.allSixCommittedBlockIdentityExact and .matrix.equivocations.total==0 and
  .noFailureEvidence
' "$network" >/dev/null

assert_sources
assert_nodes_and_failures
assert_live 0x22

latest_head="$(rpc 29545 eth_getBlockByNumber '["latest",false]' | \
  jq -ec '.result|{number,hash,parentHash,stateRoot,receiptsRoot,transactionsRoot,miner}')"
consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
temporary="$(mktemp "$runtime/evidence/.goal-completion.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
  --arg primary "$(git -C "$primary_repo" rev-parse HEAD)" \
  --arg gov_main "$expected_gov_main" --arg gov_candidate "$expected_gov_candidate" \
  --arg combo "$expected_combo" --arg reth "$expected_reth" --arg deps "$expected_deps" \
  --arg total_sha "$(sha256 "$total")" --arg boundary_sha "$(sha256 "$boundary")" \
  --arg network_sha "$(sha256 "$network")" --argjson head "$latest_head" \
  --argjson consensus "$consensus" '
  {at:$at,event:"gov5_906_goal_completion_audit",status:"PASS",
   acceptanceRelaxed:false,auditorSha256:$self,primaryHead:$primary,
   gov5Main:$gov_main,gov5Candidate:$gov_candidate,combinationCommit:$combo,
   rethCommit:$reth,dependencyCommit:$deps,
   evidenceSha256:{totalGoal:$total_sha,copiedBoundary:$boundary_sha,
     finalNetwork:$network_sha},finalCanonicalHead:$head,consensus:$consensus,
   strict24hExact:true,transactionsFinalized:17,archiveAndQmdbParityExact:true,
   controlledRestartAndCatchupExact:true,latestStableRethExtraHourExact:true,
   copied905BoundaryAndGenesisExact:true,postRolloverNetworkExact:true,
   latestAndPendingNonce:"0x22",sourcesAndRemotesExact:true,binariesExact:true,
   allSixEndpointsExact:true,zeroEquivocations:true,noFailureEvidence:true,
   objectiveRequirementsIndependentlyClosed:true}' >"$temporary"
jq -e '
  .status=="PASS" and .strict24hExact and .transactionsFinalized==17 and
  .archiveAndQmdbParityExact and .controlledRestartAndCatchupExact and
  .latestStableRethExtraHourExact and .copied905BoundaryAndGenesisExact and
  .postRolloverNetworkExact and .latestAndPendingNonce=="0x22" and
  .sourcesAndRemotesExact and .allSixEndpointsExact and .zeroEquivocations and
  .noFailureEvidence and .objectiveRequirementsIndependentlyClosed
' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
