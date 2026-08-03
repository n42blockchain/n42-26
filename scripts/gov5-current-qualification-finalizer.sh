#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-27-gov5-d122-latest-reth}"
finalizer_script="${BASH_SOURCE[0]}"
expected_finalizer_sha="${N42_QUAL_EXPECTED_FINALIZER_SHA:?expected finalizer SHA-256 required}"
harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
qmdb_proof_verifier="$runtime/artifacts/binaries/n42-qmdb-proof-verify"
gov_version="${N42_QUAL_GOV_VERSION:-906}"
formal="$runtime/evidence/mixed-soak-24h.jsonl"
burst_artifact="$runtime/artifacts/p4-signed-transaction-burst.json"
burst_evidence="$runtime/evidence/p4-transaction-burst-$gov_version.jsonl"
post_burst="$runtime/evidence/mixed-post-burst-10m.jsonl"
post_restart="$runtime/evidence/mixed-post-restart-10m.jsonl"
archive_post_burst="$runtime/evidence/archive-rpc-parity-$gov_version-post-burst.jsonl"
restart_evidence="$runtime/evidence/rust-restart-rejoin-$gov_version.jsonl"
leader_final="$runtime/evidence/rust-leader-final-audit.jsonl"
timeout_final="$runtime/evidence/timeout-recovery-final-audit.jsonl"
runtime_log_final="$runtime/evidence/runtime-log-final-audit.jsonl"
final_log_root="$runtime/evidence/final-log-snapshot"
final_rust_log="$final_log_root/logs/rust.log"
resource_evidence="$runtime/evidence/rust-resource-24h.jsonl"
resource_audit="$runtime/evidence/rust-resource-24h-audit.json"
upstream="$runtime/evidence/gov5-upstream-24h.jsonl"
upstream_complete="$runtime/evidence/gov5-upstream-24h-complete.json"
upstream_audit="$runtime/evidence/gov5-upstream-24h-audit.json"
soak_audit="$runtime/evidence/mixed-soak-24h-audit.json"
post_burst_audit="$runtime/evidence/mixed-post-burst-10m-audit.json"
post_restart_audit="$runtime/evidence/mixed-post-restart-10m-audit.json"
summary="$runtime/evidence/gov5-$gov_version-final-qualification.json"
failures="$runtime/evidence/gov5-$gov_version-finalizer-failures.jsonl"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
rust_port="${N42_QUAL_RUST_PORT:-29545}"
rust_miner="${N42_QUAL_RUST_MINER:-0x81d4c1f92ddb837cb46f82280d9b491b101fa582}"
configured_rust_leader_start="${N42_QUAL_RUST_LEADER_START:-}"
rust_leader_start=""
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_genesis_artifact_sha="561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687"
expected_consensus_config_sha="38cd3fb1f57e5e3053e23de836b7c98e542ccb5375d0521a65b5c2f6175bd8bf"
expected_bootstrap_bundle_sha="35dda59684e7f56978e5d8de385fa2d2bf15b47747388b88a7449ac31387bf15"
expected_harness_sha="037cc547eb958f0b993565b81aefe30b239e0ad061c27895e3287c6d23e95309"
expected_qmdb_verifier_sha="b329baa1e51435082b2bb2cf538a8d1a1ffd994b5c4ac73474e688ffbfc35c19"
expected_gov_sha="${N42_QUAL_EXPECTED_GOV_SHA:-72e918d9500169e227ef1a0c9d5dd751dcd7d58f1df0871825b61f196e3fce95}"
expected_rust_sha="${N42_QUAL_EXPECTED_RUST_SHA:-0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f}"
frozen_validator_key_dir="$runtime/artifacts/validator-keys/node0"
expected_validator_key_sha="babd0b3550da7702230d3da9a3f00bfce741ed9f1fb8210b702c6023080ea509"
expected_p2p_key_sha="d82561e312fbb044f56eec5f434f03ea1e852924f055a8949ea82be9e7bbe277"
expected_gov_upstream_sha="${N42_QUAL_EXPECTED_GOV_UPSTREAM_SHA:-d12257c92e9b1e83d35c981441593663db6db72b}"
expected_gov_candidate_sha="${N42_QUAL_EXPECTED_GOV_CANDIDATE_SHA:-d0999e7680bfbba71c252de1dd95efe64736e5f9}"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"

