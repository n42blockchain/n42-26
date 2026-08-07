#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME OUTPUT}"
output="${2:?usage: $0 RUNTIME OUTPUT}"
artifact="$runtime/artifacts/p4-signed-transaction-burst.json"
ports=(28501 28502 28503 28504 28505 29545)
expected_sender=0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
expected_recipient=0x000000000000000000000000000000000000dead

test -s "$artifact"
test ! -e "$output"
command -v cast >/dev/null
command -v jq >/dev/null
command -v curl >/dev/null

audit_dir="$(mktemp -d)"
trap 'rm -rf "$audit_dir"' EXIT
decoded="$audit_dir/decoded.jsonl"
rpc_rows="$audit_dir/rpc.jsonl"

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

count="$(jq -er '.transactions | length' "$artifact")"
test "$count" -eq 17
for index in $(seq 0 $((count - 1))); do
  tx="$(jq -ec --argjson index "$index" '.transactions[$index]' "$artifact")"
  raw="$(jq -r '.raw' <<<"$tx")"
  expected_nonce=$((17 + index))
  decoded_tx="$(cast decode-transaction --json "$raw" | jq -ec '.')"
  test "$(jq -r '.signer' <<<"$decoded_tx")" = "$expected_sender"
  test "$(jq -r '.chainId' <<<"$decoded_tx")" = 0x477
  test "$(jq -r '.nonce' <<<"$decoded_tx")" = "$(printf '0x%x' "$expected_nonce")"
  test "$(jq -r '.hash' <<<"$decoded_tx")" = "$(jq -r '.hash' <<<"$tx")"
  test "$(cast keccak "$raw")" = "$(jq -r '.hash' <<<"$tx")"
  if test "$index" -eq 0; then
    test "$(jq -r '.to' <<<"$decoded_tx")" = null
    test "$(jq -r '.gas' <<<"$decoded_tx")" = 0x186a0
    test "$(jq -r '.input' <<<"$decoded_tx")" = \
      "$(jq -r '.constructorInitcode' "$artifact")"
  else
    test "$(jq -r '.to' <<<"$decoded_tx")" = "$expected_recipient"
    test "$(jq -r '.gas' <<<"$decoded_tx")" = 0x5208
    test "$(jq -r '.value' <<<"$decoded_tx")" = "$(printf '0x%x' "$expected_nonce")"
  fi
  jq -nc --argjson index "$index" --arg intended "$(jq -r '.intendedIngress' <<<"$tx")" \
    --arg kind "$(jq -r '.kind' <<<"$tx")" --argjson decoded "$decoded_tx" \
    '{index:$index,kind:$kind,intendedIngress:$intended,decoded:$decoded}' >>"$decoded"
done

jq -e -s '
  length==17 and
  (map(select(.intendedIngress=="rust"))|length)==9 and
  (map(select(.intendedIngress=="gov"))|length)==8 and
  (map(.decoded.hash)|unique|length)==17
' "$decoded" >/dev/null

deploy_request="$(jq -nc --arg from "$expected_sender" \
  --arg data "$(jq -r '.constructorInitcode' "$artifact")" \
  '[{from:$from,data:$data,value:"0x0",gasPrice:"0x3b9aca07"},"latest"]')"
transfer_request="$(jq -nc --arg from "$expected_sender" --arg to "$expected_recipient" \
  '[{from:$from,to:$to,value:"0x12",gasPrice:"0x3b9aca07"},"latest"]')"

for port in "${ports[@]}"; do
  latest="$(rpc "$port" eth_getTransactionCount \
    "[\"$expected_sender\",\"latest\"]" | jq -er '.result')"
  pending="$(rpc "$port" eth_getTransactionCount \
    "[\"$expected_sender\",\"pending\"]" | jq -er '.result')"
  test "$latest" = 0x11
  test "$pending" = 0x11

  deploy_call="$(rpc "$port" eth_call "$deploy_request" | jq -er 'select(.error==null)|.result')"
  deploy_estimate="$(rpc "$port" eth_estimateGas "$deploy_request" | \
    jq -er 'select(.error==null)|.result')"
  transfer_call="$(rpc "$port" eth_call "$transfer_request" | jq -er 'select(.error==null)|.result')"
  transfer_estimate="$(rpc "$port" eth_estimateGas "$transfer_request" | \
    jq -er 'select(.error==null)|.result')"
  test "$((deploy_estimate))" -le $((0x186a0))
  test "$((transfer_estimate))" -le $((0x5208))
  jq -nc --argjson port "$port" --arg latest "$latest" --arg pending "$pending" \
    --arg deploy_call "$deploy_call" --arg deploy_estimate "$deploy_estimate" \
    --arg transfer_call "$transfer_call" --arg transfer_estimate "$transfer_estimate" \
    '{port:$port,latestNonce:$latest,pendingNonce:$pending,
      contractDeploy:{callResult:$deploy_call,estimatedGas:$deploy_estimate,gasLimit:"0x186a0"},
      valueTransfer:{callResult:$transfer_call,estimatedGas:$transfer_estimate,gasLimit:"0x5208"}}' \
    >>"$rpc_rows"
done

jq -e -s '
  length==6 and all(.[];
    .latestNonce=="0x11" and .pendingNonce=="0x11" and
    .contractDeploy.callResult=="0x" and .valueTransfer.callResult=="0x") and
  ([.[].valueTransfer.estimatedGas]|unique)==["0x5208"]
' "$rpc_rows" >/dev/null

temporary="$(mktemp "$(dirname "$output")/.burst-readonly.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg artifact "$artifact" \
  --arg artifact_sha "$(shasum -a 256 "$artifact" | awk '{print $1}')" \
  --arg cast_version "$(cast --version | head -n 1)" \
  --slurpfile decoded "$decoded" --slurpfile rpc "$rpc_rows" '
  {at:$at,event:"gov5_burst_readonly_audit",status:"PASS",mutationPerformed:false,
   transactionsSent:0,artifact:$artifact,artifactSha256:$artifact_sha,
   decoder:$cast_version,transactionsDecoded:($decoded|length),
   sender:$decoded[0].decoded.signer,chainId:$decoded[0].decoded.chainId,
   firstNonce:$decoded[0].decoded.nonce,lastNonce:$decoded[-1].decoded.nonce,
   transactionHashes:[$decoded[].decoded.hash],
   intendedIngressCounts:{rust:([$decoded[]|select(.intendedIngress=="rust")]|length),
     gov:([$decoded[]|select(.intendedIngress=="gov")]|length)},
   allSignaturesRecoverExpectedSender:true,allRawHashesExact:true,
   allNoncesContiguous:true,allChainIdsExact:true,rpcEndpoints:$rpc,
   allEndpointNoncesExact:true,allCallsSucceeded:true,allEstimatesWithinSignedGas:true}' \
  >"$temporary"
jq -e '
  .status=="PASS" and .mutationPerformed==false and .transactionsSent==0 and
  .transactionsDecoded==17 and .firstNonce=="0x11" and .lastNonce=="0x21" and
  .intendedIngressCounts=={rust:9,gov:8} and .allSignaturesRecoverExpectedSender and
  .allRawHashesExact and .allNoncesContiguous and .allChainIdsExact and
  .allEndpointNoncesExact and .allCallsSucceeded and .allEstimatesWithinSignedGas
' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
