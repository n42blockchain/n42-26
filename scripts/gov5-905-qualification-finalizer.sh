#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-16-gov5-905-fresh-reth}"
harness="$repo/scripts/gov5-interop-qualification.sh"
formal="$runtime/evidence/mixed-soak-24h.jsonl"
burst_artifact="$runtime/artifacts/p4-signed-transaction-burst.json"
burst_evidence="$runtime/evidence/p4-transaction-burst-905.jsonl"
post_burst="$runtime/evidence/mixed-post-burst-10m.jsonl"
post_restart="$runtime/evidence/mixed-post-restart-10m.jsonl"
archive_post_burst="$runtime/evidence/archive-rpc-parity-905-post-burst.jsonl"
restart_evidence="$runtime/evidence/rust-restart-rejoin-905.jsonl"
leader_final="$runtime/evidence/rust-leader-final-audit.jsonl"
soak_audit="$runtime/evidence/mixed-soak-24h-audit.json"
post_burst_audit="$runtime/evidence/mixed-post-burst-10m-audit.json"
post_restart_audit="$runtime/evidence/mixed-post-restart-10m-audit.json"
summary="$runtime/evidence/gov5-905-final-qualification.json"
failures="$runtime/evidence/gov5-905-finalizer-failures.jsonl"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
rust_port="${N42_QUAL_RUST_PORT:-29545}"
rust_miner="${N42_QUAL_RUST_MINER:-0x81d4c1f92ddb837cb46f82280d9b491b101fa582}"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_gov_sha="4797696faa42cff77cb4f75fb8db22cb89decb42cd10178748052c25131f77f2"
expected_rust_sha="d917782b906176119172e656005218be34ec3d5ad1b7241c0c53f8f6d593da2d"

mkdir -p "$runtime/evidence"

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson status "$status" \
    --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"gov5_905_finalizer_failure",statusCode:$status,
      line:$line,command:$command}' >>"$failures"
  exit "$status"
}
trap on_error ERR

require_file() {
  test -f "$1" || {
    echo "missing required file: $1" >&2
    return 1
  }
}