mkdir -p "$runtime/evidence"

on_error() {
  local status=$?
  local line="${BASH_LINENO[0]:-0}"
  trap - ERR
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg gov_version "$gov_version" \
    --argjson status "$status" \
    --argjson line "$line" \
    --arg command "${BASH_COMMAND:-unknown}" \
    '{at:$at,event:("gov5_"+$gov_version+"_finalizer_failure"),statusCode:$status,
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

wait_for_timeout_recovery_close() {
  local _ recent timeout_view recovery_view committed_view recovery_line
  for _ in $(seq 1 1200); do
    recent="$(tail -n 2000 "$runtime/logs/rust.log")"
    timeout_view="$(sed -E -n \
      's/.*view timed out view=([0-9]+)$/\1/p' <<<"$recent" | tail -n 1)"
    if [[ "$timeout_view" =~ ^[0-9]+$ ]]; then
      recovery_view=$((timeout_view + 1))
      committed_view="$(rpc "$rust_port" n42_consensusStatus '[]' |
        jq -er '.result.latestCommittedView')"
      recovery_line="$(rg -F "block committed! view=$recovery_view " \
        <<<"$recent" | tail -n 1 || true)"
      if test "$committed_view" -ge "$recovery_view" &&
        [[ "$recovery_line" == *"votes=5+5" ]]; then
        return 0
      fi
    fi
    sleep 0.25
  done
  return 1
}

resolve_rust_leader_start() {
  local line="" hash block number_hex derived attempt
  # The strict-launch Rust log may be rotated independently of older runtime
  # history. Bind the canonical leader audit to the first commit that is
  # actually present in the immutable log instead of a stale historical
  # height that the log cannot prove.
  for attempt in $(seq 1 1200); do
    line="$(rg -m1 ' INFO (.*: )?block committed! view=' \
      "$runtime/logs/rust.log" || true)"
    if test -n "$line"; then
      break
    fi
    sleep 0.25
  done
  if test -z "$line"; then
    echo "strict Rust log contains no committed leader block" >&2
    return 1
  fi
  hash="$(sed -E -n \
    's/.*block_hash=(0x[0-9a-f]{64}).*/\1/p' <<<"$line")"
  if ! [[ "$hash" =~ ^0x[0-9a-f]{64}$ ]]; then
    echo "cannot parse first committed block hash from strict Rust log" >&2
    return 1
  fi
  block="$(rpc "$rust_port" eth_getBlockByHash \
    "$(jq -nc --arg hash "$hash" '[$hash,false]')" | jq -ec '.result')"
  jq -e --arg miner "$rust_miner" '
    .hash != null and (.hash | test("^0x[0-9a-f]{64}$")) and
    (.miner | ascii_downcase) == $miner and
    (.number | test("^0x[0-9a-f]+$"))
  ' <<<"$block" >/dev/null || {
    echo "first logged Rust commit is not a canonical Rust-authored block" >&2
    return 1
  }
  number_hex="$(jq -er '.number' <<<"$block")"
  derived=$((number_hex))
  if test "$derived" -lt 1; then
    echo "invalid first strict Rust leader height: $derived" >&2
    return 1
  fi
  if test -n "$configured_rust_leader_start"; then
    if ! [[ "$configured_rust_leader_start" =~ ^[0-9]+$ ]] ||
      test "$configured_rust_leader_start" -ne "$derived"; then
      echo "configured Rust leader start does not match strict log: " \
        "configured=$configured_rust_leader_start derived=$derived" >&2
      return 1
    fi
  fi
  printf '%s\n' "$derived"
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

assert_gov_upstream() {
  local remote
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_upstream_sha"
}

