#!/usr/bin/env bash
set -Eeuo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-18-gov5-906-latest-reth}"
qualification_dir="${N42_LATEST_RETH_QUAL_DIR:-/Users/jieliu/Documents/n42/live-interop-20260721/post-qualification-latest-reth-20260802}"
script_path="${BASH_SOURCE[0]}"
expected_self_sha="${N42_LATEST_RETH_EXPECTED_SELF_SHA256:-}"
latest_binary="${N42_LATEST_RETH_BINARY:-$qualification_dir/n42-node}"
expected_binary_sha="${N42_LATEST_RETH_BINARY_SHA256:-0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f}"
expected_reth_commit="${N42_LATEST_RETH_COMMIT:-91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b}"
expected_source_commit="${N42_LATEST_RETH_SOURCE_COMMIT:-}"
source_repo="${N42_LATEST_RETH_SOURCE_REPO:-/Users/jieliu/Documents/n42/interop-reth-latest-20260802/n42-26}"
gov_repo="${N42_LATEST_RETH_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260801}"
expected_gov_commit="${N42_LATEST_RETH_GOV_COMMIT:-8915b4cc07d82dc195daee2e8e741ea5e8446068}"
expected_gov_upstream="${N42_LATEST_RETH_GOV_UPSTREAM:-920f7536eb263b6744b48f28dfeb77f4c2798c1a}"
harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
independent="$runtime/evidence/gov5-906-independent-final-verification.json"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
rust_port="${N42_QUAL_RUST_PORT:-29545}"
rust_miner="${N42_QUAL_RUST_MINER:-0x81d4c1f92ddb837cb46f82280d9b491b101fa582}"
duration="${N42_LATEST_RETH_DURATION_SECONDS:-3600}"
resource_duration="$duration"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_harness_sha="bd5fafe7b47a8613252c977d0060ccd25e2e1ee6fba949c8f28e0b9feda95d5e"
expected_validator_key_sha="babd0b3550da7702230d3da9a3f00bfce741ed9f1fb8210b702c6023080ea509"
expected_p2p_key_sha="d82561e312fbb044f56eec5f434f03ea1e852924f055a8949ea82be9e7bbe277"
expected_consensus_config_sha="38cd3fb1f57e5e3053e23de836b7c98e542ccb5375d0521a65b5c2f6175bd8bf"
expected_bootstrap_sha="35dda59684e7f56978e5d8de385fa2d2bf15b47747388b88a7449ac31387bf15"
expected_genesis_artifact_sha="561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687"
key_dir="$runtime/artifacts/validator-keys/node0"
validator_key="$key_dir/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key"
p2p_key="$key_dir/network-keys"
head_evidence="$qualification_dir/latest-reth-heads-1h.jsonl"
head_audit="$qualification_dir/latest-reth-heads-1h-audit.json"
resource_evidence="$qualification_dir/latest-reth-resources-1h.jsonl"
resource_audit="$qualification_dir/latest-reth-resources-1h-audit.json"
rollover_evidence="$qualification_dir/latest-reth-rollover.jsonl"
leader_evidence="$qualification_dir/latest-reth-leader-audit.jsonl"
latest_log="$qualification_dir/latest-reth-rust.log"
summary="$qualification_dir/latest-reth-final-qualification.json"
failures="$qualification_dir/latest-reth-failures.jsonl"
resource_pid=""

mkdir -p "$qualification_dir"

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  if test -n "$resource_pid" && kill -0 "$resource_pid" 2>/dev/null; then
    kill "$resource_pid" 2>/dev/null || true
    wait "$resource_pid" 2>/dev/null || true
  fi
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson status "$status" \
    --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:"latest_reth_rollover_failure",statusCode:$status,
      line:$line,command:$command}' >>"$failures"
  exit "$status"
}
trap on_error ERR

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
  local attempts="$1"
  local _
  for _ in $(seq 1 "$attempts"); do
    if rpc "$rust_port" eth_blockNumber '[]' | jq -e '.result != null' >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

