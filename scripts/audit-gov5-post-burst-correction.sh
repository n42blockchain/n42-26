#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:?runtime is required}"
failure="${N42_QUAL_PRIOR_FINALIZER_FAILURE:-$runtime/evidence/gov5-906-finalizer-failures.jsonl}"
burst="${N42_QUAL_BURST_EVIDENCE:-$runtime/evidence/p4-transaction-burst-906.jsonl}"
artifact="${N42_QUAL_BURST_ARTIFACT:-$runtime/artifacts/p4-signed-transaction-burst.json}"
harness="${N42_QUAL_HARNESS:-$runtime/artifacts/scripts/gov5-interop-qualification.sh}"
output="${N42_QUAL_CORRECTION_OUTPUT:-$runtime/evidence/gov5-906-post-burst-correction.json}"
expected_failure_sha="${N42_QUAL_EXPECTED_PRIOR_FAILURE_SHA:?prior failure SHA-256 is required}"
expected_burst_sha="${N42_QUAL_EXPECTED_PRIOR_BURST_SHA:?prior 17-row burst SHA-256 is required}"
expected_old_harness_sha="${N42_QUAL_EXPECTED_OLD_HARNESS_SHA:?old harness SHA-256 is required}"
expected_new_harness_sha="${N42_QUAL_EXPECTED_NEW_HARNESS_SHA:?new harness SHA-256 is required}"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
expected_storage=0x0000000000000000000000000000000000000000000000000000000000000001
sender="$(jq -er '.sender' "$artifact")"
contract="$(jq -er '.expectedContract' "$artifact")"

sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

test ! -e "$output"
test "$(sha256 "$failure")" = "$expected_failure_sha"
test "$(sha256 "$burst")" = "$expected_burst_sha"
test "$(sha256 "$harness")" = "$expected_new_harness_sha"
test "$expected_old_harness_sha" != "$expected_new_harness_sha"
jq -e -s '
  length == 1 and .[0].event == "gov5_906_finalizer_failure" and
  .[0].statusCode == 1 and (.[0].command | contains("transaction-burst"))
' "$failure" >/dev/null
jq -e -s --slurpfile artifact "$artifact" '
  (map(select(.event == "p4_transaction_finalized"))) as $txs |
  length == 17 and ($txs | length) == 17 and
  (map(select(.event == "p4_transaction_burst_pass")) | length) == 0 and
  ([range(0; 17) as $i |
    $txs[$i].nonce == $artifact[0].transactions[$i].nonce and
    $txs[$i].kind == $artifact[0].transactions[$i].kind and
    $txs[$i].ingress == $artifact[0].transactions[$i].intendedIngress and
    $txs[$i].transactionHash == $artifact[0].transactions[$i].hash and
    $txs[$i].status == "0x1"] | all)
' "$burst" >/dev/null

first_block="$(jq -ers '.[0].blockNumber' "$burst")"
deploy_block="$(jq -ers 'map(select(.kind == "contract_deploy"))[0].blockNumber' "$burst")"
last_block="$(jq -ers '.[-1].blockNumber' "$burst")"
checks="$(mktemp)"
trap 'rm -f "$checks"' EXIT

for port in $ports; do
  latest_nonce="$(rpc "$port" eth_getTransactionCount \
    "$(jq -nc --arg sender "$sender" '[ $sender, "latest" ]')" | jq -er '.result')"
  pending_nonce="$(rpc "$port" eth_getTransactionCount \
    "$(jq -nc --arg sender "$sender" '[ $sender, "pending" ]')" | jq -er '.result')"
  test "$latest_nonce" = 0x22
  test "$pending_nonce" = 0x22
  for block in "$deploy_block" "$last_block" latest; do
    storage="$(rpc "$port" eth_getStorageAt \
      "$(jq -nc --arg contract "$contract" --arg block "$block" \
        '[ $contract, "0x0", $block ]')" | jq -er '.result')"
    test "$storage" = "$expected_storage"
    jq -nc --argjson port "$port" --arg block "$block" --arg storage "$storage" \
      --arg latest_nonce "$latest_nonce" --arg pending_nonce "$pending_nonce" \
      '{port:$port,block:$block,storage:$storage,
        latestNonce:$latest_nonce,pendingNonce:$pending_nonce}' >>"$checks"
  done
done

while IFS= read -r hash; do
  reference=""
  for port in $ports; do
    receipt="$(rpc "$port" eth_getTransactionReceipt \
      "$(jq -nc --arg hash "$hash" '[$hash]')" | jq -ecS '.result')"
    jq -e '.status == "0x1" and .blockHash != null and .blockNumber != null' \
      <<<"$receipt" >/dev/null
    if test -z "$reference"; then
      reference="$receipt"
    else
      test "$receipt" = "$reference"
    fi
  done
done < <(jq -r '.transactions[].hash' "$artifact")

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg failure "$failure" --arg failure_sha "$expected_failure_sha" \
  --arg burst "$burst" --arg burst_sha "$expected_burst_sha" \
  --arg artifact "$artifact" --arg artifact_sha "$(sha256 "$artifact")" \
  --arg old_harness_sha "$expected_old_harness_sha" \
  --arg new_harness_sha "$expected_new_harness_sha" \
  --arg sender "$sender" --arg contract "$contract" \
  --arg first_block "$first_block" --arg deploy_block "$deploy_block" \
  --arg last_block "$last_block" --arg storage "$expected_storage" \
  --slurpfile checks "$checks" '
  {at:$at,event:"gov5_906_post_burst_correction",status:"PASS",
   acceptanceRelaxed:false,
   priorFinalizerFailure:{path:$failure,sha256:$failure_sha,preserved:true},
   finalizedBurstBeforeCorrection:{path:$burst,sha256:$burst_sha,rows:17,
     finalizedTransactions:17,passRows:0},
   artifact:{path:$artifact,sha256:$artifact_sha},
   tooling:{oldHarnessSha256:$old_harness_sha,newHarnessSha256:$new_harness_sha,
     oldComparisonUsedJsonEncodedString:true,newComparisonUsesRawString:true},
   rootCause:"old harness compared jq -c JSON string output with a raw hex string",
   sender:$sender,contract:$contract,firstBlock:$first_block,
   deployBlock:$deploy_block,lastBlock:$last_block,
   expectedStorage:$storage,endpointChecks:$checks,
   allSeventeenReceiptsExactAcrossEndpoints:true,
   allEndpointLatestAndPendingNoncesExact:true,expectedNonce:"0x22",
   deployBlockLastBlockAndLatestStorageExact:true,
   transactionsResent:0,chainDataMutationPerformed:false,
   nodeOrMonitorMutationPerformed:false,resumeRequired:true}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