assert_gov_source() {
  local branch remote_candidate
  test "$(git -C "$gov_repo" rev-parse HEAD)" = "$expected_gov_candidate_sha"
  test -z "$(git -C "$gov_repo" status --porcelain)"
  branch="$(git -C "$gov_repo" rev-parse --abbrev-ref HEAD)"
  remote_candidate="$(git -C "$gov_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR == 1 {print $1}')"
  test "$remote_candidate" = "$expected_gov_candidate_sha"
}

audit_gov_upstream() {
  jq -e -s --arg expected "$expected_gov_upstream_sha" '
    length >= 2 and
    all(.[];
      .event == "gov5_upstream_snapshot" and
      .baseline == $expected and .remoteMain == $expected and
      .remoteReachable == true and .baselineExact == true) and
    ([.[].at | fromdateiso8601] as $times |
      ($times[-1] - $times[0]) >= 86400 and
      ([range(1; $times | length) as $i |
        ($times[$i] - $times[$i - 1]) > 0 and
        ($times[$i] - $times[$i - 1]) <= 700] | all))
  ' "$upstream" >/dev/null
  assert_gov_upstream

  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg evidence "$upstream" \
    --arg evidence_sha256 "$(shasum -a 256 "$upstream" | awk '{print $1}')" \
    --arg expected "$expected_gov_upstream_sha" \
    --slurpfile samples "$upstream" '
    [$samples[].at | fromdateiso8601] as $times |
    {at:$at,event:"gov5_upstream_audit",status:"PASS",
      evidence:$evidence,evidenceSha256:$evidence_sha256,
      expectedMain:$expected,samples:($samples|length),
      firstAt:$samples[0].at,lastAt:$samples[-1].at,
      elapsedSeconds:($times[-1]-$times[0]),
      maximumSampleGapSeconds:([range(1;$times|length) as $i |
        $times[$i]-$times[$i-1]]|max),
      allSnapshotsReachableAndExact:true,currentRemoteMainExact:true}'
}

assert_genesis() {
  local port hash
  for port in $ports; do
    hash="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')"
    test "$hash" = "$expected_genesis"
  done
}

assert_runtime_identity() {
  local validator_key="$frozen_validator_key_dir/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key"
  local p2p_key="$frozen_validator_key_dir/network-keys"
  local genesis_artifact="$runtime/artifacts/genesis.json"
  local consensus_config="$runtime/artifacts/consensus-peer-bound.json"
  local bootstrap_bundle="$runtime/artifacts/bootstrap-bundle.json"
  require_file "$runtime/geth-live"
  require_file "$runtime/n42-node"
  require_file "$finalizer_script"
  require_file "$harness"
  require_file "$qmdb_proof_verifier"
  require_file "$genesis_artifact"
  require_file "$consensus_config"
  require_file "$bootstrap_bundle"
  require_file "$validator_key"
  require_file "$p2p_key"
  test "$(shasum -a 256 "$runtime/geth-live" | awk '{print $1}')" = "$expected_gov_sha"
  test "$(shasum -a 256 "$runtime/n42-node" | awk '{print $1}')" = "$expected_rust_sha"
  test "$(shasum -a 256 "$finalizer_script" | awk '{print $1}')" = \
    "$expected_finalizer_sha"
  test "$(shasum -a 256 "$harness" | awk '{print $1}')" = \
    "$expected_harness_sha"
  test "$(shasum -a 256 "$qmdb_proof_verifier" | awk '{print $1}')" = \
    "$expected_qmdb_verifier_sha"
  test "$(shasum -a 256 "$genesis_artifact" | awk '{print $1}')" = \
    "$expected_genesis_artifact_sha"
  test "$(shasum -a 256 "$consensus_config" | awk '{print $1}')" = \
    "$expected_consensus_config_sha"
  test "$(shasum -a 256 "$bootstrap_bundle" | awk '{print $1}')" = \
    "$expected_bootstrap_bundle_sha"
  test "$(shasum -a 256 "$validator_key" | awk '{print $1}')" = \
    "$expected_validator_key_sha"
  test "$(shasum -a 256 "$p2p_key" | awk '{print $1}')" = \
    "$expected_p2p_key_sha"
}

