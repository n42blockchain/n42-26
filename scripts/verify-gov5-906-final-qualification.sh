#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-35-gov5-906-rustsec-final}"
repo="${N42_VERIFY_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/n42-26}"
verifier_script="${BASH_SOURCE[0]}"
expected_verifier_script_sha="${N42_VERIFY_EXPECTED_SELF_SHA:-}"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
deps_repo="${N42_QUAL_DEPS_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/n42-26}"
reth_repo="${N42_QUAL_RETH_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/reth}"
paired_reth_repo="${N42_QUAL_PAIRED_RETH_REPO:-/Users/jieliu/Documents/n42/security-refresh-20260804/reth}"
preflight_only="${N42_VERIFY_PREFLIGHT_ONLY:-0}"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_gov_upstream="${N42_VERIFY_GOV_UPSTREAM:-75dab6e5d1bc6aefa213f4f0f7dcc972dd04f89d}"
expected_gov_candidate="${N42_VERIFY_GOV_CANDIDATE:-23533225648c62440c6112f88dfdb64b5f55f3f1}"
expected_deps_head="${N42_VERIFY_DEPS_HEAD:-ce4e88ccfe7bc845ecd57605d417a7559fbde932}"
expected_reth_head="${N42_VERIFY_RETH_HEAD:-0fc810bae34412838bedfd8dc2f212e14e915e5d}"
expected_gov_binary_sha="${N42_VERIFY_GOV_BINARY_SHA:-e08c1ea7aac198e268d7fb07eacce33347798c90f33532ffc4cd85bfc1af7033}"
expected_rust_binary_sha="${N42_VERIFY_RUST_BINARY_SHA:-d639f712a87c22c2a45de29dbd895897058a8a28e4a2145061bd195d79eb6d2e}"
expected_finalizer_sha="${N42_VERIFY_FINALIZER_SHA:-c48c5f3a94e361ce7cb81b41c586e12907f7a2adff688cb661062d9d21692fa0}"
expected_harness_sha="${N42_VERIFY_HARNESS_SHA:-aa906f42b83048cb4168e1ceb1077d1ca8b27429be5189acd1aaa74f06c551e9}"

require_file() {
  test -f "$1" || {
    echo "missing required file: $1" >&2
    return 1
  }
}

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

