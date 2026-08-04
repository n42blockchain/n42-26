#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME GOV_REPO EXPECTED_MAIN OUTPUT [EXPECTED_NONCE]}"
gov_repo="${2:?Gov5 repository is required}"
expected_main="${3:?expected Gov5 main commit is required}"
output="${4:?output evidence path is required}"
expected_nonce="${5:-0x11}"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_copied_head=92605
expected_copied_hash="0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82"
expected_builder_commit="8e1d27efb7380a3a43702bd84c78283373ccc408"
expected_tail_commit="b8c17d04614346bace2fbb5c05393bdaf454cf5a"
expected_integration_commit="8e1d27efb7380a3a43702bd84c78283373ccc408"
ports=(28501 28502 28503 28504 28505 29545)

test -d "$runtime"
git -C "$gov_repo" rev-parse --git-dir >/dev/null
test ! -e "$output"
[[ "$expected_main" =~ ^[0-9a-f]{40}$ ]]
[[ "$expected_nonce" =~ ^0x[0-9a-f]+$ ]]

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
node_rows="$temporary_dir/nodes.jsonl"
rpc_rows="$temporary_dir/rpc.jsonl"

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

block_identity() {
  local port="$1" tag="$2"
  rpc "$port" eth_getBlockByNumber \
    "$(jq -nc --arg tag "$tag" '[$tag,false]')" | jq -ec '.result | {
      number,hash,parentHash,stateRoot,receiptsRoot,transactionsRoot,miner,
      txCount:(.transactions|length)
    }'
}

remote_main="$(git -C "$gov_repo" ls-remote origin refs/heads/main | \
  awk 'NR==1{print $1}')"
test "$remote_main" = "$expected_main"
test "$(git -C "$gov_repo" rev-parse refs/remotes/origin/main)" = "$expected_main"
git -C "$gov_repo" merge-base --is-ancestor "$expected_main" HEAD

builder_commit="$(git -C "$gov_repo" log -1 --format=%H "$expected_main" -- \
  internal/txlookup/source_builder.go)"
tail_commit="$(git -C "$gov_repo" log -1 --format=%H "$expected_main" -- \
  internal/txlookup/tail.go)"
test "$builder_commit" = "$expected_builder_commit"
test "$tail_commit" = "$expected_tail_commit"

# Stage 3 wires the tail into the node, but only behind an explicit opt-in.
# The Add call happens after CommitToCanonical's MDBX transaction, so this tier
# remains off the consensus write path. Running 905-lineage data is accepted
# only when every Gov process leaves the opt-in absent and no index files were
# materialized.
integration_commit="$(git -C "$gov_repo" log -1 --format=%H "$expected_main" -- \
  internal/txindexer/indexer.go internal/node/node.go internal/blockchain.go)"
test "$integration_commit" = "$expected_integration_commit"
git -C "$gov_repo" grep -q 'N42_TXINDEX_TAIL' "$expected_main" -- \
  internal/txindexer/indexer.go
git -C "$gov_repo" grep -q 'txindexer.New' "$expected_main" -- internal/node/node.go
git -C "$gov_repo" grep -q 'bc.txIndexer.Add' "$expected_main" -- \
  internal/blockchain.go

for node in 1 2 3 4 5; do
  pid_file="$runtime/pids/gov${node}.pid"
  datadir="$runtime/gov/node${node}"
  mdbx="$datadir/chaindata/mdbx.dat"
  test -s "$pid_file"
  pid="$(<"$pid_file")"
  kill -0 "$pid"
  test -z "$(ps eww -p "$pid" -o command= | \
    rg '(^| )N42_TXINDEX_TAIL=' || true)"
  test -s "$mdbx"
  test ! -e "$datadir/txindex"
  ranges_count="$(find "$datadir" -type f -name txindex.ranges -print | wc -l | \
    tr -d ' ')"
  test "$ranges_count" = 0
  mdbx_bytes="$(stat -f %z "$mdbx")"
  mdbx_allocated_kib="$(du -k "$mdbx" | awk '{print $1}')"
  jq -nc --argjson node "$node" --argjson pid "$pid" --arg datadir "$datadir" \
    --argjson mdbx_bytes "$mdbx_bytes" \
    --argjson mdbx_allocated_kib "$mdbx_allocated_kib" \
    --argjson ranges_count "$ranges_count" \
    '{node:$node,pid:$pid,datadir:$datadir,mdbxBytes:$mdbx_bytes,
      mdbxAllocatedKiB:$mdbx_allocated_kib,txindexRangesFiles:$ranges_count,
      txindexTailEnvironmentPresent:false,txindexDirectoryPresent:false,
      processAlive:true,chaindataPresent:true}' >>"$node_rows"