preflight_burst() {
  local label="${1:?preflight label required}"
  local preflight="$runtime/evidence/p4-transaction-burst-$gov_version-finalizer-$label.jsonl"
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
require_file "$upstream"
require_file "$resource_evidence"
assert_runtime_identity
assert_gov_source
assert_genesis
assert_live_identity
assert_gov_upstream
rust_leader_start="$(resolve_rust_leader_start)"

if test "${N42_QUAL_FINALIZER_PREFLIGHT_ONLY:-0}" = 1; then
  preflight_burst launch-preflight
  jq -nc \
    --arg at "$(date -u +%FT%TZ)" \
    --arg gov_version "$gov_version" \
    --arg runtime "$runtime" \
    '{at:$at,event:("gov5_"+$gov_version+"_finalizer_preflight"),status:"PASS",
      runtime:$runtime,transactionsSent:0}'
  exit 0
fi

test ! -e "$burst_evidence"
test ! -e "$post_burst"
test ! -e "$post_restart"
test ! -e "$archive_post_burst"
test ! -e "$restart_evidence"
test ! -e "$leader_final"
test ! -e "$timeout_final"
test ! -e "$runtime_log_final"
test ! -e "$final_log_root"
test ! -e "$resource_audit"
test ! -e "$upstream_audit"
test ! -e "$soak_audit"
test ! -e "$post_burst_audit"
test ! -e "$post_restart_audit"
test ! -e "$summary"

# The monitor intentionally runs four minutes beyond the 24-hour acceptance
# threshold. Do not release the transaction burst merely because an in-flight
# snapshot has reached 86,400 seconds: the still-running zero-transaction
# monitor would observe that burst, fail, and append a disqualifying row after
# the audit. Wait for the complete 86,640-second stream to close first, then
# audit the immutable file before any transaction is sent.
formal_monitor_pattern="monitor-heads 86640 30 $formal"
while pgrep -f "$formal_monitor_pattern" >/dev/null; do
  kill -0 "$(<"$runtime/pids/rust.pid")"
  for port in $ports; do
    wait_for_rpc "$port" 1
  done
  sleep 60
done
env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$formal" 86400 120 6 1 >"$soak_audit.pending"
mv "$soak_audit.pending" "$soak_audit"
env N42_QUAL_RUNTIME="$runtime" "$harness" \
  audit-soak "$formal" 86400 120 6 1 >/dev/null
assert_live_identity

# The tested Gov5 baseline must also remain the latest upstream main for a
# complete 24-hour observation window. This prevents a stale Gov build from
# receiving a final PASS if upstream moves while the mixed-client soak runs.
upstream_monitor_pattern="gov5-current-upstream-monitor.sh 87000 600 $upstream"
while ! test -f "$upstream_complete"; do
  pgrep -f "$upstream_monitor_pattern" >/dev/null
  kill -0 "$(<"$runtime/pids/rust.pid")"
  for port in $ports; do
    wait_for_rpc "$port" 1
  done
  sleep 60
done
jq -e --arg expected "$expected_gov_upstream_sha" '
  .event == "gov5_upstream_monitor_complete" and .status == "PASS" and
  .expectedMain == $expected and .elapsedSeconds >= 86400 and .samples >= 2
' "$upstream_complete" >/dev/null
audit_gov_upstream >"$upstream_audit.pending"
mv "$upstream_audit.pending" "$upstream_audit"

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

env N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_QMDB_PROOF_VERIFY="$qmdb_proof_verifier" \
  "$harness" archive-rpc-parity \
  http://127.0.0.1:28501 "http://127.0.0.1:$rust_port" "$archive_post_burst"
