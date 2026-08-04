#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME OUTPUT}"
output="${2:?usage: $0 RUNTIME OUTPUT}"
expected_rust_client="${N42_NETWORK_EXPECTED_RUST_CLIENT:-reth/v2.4.1-91725e3/aarch64-apple-darwin}"
audit_label="${N42_NETWORK_AUDIT_LABEL:-}"
ports=(28501 28502 28503 28504 28505 29545)
expected_remote_ports='[30301,30302,30303,30304,30305]'

test -d "$runtime"
test ! -e "$output"
test -s "$runtime/pids/rust.pid"
rust_pid="$(<"$runtime/pids/rust.pid")"
kill -0 "$rust_pid"

audit_dir="$(mktemp -d)"
trap 'rm -rf "$audit_dir"' EXIT
rpc_rows="$audit_dir/rpc.jsonl"
authenticated_rows="$audit_dir/authenticated.jsonl"
committed_rows="$audit_dir/committed.jsonl"

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

for port in "${ports[@]}"; do
  client="$(rpc "$port" web3_clientVersion '[]' | jq -er '.result')"
  peers="$(rpc "$port" net_peerCount '[]' | jq -er '.result')"
  # `jq -e` maps the valid JSON value false to exit status 1. Read it as JSON
  # here and assert false over the complete matrix below.
  syncing="$(rpc "$port" eth_syncing '[]' | jq -c '.result')"
  chain="$(rpc "$port" eth_chainId '[]' | jq -er '.result')"
  jq -nc --argjson port "$port" --arg client "$client" --arg peers "$peers" \
    --argjson syncing "$syncing" --arg chain "$chain" \
    '{port:$port,clientVersion:$client,executionPeerCountHex:$peers,
      syncing:$syncing,chainId:$chain}' >>"$rpc_rows"
done

jq -e -s --arg rust "$expected_rust_client" '
  length==6 and all(.[];.syncing==false and .chainId=="0x477") and
  all(.[:5][];.clientVersion=="N42/5.7.906" and
    .executionPeerCountHex=="0x5") and
  .[5].clientVersion==$rust and .[5].executionPeerCountHex=="0x0"
' "$rpc_rows" >/dev/null

remote_ports="$(lsof -nP -a -p "$rust_pid" -iTCP 2>/dev/null | sed -E -n \
  's#.*->127\.0\.0\.1:([0-9]+) \(ESTABLISHED\).*#\1#p' | sort -n | \
  jq -R 'tonumber' | jq -sc '.')"
test "$(jq -c . <<<"$remote_ports")" = "$expected_remote_ports"

rg 'peer promoted to authenticated validator peer_id=.* validator_index=' \
  "$runtime/logs/rust.log" | sed -E -n \
  's/.*peer_id=([^ ]+) validator_index=([0-9]+).*/\1 \2/p' | sort -u | \
  awk '{printf "{\"peerId\":\"%s\",\"validatorIndex\":%s}\n",$1,$2}' \
  >"$authenticated_rows"
jq -e -s '
  length==5 and ([.[].peerId]|unique|length)==5 and
  ([.[].validatorIndex]|unique|sort)==[1,2,3,4,5]
' "$authenticated_rows" >/dev/null

quorum_line="$(rg 'validator peer quorum reached for leader build' \
  "$runtime/logs/rust.log" | tail -n 1)"
connected="$(sed -E -n \
  's/.*connected_validator_peers=([0-9]+).*/\1/p' <<<"$quorum_line")"
needed="$(sed -E -n \
  's/.*needed_quorum_peers=([0-9]+).*/\1/p' <<<"$quorum_line")"
test "$connected" = 5
test "$needed" = 4

direct_line="$(rg 'N42_DIRECT_PUSH: sent to all validator peers' \
  "$runtime/logs/rust.log" | tail -n 1)"
direct="$(sed -E -n 's/.*direct_count=([0-9]+).*/\1/p' <<<"$direct_line")"
test "$direct" = 5

commit_line="$(rg ' INFO (.*: )?block committed! view=.*votes=5\+5$' \
  "$runtime/logs/rust.log" | tail -n 1)"