assert_live_identity() {
  local attempts="${1:-30}"
  local attempt expected port identity exact
  for attempt in $(seq 1 "$attempts"); do
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

assert_source() {
  local branch remote
  if test -n "$expected_source_commit"; then
    test "$(git -C "$source_repo" rev-parse HEAD)" = \
      "$(git -C "$source_repo" rev-parse "$expected_source_commit^{commit}")"
  fi
  test -z "$(git -C "$source_repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$source_repo" rev-parse --abbrev-ref HEAD)"
  remote="$(git -C "$source_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$(git -C "$source_repo" rev-parse HEAD)"
}

assert_gov_source() {
  local branch remote_candidate remote_main
  test "$(git -C "$gov_repo" rev-parse HEAD)" = "$expected_gov_commit"
  test -z "$(git -C "$gov_repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$gov_repo" rev-parse --abbrev-ref HEAD)"
  remote_candidate="$(git -C "$gov_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR == 1 {print $1}')"
  remote_main="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote_candidate" = "$expected_gov_commit"
  test "$remote_main" = "$expected_gov_upstream"
}

assert_static_inputs() {
  local version
  if test -n "$expected_self_sha"; then
    test "$(shasum -a 256 "$script_path" | awk '{print $1}')" = "$expected_self_sha"
  fi
  test -x "$latest_binary"
  test "$(shasum -a 256 "$latest_binary" | awk '{print $1}')" = "$expected_binary_sha"
  version="$("$latest_binary" --version)"
  grep -F 'Reth Version: 2.4.1' <<<"$version" >/dev/null
  grep -F "Commit SHA: $expected_reth_commit" <<<"$version" >/dev/null
  test "$(shasum -a 256 "$harness" | awk '{print $1}')" = "$expected_harness_sha"
  test "$(shasum -a 256 "$validator_key" | awk '{print $1}')" = \
    "$expected_validator_key_sha"
  test "$(shasum -a 256 "$p2p_key" | awk '{print $1}')" = "$expected_p2p_key_sha"
  test "$(shasum -a 256 "$runtime/artifacts/consensus-peer-bound.json" |
    awk '{print $1}')" = "$expected_consensus_config_sha"
  test "$(shasum -a 256 "$runtime/artifacts/bootstrap-bundle.json" |
    awk '{print $1}')" = "$expected_bootstrap_sha"
  test "$(shasum -a 256 "$runtime/artifacts/genesis.json" | awk '{print $1}')" = \
    "$expected_genesis_artifact_sha"
  assert_source
  assert_gov_source
  assert_genesis
  assert_live_identity
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

resolve_first_latest_rust_height() {
  local line hash block number_hex
  while IFS= read -r line; do
    hash="$(sed -E -n 's/.*block_hash=(0x[0-9a-f]{64}).*/\1/p' <<<"$line")"
    if ! [[ "$hash" =~ ^0x[0-9a-f]{64}$ ]]; then
      continue
    fi
    block="$(rpc "$rust_port" eth_getBlockByHash \
      "$(jq -nc --arg hash "$hash" '[$hash,false]')" | jq -ec '.result')"
    if test "$(jq -r '.miner | ascii_downcase' <<<"$block")" = "$rust_miner"; then
      number_hex="$(jq -er '.number' <<<"$block")"
      printf '%s\n' "$((number_hex))"
      return 0
    fi
  done < <(rg ' INFO (.*: )?block committed! view=' "$latest_log")
  echo "latest Reth log contains no canonical Rust-authored block" >&2
  return 1
}

preflight() {
  assert_static_inputs
  kill -0 "$(<"$runtime/pids/rust.pid")"
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg binary "$latest_binary" \
    --arg binary_sha256 "$expected_binary_sha" \
    --arg reth_commit "$expected_reth_commit" \
    --arg source_commit "$(git -C "$source_repo" rev-parse HEAD)" \
    '{at:$at,event:"latest_reth_rollover_preflight",status:"PASS",
      binary:$binary,binarySha256:$binary_sha256,rethVersion:"2.4.1",
      rethCommit:$reth_commit,sourceCommit:$source_commit,
      liveSixEndpointIdentityExact:true,genesisExact:true,mutationPerformed:false}'
}