jq -e -s '
  (map(select(.event == "archive_qmdb_reference_parity" and
    .govRustProofRootsExact == true and .govRustProofBytesExact == true and
    .govRustProofsOfflineVerified == true)) | length) == 1 and
  (map(select(.event == "archive_rpc_parity" and .govRustRpcExact == true and
    .qmdbProofRootExact == true and .qmdbProofOfflineVerified == true)) | length) == 11
' "$archive_post_burst" >/dev/null

# Begin the restart only after the latest missing-validator timeout has its
# canonical Rust 5+5 recovery commit. Requiring that Rust block to remain the
# RPC latest head is racy because all five Gov blocks can follow in less than
# the polling interval.
assert_runtime_identity
assert_genesis
wait_for_timeout_recovery_close
assert_live_identity
pre_restart_pid="$(<"$runtime/pids/rust.pid")"
pre_restart_head_hex="$(rpc "$rust_port" eth_blockNumber '[]' | jq -er '.result')"
pre_restart_head=$((pre_restart_head_hex))
pre_restart_status="$(rpc "$rust_port" n42_consensusStatus '[]' | jq -ec '.result')"
pre_restart_equivocations="$(rpc "$rust_port" n42_equivocations '[]' | jq -ec '.result')"
jq -e '.hasCommittedQc == true and .validatorCount == 7' \
  <<<"$pre_restart_status" >/dev/null
jq -e '.total == 0 and (.evidence | length) == 0' \
  <<<"$pre_restart_equivocations" >/dev/null
restart_started_seconds="$(date +%s)"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --argjson pid "$pre_restart_pid" \
  --argjson head "$pre_restart_head" \
  --argjson consensus "$pre_restart_status" \
  --argjson equivocations "$pre_restart_equivocations" \
  '{at:$at,event:"rust_restart_started",pidBefore:$pid,headBefore:$head,
    consensusBefore:$consensus,equivocationsBefore:$equivocations}' \
  >>"$restart_evidence"

env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_NODE_BINARY="$runtime/n42-node" \
  N42_CONSENSUS_CONFIG_FILE="$runtime/artifacts/consensus-peer-bound.json" \
  N42_VALIDATOR_KEY_DIR="$frozen_validator_key_dir" \
  N42_EXPECTED_VALIDATOR_KEY_SHA256="$expected_validator_key_sha" \
  N42_EXPECTED_P2P_KEY_SHA256="$expected_p2p_key_sha" \
  N42_GOV5_CATCHUP_BUFFER_BLOCKS=131072 \
  N42_QMDB_REPLAY_DEPTH=1048576 \
  "$harness" restart-rust
wait_for_rpc "$rust_port" 300
rpc_recovery_seconds=$(( $(date +%s) - restart_started_seconds ))
post_restart_pid="$(<"$runtime/pids/rust.pid")"
test "$post_restart_pid" != "$pre_restart_pid"
assert_live_identity 300
rejoin_wait_seconds=$(( $(date +%s) - restart_started_seconds ))

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
jq -e '.hasCommittedQc == true and .validatorCount == 7' \
  <<<"$post_restart_status" >/dev/null
jq -e '.total == 0 and (.evidence | length) == 0' <<<"$equivocations" >/dev/null
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --argjson pid "$post_restart_pid" \
  --argjson head "$post_restart_head" \
  --argjson rpc_recovery_seconds "$rpc_recovery_seconds" \
  --argjson rejoin_wait_seconds "$rejoin_wait_seconds" \
  --argjson consensus "$post_restart_status" \
  --argjson equivocations "$equivocations" \
  '{at:$at,event:"rust_restart_rejoined",pidAfter:$pid,headAfter:$head,
    exactIdentityBeforeStabilityWindow:true,rejoinWaitSeconds:$rejoin_wait_seconds,
    rpcRecoverySeconds:$rpc_recovery_seconds,
    consensusAfter:$consensus,equivocations:$equivocations}' >>"$restart_evidence"

# Close the final evidence only after the latest timeout's Rust 5+5 recovery,
# so the summary contains no timeout waiting for its successor view.
wait_for_timeout_recovery_close

