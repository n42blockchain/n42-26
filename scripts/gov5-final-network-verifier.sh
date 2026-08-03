#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_FINAL_NETWORK_RUNTIME:?runtime is required}"
gov_repo="${N42_FINAL_NETWORK_GOV_REPO:?Gov5 repository is required}"
expected_main="${N42_FINAL_NETWORK_GOV_MAIN:?expected Gov5 main is required}"
expected_self_sha="${N42_FINAL_NETWORK_EXPECTED_SELF_SHA:?verifier SHA-256 is required}"
expected_auditor_sha="${N42_FINAL_NETWORK_EXPECTED_AUDITOR_SHA:?auditor SHA-256 is required}"
preflight_only="${N42_FINAL_NETWORK_PREFLIGHT_ONLY:-0}"

auditor="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/audit-gov5-mixed-network-matrix.sh"
total="$runtime/evidence/gov5-906-total-goal-final-verification.json"
raw="$runtime/evidence/gov5-906-final-network-consensus-matrix.raw.json"
output="$runtime/evidence/gov5-906-final-network-verification.json"
failure="$runtime/evidence/gov5-906-final-network-verification-failure.json"
ports=(28501 28502 28503 28504 28505 29545)
sender=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266

for evidence_item in "$raw" "$output" "$failure"; do
  test ! -e "$evidence_item"
done

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
    '{at:$at,event:"gov5_906_final_network_verification_failure",status:"FAIL",
      statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

assert_wait_state() {
  local node_file remote
  test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
  test -s "$auditor"
  test "$(sha256 "$auditor")" = "$expected_auditor_sha"
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
    "$runtime/evidence/gov5-906-copied-boundary-final-verification-failure.json"; do
    test ! -s "$evidence_item"
  done
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}')"
  test "$remote" = "$expected_main"
}

assert_nonce() {
  local expected="$1" port latest pending
  for port in "${ports[@]}"; do
    latest="$(rpc "$port" eth_getTransactionCount \
      "[\"$sender\",\"latest\"]" | jq -er '.result')"
    pending="$(rpc "$port" eth_getTransactionCount \
      "[\"$sender\",\"pending\"]" | jq -er '.result')"
    test "$latest" = "$expected"
    test "$pending" = "$expected"
  done
}

assert_matrix() {
  local matrix_file="$1"
  jq -e '
    .status=="PASS" and .mutationPerformed==false and
    .rustConsensusSockets.allFiveEstablished and
    .authenticatedValidatorPeerCount==5 and
    .quorumEvidence.connectedValidatorPeers==5 and
    .quorumEvidence.neededQuorumPeers==4 and
    .directPushEvidence.directValidatorPeers==5 and
    .latestRustFivePlusFiveCommit.blockHash!=null and
    .consensusStatus.validatorCount==7 and .consensusStatus.hasCommittedQc and
    .equivocations.total==0 and .allSixCommittedBlockIdentityExact and
    .allEndpointsNotSyncing and .allChainIdsExact and
    .consensusNetworkConnectedAndQuorate
  ' "$matrix_file" >/dev/null
}

assert_wait_state
if test "$preflight_only" = 1; then
  preflight_dir="$(mktemp -d)"
  trap 'rm -rf "$preflight_dir"' EXIT
  "$auditor" "$runtime" "$preflight_dir/matrix.json" >/dev/null
  assert_matrix "$preflight_dir/matrix.json"
  assert_nonce 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
    --arg auditor "$expected_auditor_sha" \
    --argjson matrix "$(<"$preflight_dir/matrix.json")" \
    '{at:$at,event:"gov5_906_final_network_verifier_preflight",status:"PASS",
      verifierSha256:$self,auditorSha256:$auditor,matrix:$matrix,
      latestAndPendingNonce:"0x11",mutationPerformed:false}'
  exit 0
fi

while ! test -s "$total"; do
  assert_wait_state
  sleep 60
done
jq -e '
  .status=="PASS" and .latestRethExtraHourExact==true and
  .latestAndPendingNonce=="0x22" and .sourceAndRemotePinsExact==true and
  .noFailureEvidence==true
' "$total" >/dev/null
assert_wait_state

raw_dir="$(mktemp -d "$runtime/evidence/.final-network-matrix.XXXXXX")"
raw_pending="$raw_dir/matrix.json"
"$auditor" "$runtime" "$raw_pending" >/dev/null
assert_matrix "$raw_pending"
assert_nonce 0x22
mv "$raw_pending" "$raw"
rmdir "$raw_dir"

temporary="$(mktemp "$runtime/evidence/.final-network.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
  --arg auditor "$expected_auditor_sha" --arg total_sha "$(sha256 "$total")" \
  --arg matrix_sha "$(sha256 "$raw")" --argjson rust_pid "$(<"$runtime/pids/rust.pid")" \
  --slurpfile matrix "$raw" '
  {at:$at,event:"gov5_906_final_network_verification",status:"PASS",
   verifierSha256:$self,auditorSha256:$auditor,totalGoalEvidenceSha256:$total_sha,
   matrixEvidenceSha256:$matrix_sha,rustPidAfterLatestRethRollover:$rust_pid,
   matrix:$matrix[0],latestAndPendingNonce:"0x22",mutationPerformed:false,
   latestRethConsensusNetworkReestablished:true,noFailureEvidence:true}' \
  >"$temporary"
jq -e '
  .status=="PASS" and .latestAndPendingNonce=="0x22" and
  .latestRethConsensusNetworkReestablished and .matrix.status=="PASS" and
  .matrix.consensusNetworkConnectedAndQuorate and .matrix.equivocations.total==0 and
  .noFailureEvidence
' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
