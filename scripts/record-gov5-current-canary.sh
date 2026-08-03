#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?runtime required}"
expected_gov_main="${2:?expected Gov5 main required}"
expected_gov_candidate="${3:?expected Gov5 candidate required}"
expected_gov_sha="${4:?expected Gov5 binary SHA-256 required}"
expected_rust_sha="${5:?expected Rust binary SHA-256 required}"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
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

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

for _ in $(seq 1 180); do
  : >"$work_dir/endpoints.jsonl"
  expected=""
  exact=true
  for port in "${ports[@]}"; do
    latest="$(rpc "$port" eth_getBlockByNumber '["latest",false]')"
    genesis_response="$(rpc "$port" eth_getBlockByNumber '["0x0",false]')"
    item="$(jq -nc --argjson port "$port" --argjson latest "$latest" \
      --argjson genesis_response "$genesis_response" \
      '{port:$port,number:$latest.result.number,hash:$latest.result.hash,
        stateRoot:$latest.result.stateRoot,receiptsRoot:$latest.result.receiptsRoot,
        miner:$latest.result.miner,genesis:$genesis_response.result.hash}')"
    identity="$(jq -r '[.number,.hash,.stateRoot,.receiptsRoot] | join(":")' <<<"$item")"
    test "$(jq -r '.genesis' <<<"$item")" = "$genesis"
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
  rg 'block committed! view=.*votes=5\+5' "$runtime/logs/rust.log" |
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
  --slurpfile endpoints "$work_dir/endpoints.jsonl" \
  --slurpfile rust_leaders "$work_dir/rust-leaders.json" \
  --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" \
  '{at:$at,event:"gov5_current_mixed_client_canary",status:"PASS",
    gov5:{main:$expected_gov_main,candidate:$expected_gov_candidate,
      binarySha256:$expected_gov_sha},rust:{binarySha256:$expected_rust_sha},
    endpoints:$endpoints,rustCommits:$rust_leaders[0],consensus:$consensus,
    equivocations:$equivocations,allSixLatestExact:true,genesisExact:true,
    rustFivePlusFive:true}'