commit_view="$(sed -E -n \
  's/.*view=([0-9]+) block_hash=.*/\1/p' <<<"$commit_line")"
commit_hash="$(sed -E -n \
  's/.*block_hash=(0x[0-9a-f]{64}).*/\1/p' <<<"$commit_line")"
[[ "$commit_view" =~ ^[0-9]+$ ]]
[[ "$commit_hash" =~ ^0x[0-9a-f]{64}$ ]]

consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
jq -e '.validatorCount==7 and .hasCommittedQc==true' <<<"$consensus" >/dev/null
jq -e '.total==0 and (.evidence|length)==0' <<<"$equivocations" >/dev/null

committed_hash="$(jq -r '.latestCommittedBlockHash' <<<"$consensus")"
for port in "${ports[@]}"; do
  rpc "$port" eth_getBlockByHash "[\"$committed_hash\",false]" | \
    jq -ec --argjson port "$port" \
      '{port:$port,identity:(.result|{number,hash,parentHash,stateRoot,
        receiptsRoot,transactionsRoot,miner,txCount:(.transactions|length)})}' \
      >>"$committed_rows"
done
jq -e -s --arg hash "$committed_hash" '
  length==6 and ([.[].identity]|unique|length)==1 and
  .[0].identity.hash==$hash and .[0].identity.txCount==0
' "$committed_rows" >/dev/null

temporary="$(mktemp "$(dirname "$output")/.network-matrix.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg label "$audit_label" \
  --argjson pid "$rust_pid" \
  --argjson remote_ports "$remote_ports" --arg quorum_line "$quorum_line" \
  --argjson connected "$connected" --argjson needed "$needed" \
  --arg direct_line "$direct_line" --argjson direct "$direct" \
  --arg commit_line "$commit_line" --argjson commit_view "$commit_view" \
  --arg commit_hash "$commit_hash" --argjson consensus "$consensus" \
  --argjson equivocations "$equivocations" --slurpfile rpc "$rpc_rows" \
  --slurpfile authenticated "$authenticated_rows" \
  --slurpfile committed "$committed_rows" '
  {at:$at,event:"gov5_mixed_network_consensus_matrix",
   status:"PASS",label:(if $label=="" then null else $label end),
   mutationPerformed:false,rpcEndpoints:$rpc,
   executionPeerCountSemantics:{govDevp2pPeersEach:5,rustExecutionPeers:0,
     rustConsensusPeersCountedSeparately:true},
   rustConsensusSockets:{pid:$pid,establishedGovTcpRemotePorts:$remote_ports,
     allFiveEstablished:true},
   authenticatedValidatorPeers:$authenticated,
   authenticatedValidatorPeerCount:($authenticated|length),
   quorumEvidence:{logLine:$quorum_line,connectedValidatorPeers:$connected,
     neededQuorumPeers:$needed},
   directPushEvidence:{logLine:$direct_line,directValidatorPeers:$direct},
   latestRustFivePlusFiveCommit:{logLine:$commit_line,view:$commit_view,
     blockHash:$commit_hash},consensusStatus:$consensus,
   equivocations:$equivocations,committedBlockSixEndpointIdentity:$committed,
   allSixCommittedBlockIdentityExact:true,allEndpointsNotSyncing:true,
   allChainIdsExact:true,govClientVersionsExact:true,rustClientVersionExact:true,
   consensusNetworkConnectedAndQuorate:true}' >"$temporary"
jq -e '
  .status=="PASS" and .rustConsensusSockets.allFiveEstablished and
  .authenticatedValidatorPeerCount==5 and
  .quorumEvidence.connectedValidatorPeers==5 and
  .quorumEvidence.neededQuorumPeers==4 and
  .directPushEvidence.directValidatorPeers==5 and
  .consensusStatus.validatorCount==7 and .consensusStatus.hasCommittedQc and
  .equivocations.total==0 and .allSixCommittedBlockIdentityExact and
  .allEndpointsNotSyncing and .consensusNetworkConnectedAndQuorate
' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
