#!/usr/bin/env bash
set -euo pipefail

artifact="${1:?signed transaction artifact required}"
cast_bin="${N42_CAST_BINARY:-cast}"

test -f "$artifact"
command -v "$cast_bin" >/dev/null
command -v jq >/dev/null

expected_sender="0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
expected_chain_id="0x477"
expected_recipient="0x000000000000000000000000000000000000dead"
expected_gas_price="0x3b9aca07"
expected_initcode="0x600160005560006000a060006000f3"

jq -e --arg sender "$expected_sender" '
  .formatVersion == 2 and .chainId == 1143 and
  (.sender | ascii_downcase) == $sender and
  (.transactions | length) == 17 and
  [.transactions[].nonce] == [range(17; 34)]
' "$artifact" >/dev/null

sender="$(jq -er '.sender | ascii_downcase' "$artifact")"
expected_contract="$(jq -er '.expectedContract | ascii_downcase' "$artifact")"
computed_contract="$($cast_bin compute-address "$sender" --nonce 17 |
  sed -E -n 's/^Computed Address: (0x[0-9A-Fa-f]{40})$/\1/p' |
  tr '[:upper:]' '[:lower:]')"
test "$computed_contract" = "$expected_contract"

gov_count=0
rust_count=0
for index in $(seq 0 16); do
  raw="$(jq -er --argjson i "$index" '.transactions[$i].raw' "$artifact")"
  expected_hash="$(jq -er --argjson i "$index" '.transactions[$i].hash' "$artifact")"
  expected_nonce="$(jq -er --argjson i "$index" '.transactions[$i].nonce' "$artifact")"
  intended_ingress="$(jq -er --argjson i "$index" '.transactions[$i].intendedIngress' "$artifact")"
  decoded="$($cast_bin decode-transaction --json "$raw")"
  actual_hash="$($cast_bin keccak "$raw")"
  printf -v nonce_hex '0x%x' "$expected_nonce"

  test "$actual_hash" = "$expected_hash"
  jq -e --arg sender "$expected_sender" --arg chain_id "$expected_chain_id" \
    --arg nonce "$nonce_hex" --arg hash "$expected_hash" \
    --arg gas_price "$expected_gas_price" '
    (.signer | ascii_downcase) == $sender and .type == "0x0" and
    .chainId == $chain_id and .nonce == $nonce and .hash == $hash and
    .gasPrice == $gas_price
  ' <<<"$decoded" >/dev/null

  if test "$index" -eq 0; then
    test "$intended_ingress" = rust
    jq -e --arg initcode "$expected_initcode" '
      .to == null and .value == "0x0" and .gas == "0x186a0" and
      .input == $initcode
    ' <<<"$decoded" >/dev/null
    rust_count=$((rust_count + 1))
  else
    printf -v value_hex '0x%x' "$expected_nonce"
    jq -e --arg recipient "$expected_recipient" --arg value "$value_hex" '
      (.to | ascii_downcase) == $recipient and .value == $value and
      .gas == "0x5208" and .input == "0x"
    ' <<<"$decoded" >/dev/null
    if test $((expected_nonce % 2)) -eq 0; then
      test "$intended_ingress" = gov
      gov_count=$((gov_count + 1))
    else
      test "$intended_ingress" = rust
      rust_count=$((rust_count + 1))
    fi
  fi
done

test "$gov_count" -eq 8
test "$rust_count" -eq 9

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg artifact "$artifact" \
  --arg artifact_sha "$(shasum -a 256 "$artifact" | awk '{print $1}')" \
  --arg cast_version "$($cast_bin --version | head -n 1)" \
  --arg sender "$expected_sender" \
  --arg chain_id "$expected_chain_id" \
  --arg contract "$expected_contract" \
  --argjson gov_count "$gov_count" \
  --argjson rust_count "$rust_count" '
  {at:$at,event:"signed_transaction_burst_offline_verification",status:"PASS",
    artifact:$artifact,artifactSha256:$artifact_sha,castVersion:$cast_version,
    transactions:17,signer:$sender,chainId:$chain_id,
    nonceRange:{first:17,last:33,continuous:true},
    allRawTransactionHashesRecomputedExact:true,
    allSignersRecoveredExact:true,allEip155ChainIdsExact:true,
    allGasAndPayloadSemanticsExact:true,
    expectedContract:$contract,createAddressRecomputedExact:true,
    intendedIngressCounts:{gov:$gov_count,rust:$rust_count},
    intendedIngressAlternationExact:true,transactionsSent:0}'