run_qualification() {
  local old_pid new_pid log_bytes_before started rpc_recovery rejoin_wait
  local pre_head_hex pre_head pre_status pre_equiv resource_status
  local post_head_hex post_head post_status post_equiv first_latest_height
  local process_command version

  assert_static_inputs
  jq -e '.status == "PASS"' "$independent" >/dev/null
  test ! -e "$summary"
  test ! -e "$head_evidence"
  test ! -e "$resource_evidence"
  test ! -e "$rollover_evidence"
  test ! -e "$leader_evidence"
  test ! -e "$latest_log"
  test ! -s "$failures"

  wait_for_rust_authored_head
  assert_live_identity
  old_pid="$(<"$runtime/pids/rust.pid")"
  kill -0 "$old_pid"
  pre_head_hex="$(rpc "$rust_port" eth_blockNumber '[]' | jq -er '.result')"
  pre_head=$((pre_head_hex))
  pre_status="$(rpc "$rust_port" n42_consensusStatus '[]' | jq -ec '.result')"
  pre_equiv="$(rpc "$rust_port" n42_equivocations '[]' | jq -ec '.result')"
  jq -e '.hasCommittedQc == true and .validatorCount == 7' <<<"$pre_status" >/dev/null
  jq -e '.total == 0 and (.evidence | length) == 0' <<<"$pre_equiv" >/dev/null
  log_bytes_before="$(stat -f '%z' "$runtime/logs/rust.log")"
  started="$(date +%s)"
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson pid "$old_pid" \
    --argjson head "$pre_head" \
    --argjson consensus "$pre_status" \
    --argjson equivocations "$pre_equiv" \
    '{at:$at,event:"latest_reth_rollover_started",pidBefore:$pid,
      headBefore:$head,consensusBefore:$consensus,equivocationsBefore:$equivocations}' \
    >>"$rollover_evidence"

  env \
    N42_QUAL_RUNTIME="$runtime" \
    N42_NODE_BINARY="$latest_binary" \
    N42_CONSENSUS_CONFIG_FILE="$runtime/artifacts/consensus-peer-bound.json" \
    N42_VALIDATOR_KEY_DIR="$key_dir" \
    N42_EXPECTED_VALIDATOR_KEY_SHA256="$expected_validator_key_sha" \
    N42_EXPECTED_P2P_KEY_SHA256="$expected_p2p_key_sha" \
    N42_GOV5_CATCHUP_BUFFER_BLOCKS=131072 \
    N42_QMDB_REPLAY_DEPTH=1048576 \
    "$harness" restart-rust
  wait_for_rpc 300
  rpc_recovery=$(( $(date +%s) - started ))
  new_pid="$(<"$runtime/pids/rust.pid")"
  test "$new_pid" != "$old_pid"
  process_command="$(ps -p "$new_pid" -o command=)"
  case "$process_command" in
    "$latest_binary node "*) ;;
    *) echo "latest Reth process command mismatch: $process_command" >&2; return 1 ;;
  esac
  assert_live_identity 300
  rejoin_wait=$(( $(date +%s) - started ))

  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_RUST_PORT="$rust_port" \
    "$harness" monitor-rust-resources "$resource_duration" 300 \
    "$resource_evidence" &
  resource_pid=$!
  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    N42_QUAL_RUST_PORT="$rust_port" N42_QUAL_MAX_LAG=6 \
    "$harness" monitor-heads "$duration" 30 "$head_evidence"
  resource_status=0
  wait "$resource_pid" || resource_status=$?
  test "$resource_status" -eq 0

  env N42_QUAL_RUNTIME="$runtime" "$harness" \
    audit-soak "$head_evidence" "$duration" 120 6 0 >"$head_audit"
  env N42_QUAL_RUNTIME="$runtime" "$harness" \
    audit-rust-resources "$resource_evidence" "$resource_duration" \
    "$resource_audit" >/dev/null

  tail -c "+$((log_bytes_before + 1))" "$runtime/logs/rust.log" >"$latest_log"
  first_latest_height="$(resolve_first_latest_rust_height)"
  post_head_hex="$(rpc "$rust_port" eth_blockNumber '[]' | jq -er '.result')"
  post_head=$((post_head_hex))
  test "$post_head" -gt "$pre_head"
  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    N42_QUAL_RUST_PORT="$rust_port" N42_QUAL_RUST_MINER="$rust_miner" \
    N42_QUAL_RUST_LOG="$latest_log" \
    "$harness" audit-rust-leaders "$first_latest_height" "$post_head" \
    "$leader_evidence" >/dev/null
  post_status="$(rpc "$rust_port" n42_consensusStatus '[]' | jq -ec '.result')"
  post_equiv="$(rpc "$rust_port" n42_equivocations '[]' | jq -ec '.result')"
  jq -e '.hasCommittedQc == true and .validatorCount == 7' <<<"$post_status" >/dev/null
  jq -e '.total == 0 and (.evidence | length) == 0' <<<"$post_equiv" >/dev/null
  assert_genesis
  assert_live_identity
  assert_source
  assert_gov_source
  test "$(<"$runtime/pids/rust.pid")" = "$new_pid"
  kill -0 "$new_pid"
  process_command="$(ps -p "$new_pid" -o command=)"
  case "$process_command" in
    "$latest_binary node "*) ;;
    *) echo "latest Reth final process command mismatch: $process_command" >&2; return 1 ;;
  esac
  test "$(shasum -a 256 "$latest_binary" | awk '{print $1}')" = "$expected_binary_sha"
  version="$("$latest_binary" --version)"

  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --argjson pid "$new_pid" \
    --argjson head "$post_head" \
    --argjson rpc_recovery "$rpc_recovery" \
    --argjson rejoin_wait "$rejoin_wait" \
    --argjson consensus "$post_status" \
    --argjson equivocations "$post_equiv" \
    '{at:$at,event:"latest_reth_rollover_completed",pidAfter:$pid,
      headAfter:$head,rpcRecoverySeconds:$rpc_recovery,
      rejoinWaitSeconds:$rejoin_wait,consensusAfter:$consensus,
      equivocations:$equivocations,sourceAndGovUpstreamStillExact:true}' \
    >>"$rollover_evidence"

  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg binary "$latest_binary" \
    --arg binary_sha256 "$expected_binary_sha" \
    --arg reth_commit "$expected_reth_commit" \
    --arg version "$version" \
    --arg independent "$independent" \
    --arg independent_sha "$(shasum -a 256 "$independent" | awk '{print $1}')" \
    --arg head_sha "$(shasum -a 256 "$head_evidence" | awk '{print $1}')" \
    --arg resource_sha "$(shasum -a 256 "$resource_evidence" | awk '{print $1}')" \
    --arg leader_sha "$(shasum -a 256 "$leader_evidence" | awk '{print $1}')" \
    --arg rollover_sha "$(shasum -a 256 "$rollover_evidence" | awk '{print $1}')" \
    --arg log_sha "$(shasum -a 256 "$latest_log" | awk '{print $1}')" \
    --argjson pid_before "$old_pid" \
    --argjson pid_after "$new_pid" \
    --argjson head_before "$pre_head" \
    --argjson head_after "$post_head" \
    --argjson rpc_recovery "$rpc_recovery" \
    --argjson rejoin_wait "$rejoin_wait" \
    --argjson consensus "$post_status" \
    --argjson equivocations "$post_equiv" \
    --slurpfile head_audit "$head_audit" \
    --slurpfile resource_audit "$resource_audit" \
    --slurpfile leader "$leader_evidence" '
    {at:$at,event:"latest_reth_final_qualification",status:"PASS",
      binary:$binary,binarySha256:$binary_sha256,rethVersion:"2.4.1",
      rethCommit:$reth_commit,versionOutput:$version,
      sourceAndGovUpstreamStillExact:true,
      strictIndependentVerification:$independent,
      strictIndependentVerificationSha256:$independent_sha,
      pidBefore:$pid_before,pidAfter:$pid_after,headBefore:$head_before,
      headAfter:$head_after,headGrowth:($head_after-$head_before),
      rpcRecoverySeconds:$rpc_recovery,rejoinWaitSeconds:$rejoin_wait,
      headEvidenceSha256:$head_sha,headAudit:$head_audit[0],
      resourceEvidenceSha256:$resource_sha,resourceAudit:$resource_audit[0],
      leaderEvidenceSha256:$leader_sha,rustLeaderAudit:$leader[-1],
      rolloverEvidenceSha256:$rollover_sha,latestRustLogSha256:$log_sha,
      consensus:$consensus,equivocations:$equivocations,
      genesisExact:true,allSixEndpointsExact:true,latestBinaryStillRunning:true}' \
    >"$summary"
  jq -e '.status == "PASS" and .headGrowth > 0 and
    .headAudit.status == "PASS" and .resourceAudit.status == "PASS" and
    .rustLeaderAudit.status == "PASS" and .consensus.hasCommittedQc == true and
    .equivocations.total == 0' "$summary" >/dev/null
  cat "$summary"
}

case "${1:-}" in
  preflight) preflight ;;
  run) run_qualification ;;
  *) echo "usage: $0 {preflight|run}" >&2; exit 2 ;;
esac