# Freeze one log tree and use that exact immutable Rust log for all three
# final log-dependent audits. The live log continues growing while resource
# evidence closes, so auditing it directly would leave stale embedded hashes.
mkdir -p "$final_log_root/logs"
cp "$runtime"/logs/gov{1,2,3,4,5}.log "$runtime/logs/rust.log" \
  "$final_log_root/logs/"
final_rust_log_sha="$(shasum -a 256 "$final_rust_log" | awk '{print $1}')"

env \
  N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT="$rust_port" \
  N42_QUAL_RUST_MINER="$rust_miner" \
  N42_QUAL_RUST_LOG="$final_rust_log" \
  "$harness" audit-rust-leaders "$rust_leader_start" "$post_restart_head" \
    "$leader_final" >/dev/null
env N42_QUAL_RUNTIME="$final_log_root" N42_QUAL_RUST_PORT="$rust_port" \
  "$harness" audit-timeout-recovery "$final_rust_log" \
    "$timeout_final" >/dev/null
jq -e -s '
  length == 1 and .[0].status == "PASS" and
  .[0].completedTimeouts >= 1 and .[0].pendingTimeouts == 0 and
  .[0].timeoutViewStride == 7 and
  .[0].timeoutAndPacemakerSetsExact == true and
  .[0].everyCompletedTimeoutRecoveredAtNextView == true and
  .[0].recoveredByRustVotesFivePlusFive == true
' "$timeout_final" >/dev/null
env N42_QUAL_RUNTIME="$final_log_root" \
  "$harness" audit-runtime-logs "$final_rust_log" \
    "$runtime_log_final" >/dev/null
jq -e -s '
  length == 1 and .[0].status == "PASS" and
  .[0].warningPartitionExact == true and
  .[0].timeoutSetsCountExact == true and
  .[0].compactEvictionsMatchRustLeaderCommits == true and
  .[0].unexpectedWarnings == 0 and .[0].criticalSignals == 0
' "$runtime_log_final" >/dev/null
test "$(jq -er '.[0].logSha256' "$timeout_final")" = "$final_rust_log_sha"
test "$(jq -er '.[0].rustLogSha256' "$runtime_log_final")" = \
  "$final_rust_log_sha"
resource_monitor_pattern="monitor-rust-resources 87000 300 $resource_evidence"
while pgrep -f "$resource_monitor_pattern" >/dev/null; do
  kill -0 "$(<"$runtime/pids/rust.pid")"
  for port in $ports; do
    wait_for_rpc "$port" 1
  done
  sleep 60
done
env N42_QUAL_RUNTIME="$runtime" \
  "$harness" audit-rust-resources "$resource_evidence" 86400 \
    "$resource_audit.pending" >/dev/null