git_ls_remote_with_retry() {
  local attempt output=""
  local attempts="${N42_GOV_REMOTE_RETRY_ATTEMPTS:-6}"
  local delay="${N42_GOV_REMOTE_RETRY_DELAY_SECONDS:-10}"
  [[ "$attempts" =~ ^[1-9][0-9]*$ ]]
  [[ "$delay" =~ ^[0-9]+$ ]]
  for ((attempt=1; attempt<=attempts; attempt++)); do
    if output="$(git "$@" 2>/dev/null)" && test -n "$output"; then
      printf '%s\n' "$output"
      return 0
    fi
    test "$attempt" -ge "$attempts" || sleep "$delay"
  done
  return 1
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

assert_sha() {
  local path="$1"
  local expected="$2"
  require_file "$path"
  test "$(sha256 "$path")" = "$expected"
}

assert_summary_sha() {
  local summary="$1"
  local field="$2"
  local path="$3"
  test "$(jq -er --arg field "$field" '.[$field]' "$summary")" = \
    "$(sha256 "$path")"
}

assert_live_chain() {
  local expected="" exact identity port hash attempt
  for port in $ports; do
    hash="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')"
    test "$hash" = "$expected_genesis"
  done
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

assert_live_consensus() {
  local consensus equivocations
  consensus="$(rpc 29545 n42_consensusStatus '[]' | jq -ec '.result')"
  equivocations="$(rpc 29545 n42_equivocations '[]' | jq -ec '.result')"
  jq -e '.hasCommittedQc == true and .validatorCount == 7' \
    <<<"$consensus" >/dev/null
  jq -e '.total == 0 and (.evidence | length) == 0' \
    <<<"$equivocations" >/dev/null
}

assert_gov_qmdb_index_disabled() {
  local file pid command
  for file in "$runtime"/pids/gov{1,2,3,4,5}.pid; do
    test -s "$file"
    pid="$(<"$file")"
    kill -0 "$pid"
    command="$(ps eww -p "$pid" -o command=)"
    [[ "$command" != *"N42_QMDB_TRUNC_INDEX="* ]]
  done
}

assert_sender_nonce() {
  local expected_nonce="$1"
  local port nonce
  for port in $ports; do
    nonce="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' |
      jq -er '.result')"
    test "$nonce" = "$expected_nonce"
  done
}

assert_sources() {
  local branch remote latest_stable
  test -z "$(git -C "$repo" status --porcelain --untracked-files=no)"
  test "$(git -C "$repo" rev-parse HEAD)" = \
    "$(git -C "$repo" rev-parse '@{upstream}')"

  test "$(git -C "$gov_repo" rev-parse HEAD)" = "$expected_gov_candidate"
  test -z "$(git -C "$gov_repo" status --porcelain)"
  branch="$(git -C "$gov_repo" rev-parse --abbrev-ref HEAD)"
  remote="$(git_ls_remote_with_retry -C "$gov_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_candidate"
  remote="$(git_ls_remote_with_retry -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_upstream"

  test "$(git -C "$deps_repo" rev-parse HEAD)" = "$expected_deps_head"
  test -z "$(git -C "$deps_repo" status --porcelain)"
  test "$(git -C "$deps_repo" rev-parse HEAD)" = \
    "$(git -C "$deps_repo" rev-parse '@{upstream}')"

  test "$(git -C "$reth_repo" rev-parse HEAD)" = "$expected_reth_head"
  test -z "$(git -C "$reth_repo" status --porcelain)"
  test "$(git -C "$reth_repo" rev-parse HEAD)" = \
    "$(git -C "$reth_repo" rev-parse '@{upstream}')"
  test "$(git -C "$paired_reth_repo" rev-parse HEAD)" = "$expected_reth_head"
  test -z "$(git -C "$paired_reth_repo" status --porcelain)"
  latest_stable="$(git_ls_remote_with_retry ls-remote --tags https://github.com/paradigmxyz/reth.git \
    'refs/tags/v*' | sed -E 's#.*refs/tags/##; s/\^\{\}//' |
    rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
  test "$latest_stable" = v2.4.1
}

assert_pinned_inputs() {
  if test -n "$expected_verifier_script_sha"; then
    assert_sha "$verifier_script" "$expected_verifier_script_sha"
  fi
  assert_sha "$runtime/geth-live" "$expected_gov_binary_sha"
  assert_sha "$runtime/n42-node" "$expected_rust_binary_sha"
  assert_sha "$runtime/artifacts/genesis.json" \
    561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687
  assert_sha "$runtime/artifacts/consensus-peer-bound.json" \
    38cd3fb1f57e5e3053e23de836b7c98e542ccb5375d0521a65b5c2f6175bd8bf
  assert_sha "$runtime/artifacts/bootstrap-bundle.json" \
    35dda59684e7f56978e5d8de385fa2d2bf15b47747388b88a7449ac31387bf15
  assert_sha "$runtime/artifacts/scripts/gov5-interop-qualification.sh" \
    "$expected_harness_sha"
  assert_sha "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh" \
    "$expected_finalizer_sha"
  assert_sha "$runtime/artifacts/binaries/n42-qmdb-proof-verify" \
    b329baa1e51435082b2bb2cf538a8d1a1ffd994b5c4ac73474e688ffbfc35c19
  assert_sha "$runtime/artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key" \
    babd0b3550da7702230d3da9a3f00bfce741ed9f1fb8210b702c6023080ea509
  assert_sha "$runtime/artifacts/validator-keys/node0/network-keys" \
    d82561e312fbb044f56eec5f434f03ea1e852924f055a8949ea82be9e7bbe277
}

assert_live_chain
assert_live_consensus
assert_gov_qmdb_index_disabled
assert_pinned_inputs
assert_sources

if test "$preflight_only" = 1; then
  assert_sender_nonce 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" \
    --arg verifier_sha "$expected_verifier_script_sha" \
    --arg harness_sha "$expected_harness_sha" \
    '{at:$at,event:"gov5_906_independent_final_verifier_preflight",
      status:"PASS",liveChainExact:true,genesisExact:true,
      consensusReady:true,zeroEquivocations:true,pinnedInputsExact:true,
      sourcesAndRemotesExact:true,qmdbTruncIndexDisabledOnAllGovNodes:true,
      verifierScriptSha256:(if $verifier_sha == "" then null else $verifier_sha end),
      qualificationHarnessSha256:$harness_sha,
      senderNonce:"0x11",transactionsSent:0}'
  exit 0
fi

