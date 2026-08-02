#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-18-gov5-906-latest-reth}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
gov_repo="${N42_QUAL_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260801}"
deps_repo="${N42_QUAL_DEPS_REPO:-/Users/jieliu/Documents/n42/deps-latest-20260721/n42-26}"
reth_repo="${N42_QUAL_RETH_REPO:-/Users/jieliu/Documents/n42/deps-latest-20260721/reth}"
preflight_only="${N42_VERIFY_PREFLIGHT_ONLY:-0}"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_gov_upstream="920f7536eb263b6744b48f28dfeb77f4c2798c1a"
expected_gov_candidate="8915b4cc07d82dc195daee2e8e741ea5e8446068"
expected_deps_head="aec34a0cd465e8fdbb598b90bc778fe96e25d6c0"
expected_reth_head="91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b"

require_file() {
  test -f "$1" || {
    echo "missing required file: $1" >&2
    return 1
  }
}

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
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
  remote="$(git -C "$gov_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR == 1 {print $1}')"
  test "$remote" = "$expected_gov_candidate"
  remote="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
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
  latest_stable="$(git ls-remote --tags https://github.com/paradigmxyz/reth.git \
    'refs/tags/v*' | sed -E 's#.*refs/tags/##; s/\^\{\}//' |
    rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
  test "$latest_stable" = v2.4.1
}

assert_pinned_inputs() {
  assert_sha "$runtime/geth-live" \
    51e68918560be65f8e5221f02a3d544a7baf42bed9aa86655623449a4fd765d0
  assert_sha "$runtime/n42-node" \
    d917782b906176119172e656005218be34ec3d5ad1b7241c0c53f8f6d593da2d
  assert_sha "$runtime/artifacts/genesis.json" \
    561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687
  assert_sha "$runtime/artifacts/consensus-peer-bound.json" \
    38cd3fb1f57e5e3053e23de836b7c98e542ccb5375d0521a65b5c2f6175bd8bf
  assert_sha "$runtime/artifacts/bootstrap-bundle.json" \
    35dda59684e7f56978e5d8de385fa2d2bf15b47747388b88a7449ac31387bf15
  assert_sha "$runtime/artifacts/scripts/gov5-interop-qualification.sh" \
    bd5fafe7b47a8613252c977d0060ccd25e2e1ee6fba949c8f28e0b9feda95d5e
  assert_sha "$runtime/artifacts/scripts/gov5-current-qualification-finalizer.sh" \
    dc6f26ecd1b2d94a192266e15ec95e39f79d3cdcf7295308caf53a8da391654a
  assert_sha "$runtime/artifacts/binaries/n42-qmdb-proof-verify" \
    b329baa1e51435082b2bb2cf538a8d1a1ffd994b5c4ac73474e688ffbfc35c19
  assert_sha "$runtime/artifacts/validator-keys/node0/keystore/bls_81d4c1f92ddb837cb46f82280d9b491b101fa582.key" \
    babd0b3550da7702230d3da9a3f00bfce741ed9f1fb8210b702c6023080ea509
  assert_sha "$runtime/artifacts/validator-keys/node0/network-keys" \
    d82561e312fbb044f56eec5f434f03ea1e852924f055a8949ea82be9e7bbe277
}

assert_live_chain
assert_live_consensus
assert_pinned_inputs
assert_sources

if test "$preflight_only" = 1; then
  assert_sender_nonce 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" \
    '{at:$at,event:"gov5_906_independent_final_verifier_preflight",
      status:"PASS",liveChainExact:true,genesisExact:true,
      consensusReady:true,zeroEquivocations:true,pinnedInputsExact:true,
      sourcesAndRemotesExact:true,senderNonce:"0x11",transactionsSent:0}'
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
resources="$runtime/evidence/rust-resource-24h.jsonl"
resource_audit="$runtime/evidence/rust-resource-24h-audit.json"
failures="$runtime/evidence/gov5-906-finalizer-failures.jsonl"

for path in "$summary" "$formal" "$soak_audit" "$upstream" \
  "$upstream_complete" "$upstream_audit" "$burst" "$post_burst" \
  "$post_burst_audit" "$archive_post_burst" "$restart" "$post_restart" \
  "$post_restart_audit" "$leaders" "$timeouts" "$runtime_logs" \
  "$resources" "$resource_audit"; do
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
  .rustResourceAudit.status == "PASS" and
  .rustResourceAudit.elapsedSeconds >= 86400 and
  .rustResourceAudit.singleProcess == true and
  .rustResourceAudit.storageAndLogCountersMonotonic == true
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

jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg summary "$summary" \
  --arg summary_sha256 "$(sha256 "$summary")" \
  '{at:$at,event:"gov5_906_independent_final_verification",status:"PASS",
    summary:$summary,summarySha256:$summary_sha256,
    allEvidenceHashesRecomputedExact:true,allEmbeddedAuditsExact:true,
    liveChainExact:true,genesisExact:true,consensusReady:true,
    zeroEquivocations:true,pinnedInputsExact:true,sourcesAndRemotesExact:true,
    finalSenderNonce:"0x22",transactionsFinalized:17}'
