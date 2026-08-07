#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_BOUNDARY_RUNTIME:?runtime is required}"
expected_self_sha="${N42_BOUNDARY_EXPECTED_SELF_SHA:?verifier SHA-256 is required}"
expected_evidence_sha="${N42_BOUNDARY_EXPECTED_EVIDENCE_SHA:?boundary evidence SHA-256 is required}"
preflight_only="${N42_BOUNDARY_PREFLIGHT_ONLY:-0}"
boundary="$runtime/evidence/runtime28-copied-905-boundary-block-identity-recheck.json"
total="$runtime/evidence/gov5-906-total-goal-final-verification.json"
output="$runtime/evidence/gov5-906-copied-boundary-final-verification.json"
failure="$runtime/evidence/gov5-906-copied-boundary-final-verification-failure.json"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
ports=(28501 28502 28503 28504 28505 29545)

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

block_identity() {
  local port="$1" tag="$2"
  rpc "$port" eth_getBlockByNumber "$(jq -nc --arg tag "$tag" '[$tag,false]')" |
    jq -ec '.result | {
      number,hash,parentHash,stateRoot,receiptsRoot,transactionsRoot,miner,
      txCount:(.transactions|length)
    }'
}

on_error() {
  local status=$? line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --argjson status "$status" \
    --argjson line "$line" --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_906_copied_boundary_final_verification_failure",
      status:"FAIL",statusCode:$status,line:$line,command:$command}' >"$failure"
  exit "$status"
}
trap on_error ERR

assert_nodes() {
  local file pid
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid"; do
    test -s "$file"
    pid="$(<"$file")"
    kill -0 "$pid"
  done
}

assert_no_failures() {
  local path
  for path in \
    "$runtime/evidence/gov5-906-finalizer-failures.jsonl" \
    "$runtime/evidence/gov5-906-independent-final-verification-failure.json" \
    "$runtime/evidence/official-reth-stable/latest-reth-failures.jsonl" \
    "$runtime/evidence/official-reth-stable/latest-reth-independent-final-verification-failure.json" \
    "$runtime/evidence/gov5-906-total-goal-final-verification-failure.json"; do
    test ! -s "$path"
  done
}

assert_static() {
  test "$(sha256 "${BASH_SOURCE[0]}")" = "$expected_self_sha"
  test -s "$boundary"
  test "$(sha256 "$boundary")" = "$expected_evidence_sha"
  jq -e --arg genesis "$expected_genesis" '
    .event=="runtime28_copied_905_boundary_block_identity_recheck" and
    .status=="PASS" and .sourcePersistedHead==92605 and
    .allSixExactAtEveryHeight==true and (.ports|length)==6 and
    (.blocks|length)==7 and all(.blocks[];.allSixExact==true) and
    (.blocks|map(select(.height==0 and .identity.hash==$genesis))|length)==1 and
    (.blocks|map(select(.height==92605 and
      .identity.hash=="0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82"))|length)==1
  ' "$boundary" >/dev/null
}

assert_historical_boundaries() {
  local row height tag stored live port
  while IFS= read -r row; do
    height="$(jq -er '.height' <<<"$row")"
    printf -v tag '0x%x' "$height"
    stored="$(jq -ec '.identity' <<<"$row")"
    for port in "${ports[@]}"; do
      live="$(block_identity "$port" "$tag")"
      test "$live" = "$stored"
    done
  done < <(jq -c '.blocks[]' "$boundary")
}

assert_live() {
  local nonce_expected="$1" expected="" identity="" exact=false
  local attempt port genesis nonce pending
  for port in "${ports[@]}"; do
    genesis="$(block_identity "$port" 0x0 | jq -er '.hash')"
    test "$genesis" = "$expected_genesis"
    nonce="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')"
    pending="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')"
    test "$nonce" = "$nonce_expected"
    test "$pending" = "$nonce_expected"
  done
  for attempt in $(seq 1 30); do
    expected=""
    exact=true
    for port in "${ports[@]}"; do
      identity="$(block_identity "$port" latest)"
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

assert_static
assert_nodes
assert_no_failures
assert_historical_boundaries

if test "$preflight_only" = 1; then
  assert_live 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
    --arg boundary_sha "$expected_evidence_sha" \
    '{at:$at,event:"gov5_906_copied_boundary_final_verifier_preflight",
      status:"PASS",verifierSha256:$self,boundaryEvidenceSha256:$boundary_sha,
      historicalBoundariesReexecuted:true,liveSixEndpointIdentityExact:true,
      latestAndPendingNonce:"0x11",mutationPerformed:false}'
  exit 0
fi

while ! test -s "$total"; do
  assert_static
  assert_nodes
  assert_no_failures
  assert_historical_boundaries
  sleep 60
done

jq -e '.status=="PASS" and .latestRethExtraHourExact==true and
  .latestAndPendingNonce=="0x22" and .sourceAndRemotePinsExact==true and
  .noFailureEvidence==true' "$total" >/dev/null
assert_static
assert_nodes
assert_no_failures
assert_historical_boundaries
assert_live 0x22

latest="$(block_identity 29545 latest)"
consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
temporary="$(mktemp "$runtime/evidence/.copied-boundary-final.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$expected_self_sha" \
  --arg boundary_sha "$expected_evidence_sha" --arg total_sha "$(sha256 "$total")" \
  --argjson latest "$latest" --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" \
  '{at:$at,event:"gov5_906_copied_boundary_final_verification",status:"PASS",
    verifierSha256:$self,boundaryEvidenceSha256:$boundary_sha,
    totalGoalEvidenceSha256:$total_sha,sourcePersistedHead:92605,
    historicalBlocksReexecuted:7,historicalFieldsPerBlock:8,
    allSixHistoricalBoundariesExact:true,genesisExact:true,
    latestRethPostRolloverExact:true,latestAndPendingNonce:"0x22",
    finalCanonicalHead:$latest,consensus:$consensus,equivocations:$equivocations,
    noFailureEvidence:true}' >"$temporary"
jq -e '.status=="PASS" and .historicalBlocksReexecuted==7 and
  .allSixHistoricalBoundariesExact==true and .latestRethPostRolloverExact==true and
  .latestAndPendingNonce=="0x22" and .equivocations.total==0' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