summary="$runtime/evidence/gov5-906-final-qualification.json"
formal="$runtime/evidence/mixed-soak-24h.jsonl"
soak_audit="$runtime/evidence/mixed-soak-24h-audit.json"
upstream="$runtime/evidence/gov5-upstream-24h.jsonl"
upstream_complete="$runtime/evidence/gov5-upstream-24h-complete.json"
upstream_audit="$runtime/evidence/gov5-upstream-24h-audit.json"
burst="$runtime/evidence/p4-transaction-burst-906.jsonl"
post_burst="$runtime/evidence/mixed-post-burst-10m.jsonl"
post_burst_audit="$runtime/evidence/mixed-post-burst-10m-audit.json"
archive_post_burst="$runtime/evidence/archive-rpc-parity-906-post-burst.jsonl"
restart="$runtime/evidence/rust-restart-rejoin-906.jsonl"
post_restart="$runtime/evidence/mixed-post-restart-10m.jsonl"
post_restart_audit="$runtime/evidence/mixed-post-restart-10m-audit.json"
leaders="$runtime/evidence/rust-leader-final-audit.jsonl"
timeouts="$runtime/evidence/timeout-recovery-final-audit.jsonl"
runtime_logs="$runtime/evidence/runtime-log-final-audit.jsonl"
final_log_root="$runtime/evidence/final-log-snapshot"
final_rust_log="$final_log_root/logs/rust.log"
resources="$runtime/evidence/rust-resource-24h.jsonl"
resource_audit="$runtime/evidence/rust-resource-24h-audit.json"
failures="$runtime/evidence/gov5-906-finalizer-failures.jsonl"

for path in "$summary" "$formal" "$soak_audit" "$upstream" \
  "$upstream_complete" "$upstream_audit" "$burst" "$post_burst" \
  "$post_burst_audit" "$archive_post_burst" "$restart" "$post_restart" \
  "$post_restart_audit" "$leaders" "$timeouts" "$runtime_logs" \
  "$final_rust_log" "$resources" "$resource_audit"; do
  require_file "$path"
done
test ! -s "$failures"
assert_sender_nonce 0x22

jq -e '
  .event == "gov5_906_final_qualification" and .status == "PASS" and
  .acceptanceRelaxed == false and .genesisExact == true and
  .binariesExact == true and .finalToolingFrozenAndExact == true and
  .restartConfigurationFrozenAndExact == true and
  .keyMaterialFrozenAndExact == true and .postBurstExact == true and
  .postRestartExact == true and .archiveParityPostBurst == true and
  .zeroEquivocations == true and
  .soakAudit.status == "PASS" and .soakAudit.elapsedSeconds >= 86400 and
  .soakAudit.maximumLag <= 6 and .soakAudit.zeroTransactionRequired == true and
  .gov5UpstreamAudit.status == "PASS" and
  .gov5UpstreamAudit.elapsedSeconds >= 86400 and
  .transactionBurst.event == "p4_transaction_burst_pass" and
  .transactionBurst.transactions == 17 and
  .transactionBurst.endpointCount == 6 and
  .transactionBurst.allConfiguredEndpointsExact == true and
  .transactionBurst.receiptAndLogParity == true and
  .transactionBurst.stateAndStorageParity == true and
  .postBurstAudit.status == "PASS" and .postBurstAudit.elapsedSeconds >= 600 and
  .postBurstAudit.maximumLag <= 6 and
  .postRestartAudit.status == "PASS" and .postRestartAudit.elapsedSeconds >= 600 and
  .postRestartAudit.maximumLag <= 6 and
  (.restart | length) == 2 and
  .restart[0].event == "rust_restart_started" and
  .restart[0].consensusBefore.hasCommittedQc == true and
  .restart[0].consensusBefore.validatorCount == 7 and
  .restart[0].equivocationsBefore.total == 0 and
  .restart[1].event == "rust_restart_rejoined" and
  .restart[1].pidAfter != .restart[0].pidBefore and
  .restart[1].exactIdentityBeforeStabilityWindow == true and
  .restart[1].consensusAfter.hasCommittedQc == true and
  .restart[1].consensusAfter.validatorCount == 7 and
  .restart[1].equivocations.total == 0 and
  .rustLeaderAudit.status == "PASS" and
  .rustLeaderAudit.expectedLeaderSlotsExact == true and
  .rustLeaderAudit.allConfiguredEndpointsExact == true and
  .rustLeaderAudit.leaderCommitLog.allVotesFivePlusFive == true and
  .timeoutRecoveryAudit.status == "PASS" and
  .timeoutRecoveryAudit.pendingTimeouts == 0 and
  .timeoutRecoveryAudit.everyCompletedTimeoutRecoveredAtNextView == true and
  .runtimeLogAudit.status == "PASS" and
  .runtimeLogAudit.unexpectedWarnings == 0 and
  .runtimeLogAudit.criticalSignals == 0 and
  .immutableFinalLog.path != null and
  (.immutableFinalLog.sha256 | test("^[0-9a-f]{64}$")) and
  .rustResourceAudit.status == "PASS" and
  .rustResourceAudit.elapsedSeconds >= 86400 and
  .rustResourceAudit.singleProcess == true and
  .rustResourceAudit.logicalCountersMonotonic == true and
  .rustResourceAudit.allocatedStorageMeasurementsNonnegative == true and
  .rustResourceAudit.allocatedStorageMayDecreaseDuringCompaction == true and
  .rustResourceAudit.headLogAndWalCountersMonotonic == true and
  (.rustResourceAudit.allocatedStorageStepDecreaseKiB.maximumObserved|type) == "number" and
  (.rustResourceAudit.allocatedStorageStepDecreaseKiB.rethMaximum|type) == "number" and
  (.rustResourceAudit.allocatedStorageStepDecreaseKiB.consensusMaximum|type) == "number"