mv "$resource_audit.pending" "$resource_audit"
assert_genesis
assert_live_identity
assert_gov_upstream
assert_gov_source

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg gov_version "$gov_version" \
  --arg runtime "$runtime" \
  --arg finalizer_sha "$expected_finalizer_sha" \
  --arg genesis_artifact_sha "$expected_genesis_artifact_sha" \
  --arg consensus_config_sha "$expected_consensus_config_sha" \
  --arg bootstrap_bundle_sha "$expected_bootstrap_bundle_sha" \
  --arg harness_sha "$expected_harness_sha" \
  --arg qmdb_verifier_sha "$expected_qmdb_verifier_sha" \
  --arg validator_key_sha "$expected_validator_key_sha" \
  --arg p2p_key_sha "$expected_p2p_key_sha" \
  --arg formal "$formal" \
  --arg formal_sha "$(shasum -a 256 "$formal" | awk '{print $1}')" \
  --arg burst_sha "$(shasum -a 256 "$burst_evidence" | awk '{print $1}')" \
  --arg post_burst "$post_burst" \
  --arg post_burst_sha "$(shasum -a 256 "$post_burst" | awk '{print $1}')" \
  --arg archive_post_burst "$archive_post_burst" \
  --arg archive_post_burst_sha "$(shasum -a 256 "$archive_post_burst" | awk '{print $1}')" \
  --arg restart_sha "$(shasum -a 256 "$restart_evidence" | awk '{print $1}')" \
  --arg post_restart "$post_restart" \
  --arg post_restart_sha "$(shasum -a 256 "$post_restart" | awk '{print $1}')" \
  --arg leader_sha "$(shasum -a 256 "$leader_final" | awk '{print $1}')" \
  --arg timeout_sha "$(shasum -a 256 "$timeout_final" | awk '{print $1}')" \
  --arg runtime_log_sha "$(shasum -a 256 "$runtime_log_final" | awk '{print $1}')" \
  --arg final_rust_log "$final_rust_log" \
  --arg final_rust_log_sha "$final_rust_log_sha" \
  --arg resource_sha "$(shasum -a 256 "$resource_evidence" | awk '{print $1}')" \
  --arg upstream_sha "$(shasum -a 256 "$upstream" | awk '{print $1}')" \
  --slurpfile soak "$soak_audit" \
  --slurpfile upstream "$upstream_audit" \
  --slurpfile burst "$burst_evidence" \
  --slurpfile post_burst_audit "$post_burst_audit" \
  --slurpfile post_restart_audit "$post_restart_audit" \
  --slurpfile restart "$restart_evidence" \
  --slurpfile leaders "$leader_final" \
  --slurpfile timeouts "$timeout_final" \
  --slurpfile runtime_logs "$runtime_log_final" \
  --slurpfile resources "$resource_audit" '
  {at:$at,event:("gov5_"+$gov_version+"_final_qualification"),status:"PASS",runtime:$runtime,
   acceptanceRelaxed:false,genesisExact:true,binariesExact:true,
   finalizerScriptSha256:$finalizer_sha,
   genesisArtifactSha256:$genesis_artifact_sha,
   consensusConfigSha256:$consensus_config_sha,
   bootstrapBundleSha256:$bootstrap_bundle_sha,
   qualificationHarnessSha256:$harness_sha,
   qmdbProofVerifierSha256:$qmdb_verifier_sha,
   finalToolingFrozenAndExact:true,
   restartConfigurationFrozenAndExact:true,
   validatorKeySha256:$validator_key_sha,p2pKeySha256:$p2p_key_sha,
   keyMaterialFrozenAndExact:true,
   formalEvidence:$formal,formalEvidenceSha256:$formal_sha,
   soakAudit:$soak[0],
   gov5UpstreamAudit:$upstream[0],gov5UpstreamEvidenceSha256:$upstream_sha,
   transactionBurst:($burst|map(select(.event=="p4_transaction_burst_pass"))[0]),
   transactionBurstEvidenceSha256:$burst_sha,
   postBurstEvidence:$post_burst,postBurstEvidenceSha256:$post_burst_sha,
   postBurstAudit:$post_burst_audit[0],
   archiveParityPostBurstEvidence:$archive_post_burst,
   archiveParityPostBurstEvidenceSha256:$archive_post_burst_sha,
   restart:$restart,restartEvidenceSha256:$restart_sha,
   postRestartEvidence:$post_restart,postRestartEvidenceSha256:$post_restart_sha,
   postRestartAudit:$post_restart_audit[0],
   rustLeaderAudit:$leaders[-1],rustLeaderEvidenceSha256:$leader_sha,
   timeoutRecoveryAudit:$timeouts[-1],timeoutRecoveryEvidenceSha256:$timeout_sha,
   runtimeLogAudit:$runtime_logs[-1],runtimeLogEvidenceSha256:$runtime_log_sha,
   immutableFinalLog:{path:$final_rust_log,sha256:$final_rust_log_sha},
   rustResourceAudit:$resources[-1],rustResourceEvidenceSha256:$resource_sha,
   postBurstExact:true,postRestartExact:true,archiveParityPostBurst:true,
   zeroEquivocations:true}' >"$summary"

test ! -s "$failures"
cat "$summary"