rpc() {
  local port="$1"
  local method="$2"
  local params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

wait_for_rpc() {
  local port="$1"
  local attempts="$2"
  local _
  for _ in $(seq 1 "$attempts"); do
    if rpc "$port" eth_blockNumber '[]' | jq -e '.result != null' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

wait_for_rust_authored_head() {
  local _ block
  for _ in $(seq 1 1200); do
    block="$(rpc "$rust_port" eth_getBlockByNumber '["latest",false]' | jq -ec '.result')"
    if test "$(jq -r '.miner | ascii_downcase' <<<"$block")" = "$rust_miner"; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

assert_live_identity() {
  local attempt expected port identity exact
  for attempt in $(seq 1 30); do
    expected=""
    exact=true
    for port in $ports; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' |
        jq -er '.result | [.number,.hash,.stateRoot,.receiptsRoot] | join(":")')"
      if test -z "$expected"; then
        expected="$identity"
      elif test "$identity" != "$expected"; then
        exact=false
        break
      fi
    done
    if test "$exact" = true; then
      return 0
    fi
    sleep 1
  done
  return 1
}

assert_genesis() {
  local port hash
  for port in $ports; do
    hash="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')"
    test "$hash" = "$expected_genesis"
  done
}

assert_runtime_identity() {
  require_file "$runtime/geth-live"
  require_file "$runtime/n42-node"
  test "$(shasum -a 256 "$runtime/geth-live" | awk '{print $1}')" = "$expected_gov_sha"
  test "$(shasum -a 256 "$runtime/n42-node" | awk '{print $1}')" = "$expected_rust_sha"
}

preflight_burst() {
  local label="${1:?preflight label required}"
  local preflight="$runtime/evidence/p4-transaction-burst-905-finalizer-$label.jsonl"
  test ! -e "$preflight"
  env \
    N42_QUAL_RUNTIME="$runtime" \
    N42_QUAL_PORTS="$ports" \
    N42_QUAL_GOV_INGRESS_PORT=28501 \
    N42_QUAL_RUST_INGRESS_PORT="$rust_port" \
    N42_QUAL_BURST_PREFLIGHT_ONLY=1 \
    "$harness" transaction-burst "$burst_artifact" "$preflight"
  jq -e -s '
    length == 1 and .[0].event == "p4_transaction_burst_preflight" and
    .[0].firstNonce == 17 and .[0].expectedNonce == "0x11" and
    .[0].allConfiguredEndpointNoncesExact == true and
    .[0].transactionsSent == 0
  ' "$preflight" >/dev/null
}

require_file "$harness"
require_file "$formal"
require_file "$burst_artifact"
assert_runtime_identity
assert_genesis
assert_live_identity

if test "${N42_QUAL_FINALIZER_PREFLIGHT_ONLY:-0}" = 1; then
  preflight_burst launch-preflight
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg runtime "$runtime" \
    '{at:$at,event:"gov5_905_finalizer_preflight",status:"PASS",
      runtime:$runtime,transactionsSent:0}'
  exit 0
fi

test ! -e "$burst_evidence"
test ! -e "$post_burst"
test ! -e "$post_restart"
test ! -e "$archive_post_burst"
test ! -e "$restart_evidence"
test ! -e "$leader_final"
test ! -e "$soak_audit"
test ! -e "$post_burst_audit"
test ! -e "$post_restart_audit"
test ! -e "$summary"

# Wait for the first-to-last formal evidence interval to reach a real 24 hours.
while ! env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$formal" 86400 120 6 1 >"$soak_audit.pending" 2>/dev/null; do
  kill -0 "$(<"$runtime/pids/rust.pid")"
  for port in $ports; do
    wait_for_rpc "$port" 1
  done
  if ! pgrep -f "monitor-heads 86640 30 $formal" >/dev/null; then
    echo "formal monitor exited before its evidence passed" >&2
    exit 1
  fi
  sleep 60
done
mv "$soak_audit.pending" "$soak_audit"
env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$formal" 86400 120 6 1 >/dev/null
assert_live_identity

preflight_burst final-preflight
env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_PORTS="$ports" \
  N42_QUAL_GOV_INGRESS_PORT=28501 \
  N42_QUAL_RUST_INGRESS_PORT="$rust_port" \
  "$harness" transaction-burst "$burst_artifact" "$burst_evidence"
jq -e -s '
  (map(select(.event == "p4_transaction_finalized")) | length) == 17 and
  (map(select(.event == "p4_transaction_burst_pass")) | length) == 1 and
  (map(select(.event == "p4_transaction_burst_pass"))[0] |
    .transactions == 17 and .endpointCount == 6 and
    .allConfiguredEndpointsExact == true and
    .receiptAndLogParity == true and .stateAndStorageParity == true)
' "$burst_evidence" >/dev/null

env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT="$rust_port" N42_QUAL_MAX_LAG=6 \
  "$harness" monitor-heads 600 30 "$post_burst"
env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$post_burst" 600 120 6 0 >"$post_burst_audit"

env N42_QUAL_RUNTIME="$runtime" "$harness" archive-rpc-parity \
  http://127.0.0.1:28501 "http://127.0.0.1:$rust_port" "$archive_post_burst"
jq -e -s '
  (map(select(.event == "archive_qmdb_reference_parity" and
    .govRustProofRootsExact == true and .govRustProofBytesExact == true and
    .govRustProofsOfflineVerified == true)) | length) == 1 and
  (map(select(.event == "archive_rpc_parity" and .govRustRpcExact == true and
    .qmdbProofRootExact == true and .qmdbProofOfflineVerified == true)) | length) == 11
' "$archive_post_burst" >/dev/null

# Begin the restart immediately after a Rust-authored commit, leaving the
# largest possible part of the six-height leader cycle for graceful shutdown
# and persisted-state recovery.
wait_for_rust_authored_head
assert_live_identity
pre_restart_pid="$(<"$runtime/pids/rust.pid")"
pre_restart_head_hex="$(rpc "$rust_port" eth_blockNumber '[]' | jq -er '.result')"
pre_restart_head=$((pre_restart_head_hex))
pre_restart_status="$(rpc "$rust_port" n42_consensusStatus '[]' | jq -ec '.result')"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --argjson pid "$pre_restart_pid" \
  --argjson head "$pre_restart_head" \
  --argjson consensus "$pre_restart_status" \
  '{at:$at,event:"rust_restart_started",pidBefore:$pid,headBefore:$head,
    consensusBefore:$consensus}' >>"$restart_evidence"