' "$summary" >/dev/null

jq -e \
  --arg finalizer "$(sha256 "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh")" \
  --arg genesis "$(sha256 "$runtime/artifacts/genesis.json")" \
  --arg consensus "$(sha256 "$runtime/artifacts/consensus-peer-bound.json")" \
  --arg bootstrap "$(sha256 "$runtime/artifacts/bootstrap-bundle.json")" \
  --arg harness "$(sha256 "$runtime/artifacts/scripts/gov5-interop-qualification.sh")" \
  --arg verifier "$(sha256 "$runtime/artifacts/binaries/n42-qmdb-proof-verify")" \
  --arg validator "$(sha256 "$runtime/artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key")" \
  --arg p2p "$(sha256 "$runtime/artifacts/validator-keys/node0/network-keys")" '
  .finalizerScriptSha256 == $finalizer and
  .genesisArtifactSha256 == $genesis and
  .consensusConfigSha256 == $consensus and
  .bootstrapBundleSha256 == $bootstrap and
  .qualificationHarnessSha256 == $harness and
  .qmdbProofVerifierSha256 == $verifier and
  .validatorKeySha256 == $validator and .p2pKeySha256 == $p2p
' "$summary" >/dev/null

assert_summary_sha "$summary" formalEvidenceSha256 "$formal"
assert_summary_sha "$summary" gov5UpstreamEvidenceSha256 "$upstream"
assert_summary_sha "$summary" transactionBurstEvidenceSha256 "$burst"
assert_summary_sha "$summary" postBurstEvidenceSha256 "$post_burst"
assert_summary_sha "$summary" archiveParityPostBurstEvidenceSha256 "$archive_post_burst"
assert_summary_sha "$summary" restartEvidenceSha256 "$restart"
assert_summary_sha "$summary" postRestartEvidenceSha256 "$post_restart"
assert_summary_sha "$summary" rustLeaderEvidenceSha256 "$leaders"
assert_summary_sha "$summary" timeoutRecoveryEvidenceSha256 "$timeouts"
assert_summary_sha "$summary" runtimeLogEvidenceSha256 "$runtime_logs"
assert_summary_sha "$summary" rustResourceEvidenceSha256 "$resources"

test "$(jq -er '.immutableFinalLog.path' "$summary")" = "$final_rust_log"
test "$(jq -er '.immutableFinalLog.sha256' "$summary")" = \
  "$(sha256 "$final_rust_log")"
test "$(jq -er '.timeoutRecoveryAudit.log' "$summary")" = "$final_rust_log"
test "$(jq -er '.timeoutRecoveryAudit.logSha256' "$summary")" = \
  "$(sha256 "$final_rust_log")"
test "$(jq -er '.runtimeLogAudit.rustLog' "$summary")" = "$final_rust_log"
test "$(jq -er '.runtimeLogAudit.rustLogSha256' "$summary")" = \
  "$(sha256 "$final_rust_log")"

jq -e --slurpfile artifact "$soak_audit" '.soakAudit == $artifact[0]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$upstream_audit" '.gov5UpstreamAudit == $artifact[0]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$post_burst_audit" '.postBurstAudit == $artifact[0]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$post_restart_audit" '.postRestartAudit == $artifact[0]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$restart" '.restart == $artifact' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$burst" '
  .transactionBurst ==
    ($artifact | map(select(.event == "p4_transaction_burst_pass"))[0])
' "$summary" >/dev/null
jq -e --slurpfile artifact "$leaders" '.rustLeaderAudit == $artifact[-1]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$timeouts" '.timeoutRecoveryAudit == $artifact[-1]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$runtime_logs" '.runtimeLogAudit == $artifact[-1]' \
  "$summary" >/dev/null