done

copied_tag="$(printf '0x%x' "$expected_copied_head")"
for port in "${ports[@]}"; do
  chain_id="$(rpc "$port" eth_chainId '[]' | jq -er '.result')"
  genesis="$(block_identity "$port" 0x0)"
  copied="$(block_identity "$port" "$copied_tag")"
  latest_nonce="$(rpc "$port" eth_getTransactionCount \
    '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')"
  pending_nonce="$(rpc "$port" eth_getTransactionCount \
    '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')"
  test "$chain_id" = 0x477
  test "$(jq -r '.hash' <<<"$genesis")" = "$expected_genesis"
  test "$(jq -r '.hash' <<<"$copied")" = "$expected_copied_hash"
  test "$latest_nonce" = "$expected_nonce"
  test "$pending_nonce" = "$expected_nonce"
  jq -nc --argjson port "$port" --arg chain_id "$chain_id" \
    --argjson genesis "$genesis" --argjson copied "$copied" \
    --arg latest_nonce "$latest_nonce" --arg pending_nonce "$pending_nonce" \
    '{port:$port,chainId:$chain_id,genesis:$genesis,copiedHead:$copied,
      latestNonce:$latest_nonce,pendingNonce:$pending_nonce}' >>"$rpc_rows"
done

jq -e -s --arg genesis "$expected_genesis" --arg copied "$expected_copied_hash" \
  --arg nonce "$expected_nonce" '
  length==6 and ([.[].genesis]|unique|length)==1 and
  ([.[].copiedHead]|unique|length)==1 and
  all(.[];.chainId=="0x477" and .genesis.hash==$genesis and
    .copiedHead.hash==$copied and .latestNonce==$nonce and
    .pendingNonce==$nonce)
' "$rpc_rows" >/dev/null

latest_exact=false
latest_identity='null'
for _ in $(seq 1 30); do
  : >"$temporary_dir/latest.jsonl"
  for port in "${ports[@]}"; do
    block_identity "$port" latest >>"$temporary_dir/latest.jsonl"
  done
  if jq -e -s 'length==6 and (unique|length)==1' \
    "$temporary_dir/latest.jsonl" >/dev/null; then
    latest_exact=true
    latest_identity="$(head -n 1 "$temporary_dir/latest.jsonl")"
    break
  fi
  sleep 1
done
test "$latest_exact" = true

temporary="$(mktemp "$(dirname "$output")/.905-data-compat.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg expected_main "$expected_main" \
  --arg remote_main "$remote_main" --arg builder_commit "$builder_commit" \
  --arg tail_commit "$tail_commit" --arg integration_commit "$integration_commit" \
  --arg expected_nonce "$expected_nonce" \
  --argjson latest "$latest_identity" --slurpfile nodes "$node_rows" \
  --slurpfile rpc "$rpc_rows" '
  {at:$at,event:"gov5_905_data_compatibility_audit",status:"PASS",
   mutationPerformed:false,expectedMain:$expected_main,remoteMain:$remote_main,
   source:{variableSegmentBuilderCommit:$builder_commit,tailCommit:$tail_commit,
     nodeIntegrationCommit:$integration_commit,nodeIntegrationPresent:true,
     activationEnvironment:"N42_TXINDEX_TAIL",activationOptIn:true,
     activationAbsentInAllRunningGovProcesses:true,runtimeTailEnabled:false,
     variableSegmentsWiredToConsensus:false,inMemoryTailWiredToConsensus:false,
     txindexRangesExpectedInRunning905Data:false},nodes:$nodes,rpcEndpoints:$rpc,
   copiedPersistedHead:92605,latestSixEndpointIdentity:$latest,
   latestAndPendingNonce:$expected_nonce,allFiveProcessesAlive:true,
   allFiveChaindataPresent:true,allFiveTxindexRangesAbsent:true,
   genesisAndCopiedHeadSixEndpointExact:true,liveSixEndpointIdentityExact:true,
   dataRecopyOrRegenerationRequired:false}' >"$temporary"
jq -e '.status=="PASS" and .mutationPerformed==false and
  .source.nodeIntegrationPresent and .source.activationOptIn and
  .source.activationAbsentInAllRunningGovProcesses and
  .source.runtimeTailEnabled==false and
  .source.variableSegmentsWiredToConsensus==false and
  .source.inMemoryTailWiredToConsensus==false and
  .allFiveTxindexRangesAbsent and .genesisAndCopiedHeadSixEndpointExact and
  .liveSixEndpointIdentityExact and .dataRecopyOrRegenerationRequired==false' \
  "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
