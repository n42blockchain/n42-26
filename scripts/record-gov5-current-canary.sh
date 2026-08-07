#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?runtime required}"
expected_gov_main="${2:?expected Gov5 main required}"
expected_gov_candidate="${3:?expected Gov5 candidate required}"
expected_gov_sha="${4:?expected Gov5 binary SHA-256 required}"
expected_rust_sha="${5:?expected Rust binary SHA-256 required}"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
genesis_state_root="0x91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941"
genesis_receipts_root="0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
chain_id="0x477"
sender="0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
expected_sender_nonce="${N42_EXPECTED_SENDER_NONCE:-0x11}"
ports=(28501 28502 28503 28504 28505 29545)

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 5 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

test "$(git -C "$gov_repo" rev-parse HEAD)" = "$expected_gov_candidate"
test "$(git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}')" = \
  "$expected_gov_main"
test "$(shasum -a 256 "$runtime/geth-live" | awk '{print $1}')" = "$expected_gov_sha"
test "$(shasum -a 256 "$runtime/n42-node" | awk '{print $1}')" = "$expected_rust_sha"
recorder_sha="$(shasum -a 256 "${BASH_SOURCE[0]}" | awk '{print $1}')"

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

for _ in $(seq 1 180); do
  : >"$work_dir/endpoints.jsonl"
  expected=""
  exact=true
  for port in "${ports[@]}"; do
    latest="$(rpc "$port" eth_getBlockByNumber '["latest",false]')"
    genesis_response="$(rpc "$port" eth_getBlockByNumber '["0x0",false]')"
    endpoint_chain_id="$(rpc "$port" eth_chainId '[]' | jq -er '.result')"
    latest_nonce="$(rpc "$port" eth_getTransactionCount \
      "$(jq -nc --arg sender "$sender" '[$sender,"latest"]')" | jq -er '.result')"
    pending_nonce="$(rpc "$port" eth_getTransactionCount \
      "$(jq -nc --arg sender "$sender" '[$sender,"pending"]')" | jq -er '.result')"
    client_version="$(rpc "$port" web3_clientVersion '[]' | jq -er '.result')"
    item="$(jq -nc --argjson port "$port" --argjson latest "$latest" \
      --argjson genesis_response "$genesis_response" \
      --arg chain_id "$endpoint_chain_id" --arg latest_nonce "$latest_nonce" \
      --arg pending_nonce "$pending_nonce" --arg client_version "$client_version" \
      '{port:$port,number:$latest.result.number,hash:$latest.result.hash,
        stateRoot:$latest.result.stateRoot,receiptsRoot:$latest.result.receiptsRoot,
        miner:$latest.result.miner,chainId:$chain_id,
        latestNonce:$latest_nonce,pendingNonce:$pending_nonce,
        clientVersion:$client_version,genesis:$genesis_response.result.hash,
        genesisStateRoot:$genesis_response.result.stateRoot,
        genesisReceiptsRoot:$genesis_response.result.receiptsRoot}')"
    identity="$(jq -r '[.number,.hash,.stateRoot,.receiptsRoot] | join(":")' <<<"$item")"
    test "$endpoint_chain_id" = "$chain_id"
    test "$latest_nonce" = "$expected_sender_nonce"
    test "$pending_nonce" = "$expected_sender_nonce"
    test "$(jq -r '.genesis' <<<"$item")" = "$genesis"
    test "$(jq -r '.genesisStateRoot' <<<"$item")" = "$genesis_state_root"
    test "$(jq -r '.genesisReceiptsRoot' <<<"$item")" = "$genesis_receipts_root"
    if test -z "$expected"; then
      expected="$identity"
    elif test "$identity" != "$expected"; then
      exact=false
    fi
    printf '%s\n' "$item" >>"$work_dir/endpoints.jsonl"
  done
  if test "$exact" = true; then
    break
  fi
  sleep 1
done
test "$exact" = true

for _ in $(seq 1 180); do
  (rg 'block committed! view=.*votes=5\+5' "$runtime/logs/rust.log" || true) |
    jq -Rc 'capture("view=(?<view>[0-9]+) block_hash=(?<hash>0x[0-9a-f]{64}).*votes=(?<votes>[0-9]+[+][0-9]+)") |
      {view:(.view|tonumber),hash,votes}' |
    jq -sc 'unique_by(.view) | sort_by(.view)' >"$work_dir/rust-leaders.json"
  if test "$(jq 'length' "$work_dir/rust-leaders.json")" -ge 2; then
    break
  fi
  sleep 1
done
test "$(jq 'length' "$work_dir/rust-leaders.json")" -ge 2

consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
jq -e '.validatorCount == 7 and .hasCommittedQc == true' <<<"$consensus" >/dev/null
jq -e '.total == 0 and (.evidence | length) == 0' <<<"$equivocations" >/dev/null

jq -n \
  --arg at "$(date -u +%FT%TZ)" \
  --arg expected_gov_main "$expected_gov_main" \
  --arg expected_gov_candidate "$expected_gov_candidate" \
  --arg expected_gov_sha "$expected_gov_sha" \
  --arg expected_rust_sha "$expected_rust_sha" \
  --arg recorder_sha "$recorder_sha" \
  --arg chain_id "$chain_id" \
  --arg genesis "$genesis" \
  --arg genesis_state_root "$genesis_state_root" \
  --arg genesis_receipts_root "$genesis_receipts_root" \
  --arg sender "$sender" \
  --arg expected_sender_nonce "$expected_sender_nonce" \
  --slurpfile endpoints "$work_dir/endpoints.jsonl" \
  --slurpfile rust_leaders "$work_dir/rust-leaders.json" \
  --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" \
  '{at:$at,event:"gov5_current_mixed_client_canary",status:"PASS",
    recorderScriptSha256:$recorder_sha,
    gov5:{main:$expected_gov_main,candidate:$expected_gov_candidate,
      binarySha256:$expected_gov_sha},rust:{binarySha256:$expected_rust_sha},
    chain:{chainId:$chain_id,genesisHash:$genesis,
      genesisStateRoot:$genesis_state_root,
      genesisReceiptsRoot:$genesis_receipts_root},
    sender:{address:$sender,latestAndPendingNonce:$expected_sender_nonce},
    endpoints:$endpoints,rustCommits:$rust_leaders[0],consensus:$consensus,
    equivocations:$equivocations,allSixLatestExact:true,genesisExact:true,
    allSixChainIdsExact:true,allSixGenesisRootsExact:true,
    allSixLatestAndPendingNoncesExact:true,rustFivePlusFive:true}'