jq -e --slurpfile artifact "$resource_audit" '.rustResourceAudit == $artifact[-1]' \
  "$summary" >/dev/null

jq -e -s '
  (map(select(.event == "p4_transaction_finalized")) | length) == 17 and
  (map(select(.event == "p4_transaction_burst_pass")) | length) == 1
' "$burst" >/dev/null
jq -e -s '
  (map(select(.event == "archive_qmdb_reference_parity" and
    .govRustProofRootsExact == true and .govRustProofBytesExact == true and
    .govRustProofsOfflineVerified == true)) | length) == 1 and
  (map(select(.event == "archive_rpc_parity" and .govRustRpcExact == true and
    .qmdbProofRootExact == true and .qmdbProofOfflineVerified == true)) | length) == 11
' "$archive_post_burst" >/dev/null
jq -e '.event == "gov5_upstream_monitor_complete" and .status == "PASS" and
  .expectedMain == $expected and .elapsedSeconds >= 86400' \
  --arg expected "$expected_gov_upstream" "$upstream_complete" >/dev/null

# Re-run the acceptance logic over raw evidence and live archive RPC instead
# of accepting only the finalizer-produced audit objects.
frozen_harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
frozen_qmdb_verifier="$runtime/artifacts/binaries/n42-qmdb-proof-verify"
env N42_QUAL_RUNTIME="$runtime" "$frozen_harness" \
  audit-soak "$formal" 86400 120 6 1 >/dev/null
env N42_QUAL_RUNTIME="$runtime" "$frozen_harness" \
  audit-soak "$post_burst" 600 120 6 0 >/dev/null
env N42_QUAL_RUNTIME="$runtime" "$frozen_harness" \
  audit-soak "$post_restart" 600 120 6 0 >/dev/null
env N42_QUAL_RUNTIME="$runtime" "$frozen_harness" \
  audit-rust-resources "$resources" 86400 >/dev/null
jq -e -s --arg expected "$expected_gov_upstream" '
  length >= 2 and
  all(.[];
    .event == "gov5_upstream_snapshot" and .baseline == $expected and
    .remoteMain == $expected and .remoteReachable == true and
    .baselineExact == true) and
  ([.[].at | fromdateiso8601] as $times |
    ($times[-1] - $times[0]) >= 86400 and
    ([range(1; $times | length) as $i |
      ($times[$i] - $times[$i - 1]) > 0 and
      ($times[$i] - $times[$i - 1]) <= 700] | all))
' "$upstream" >/dev/null

leader_start="$(jq -er '.rustLeaderAudit.startHeight' "$summary")"
leader_end="$(jq -er '.rustLeaderAudit.endHeight' "$summary")"
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT=29545 \
  N42_QUAL_RUST_MINER=0x81d4c1f92ddb837cb46f82280d9b491b101fa582 \
  N42_QUAL_RUST_LOG="$final_rust_log" \
  "$frozen_harness" audit-rust-leaders "$leader_start" "$leader_end" \
    /dev/null >/dev/null
env N42_QUAL_RUNTIME="$final_log_root" N42_QUAL_RUST_PORT=29545 \
  "$frozen_harness" audit-timeout-recovery "$final_rust_log" \
    /dev/null >/dev/null
env N42_QUAL_RUNTIME="$final_log_root" "$frozen_harness" \
  audit-runtime-logs "$final_rust_log" /dev/null >/dev/null
env N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_QMDB_PROOF_VERIFY="$frozen_qmdb_verifier" \
  "$frozen_harness" archive-rpc-parity \
    http://127.0.0.1:28501 http://127.0.0.1:29545 /dev/null

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg summary "$summary" \
  --arg summary_sha256 "$(sha256 "$summary")" \
  --arg verifier_sha "${expected_verifier_script_sha:-$(sha256 "$verifier_script")}" \
  --arg harness_sha "$expected_harness_sha" \
  '{at:$at,event:"gov5_906_independent_final_verification",status:"PASS",
    summary:$summary,summarySha256:$summary_sha256,
    verifierScriptSha256:$verifier_sha,
    qualificationHarnessSha256:$harness_sha,
    allEvidenceHashesRecomputedExact:true,allEmbeddedAuditsExact:true,
    independentRawAuditsReexecuted:true,liveArchiveParityReexecuted:true,
    immutableFinalLogExact:true,
    liveChainExact:true,genesisExact:true,consensusReady:true,
    zeroEquivocations:true,pinnedInputsExact:true,sourcesAndRemotesExact:true,
    finalSenderNonce:"0x22",transactionsFinalized:17}'