env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_NODE_BINARY="$runtime/n42-node" \
  N42_CONSENSUS_CONFIG_FILE="$runtime/artifacts/consensus-peer-bound.json" \
  N42_GOV5_CATCHUP_BUFFER_BLOCKS=131072 \
  N42_QMDB_REPLAY_DEPTH=1048576 \
  "$harness" restart-rust
wait_for_rpc "$rust_port" 300
post_restart_pid="$(<"$runtime/pids/rust.pid")"
test "$post_restart_pid" != "$pre_restart_pid"

env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT="$rust_port" N42_QUAL_MAX_LAG=6 \
  "$harness" monitor-heads 600 30 "$post_restart"
env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$post_restart" 600 120 6 0 >"$post_restart_audit"
post_restart_head_hex="$(rpc "$rust_port" eth_blockNumber '[]' | jq -er '.result')"
post_restart_head=$((post_restart_head_hex))
test "$post_restart_head" -gt "$pre_restart_head"
post_restart_status="$(rpc "$rust_port" n42_consensusStatus '[]' | jq -ec '.result')"
equivocations="$(rpc "$rust_port" n42_equivocations '[]' | jq -ec '.result')"
jq -e 'hasCommittedQc == true and .validatorCount == 7' <<<"$post_restart_status" >/dev/null
jq -e '.total == 0 and (.evidence | length) == 0' <<<"$equivocations" >/dev/null
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --argjson pid "$post_restart_pid" \
  --argjson head "$post_restart_head" \
  --argjson consensus "$post_restart_status" \
  --argjson equivocations "$equivocations" \
  '{at:$at,event:"rust_restart_rejoined",pidAfter:$pid,headAfter:$head,
    consensusAfter:$consensus,equivocations:$equivocations}' >>"$restart_evidence"

env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT="$rust_port" \
  N42_QUAL_RUST_MINER="$rust_miner" \
  N42_QUAL_RUST_LOG="$runtime/logs/rust.log" \
  "$harness" audit-rust-leaders 84757 "$post_restart_head" "$leader_final" >/dev/null
assert_genesis
assert_live_identity

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg runtime "$runtime" \
  --arg formal "$formal" \
  --arg formal_sha "$(shasum -a 256 "$formal" | awk '{print $1}')" \
  --arg burst_sha "$(shasum -a 256 "$burst_evidence" | awk '{print $1}')" \
  --arg restart_sha "$(shasum -a 256 "$restart_evidence" | awk '{print $1}')" \
  --arg leader_sha "$(shasum -a 256 "$leader_final" | awk '{print $1}')" \
  --slurpfile soak "$soak_audit" \
  --slurpfile burst "$burst_evidence" \
  --slurpfile restart "$restart_evidence" \
  --slurpfile leaders "$leader_final" '
  {at:$at,event:"gov5_905_final_qualification",status:"PASS",runtime:$runtime,
   acceptanceRelaxed:false,genesisExact:true,binariesExact:true,
   formalEvidence:$formal,formalEvidenceSha256:$formal_sha,
   soakAudit:$soak[0],
   transactionBurst:($burst|map(select(.event=="p4_transaction_burst_pass"))[0]),
   transactionBurstEvidenceSha256:$burst_sha,
   restart:$restart,restartEvidenceSha256:$restart_sha,
   rustLeaderAudit:$leaders[-1],rustLeaderEvidenceSha256:$leader_sha,
   postBurstExact:true,postRestartExact:true,archiveParityPostBurst:true,
   zeroEquivocations:true}' >"$summary"

test ! -s "$failures"
cat "$summary"
