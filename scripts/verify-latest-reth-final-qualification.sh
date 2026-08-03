#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_QUAL_RUNTIME:-/Users/jieliu/Documents/n42/live-interop-20260721/runtime-24-gov5-9c8-latest-reth}"
qualification_dir="${N42_LATEST_RETH_QUAL_DIR:-/Users/jieliu/Documents/n42/live-interop-20260721/post-qualification-latest-reth-20260803-ddc}"
source_repo="${N42_LATEST_RETH_SOURCE_REPO:-/Users/jieliu/Documents/n42/interop-reth-latest-20260802/n42-26}"
primary_repo="${N42_LATEST_RETH_PRIMARY_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/n42-26}"
gov_repo="${N42_LATEST_RETH_GOV_REPO:-/Users/jieliu/Documents/n42/live-interop-20260721/N42-gov5-current-main-20260803}"
expected_source_commit="${N42_LATEST_RETH_SOURCE_COMMIT:?latest-Reth source commit is required}"
expected_self_sha="${N42_LATEST_RETH_VERIFY_EXPECTED_SELF_SHA256:-}"
preflight_only="${N42_LATEST_RETH_VERIFY_PREFLIGHT_ONLY:-0}"
script_path="${BASH_SOURCE[0]}"
ports="${N42_QUAL_PORTS:-28501 28502 28503 28504 28505 29545}"
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
expected_binary_sha="0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f"
expected_reth_commit="91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b"
expected_gov_commit="${N42_LATEST_RETH_GOV_COMMIT:-653d494d0dc48ce679c613f18a880b0daecffe92}"
expected_gov_upstream="${N42_LATEST_RETH_GOV_UPSTREAM:-9c821032e0cb77638bdd78fd5e50e70357f39954}"
expected_harness_sha="6b95241f06fbf2225e9dff8a9bd4534ac5c1363f6f62109883695ebf7db189ab"
latest_binary="$qualification_dir/n42-node"
harness="$runtime/artifacts/scripts/gov5-interop-qualification.sh"
summary="$qualification_dir/latest-reth-final-qualification.json"
heads="$qualification_dir/latest-reth-heads-1h.jsonl"
head_audit="$qualification_dir/latest-reth-heads-1h-audit.json"
resources="$qualification_dir/latest-reth-resources-1h.jsonl"
resource_audit="$qualification_dir/latest-reth-resources-1h-audit.json"
rollover="$qualification_dir/latest-reth-rollover.jsonl"
leaders="$qualification_dir/latest-reth-leader-audit.jsonl"
timeouts="$qualification_dir/latest-reth-timeout-recovery-audit.jsonl"
runtime_logs="$qualification_dir/latest-reth-runtime-log-audit.jsonl"
latest_log="$qualification_dir/latest-reth-rust.log"
failures="$qualification_dir/latest-reth-failures.jsonl"
snapshot="$qualification_dir/pre-latest-reth-rust-data"
source_manifest="$qualification_dir/pre-latest-reth-source-manifest.sha256"
snapshot_manifest="$qualification_dir/pre-latest-reth-snapshot-manifest.sha256"
strict_independent="$runtime/evidence/gov5-906-independent-final-verification.json"

sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

require_file() {
  test -f "$1" || {
    echo "missing required file: $1" >&2
    return 1
  }
}

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

resolve_official_stable() {
  local tags latest attempt
  for attempt in $(seq 1 6); do
    if tags="$(git ls-remote --tags https://github.com/paradigmxyz/reth.git \
      'refs/tags/v*')"; then
      latest="$(sed -E 's#.*refs/tags/##; s/\^\{\}//' <<<"$tags" |
        rg -v -- '-(alpha|beta|rc)[.-]' | sort -V | tail -n 1)"
      test -n "$latest"
      printf '%s\n' "$latest"
      return 0
    fi
    sleep 10
  done
  return 1
}

assert_sources() {
  local branch remote remote_main latest_stable
  test "$(git -C "$source_repo" rev-parse HEAD)" = \
    "$(git -C "$source_repo" rev-parse "$expected_source_commit^{commit}")"
  test -z "$(git -C "$source_repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$source_repo" rev-parse --abbrev-ref HEAD)"
  remote="$(git -C "$source_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$(git -C "$source_repo" rev-parse HEAD)"

  test -z "$(git -C "$primary_repo" status --porcelain --untracked-files=no)"
  test "$(git -C "$primary_repo" rev-parse HEAD)" = \
    "$(git -C "$primary_repo" rev-parse '@{upstream}')"

  test "$(git -C "$gov_repo" rev-parse HEAD)" = "$expected_gov_commit"
  test -z "$(git -C "$gov_repo" status --porcelain --untracked-files=no)"
  branch="$(git -C "$gov_repo" rev-parse --abbrev-ref HEAD)"
  remote="$(git -C "$gov_repo" ls-remote origin "refs/heads/$branch" |
    awk 'NR==1 {print $1}')"
  remote_main="$(git -C "$gov_repo" ls-remote origin refs/heads/main |
    awk 'NR==1 {print $1}')"
  test "$remote" = "$expected_gov_commit"
  test "$remote_main" = "$expected_gov_upstream"

  latest_stable="$(resolve_official_stable)"
  test "$latest_stable" = v2.4.1
}

assert_live_chain() {
  local expected="" identity hash port exact attempt
  for port in $ports; do
    hash="$(rpc "$port" eth_getBlockByNumber '["0x0",false]' | jq -er '.result.hash')"
    test "$hash" = "$expected_genesis"
  done
  for attempt in $(seq 1 30); do
    expected=""
    exact=true
    for port in $ports; do
      identity="$(rpc "$port" eth_getBlockByNumber '["latest",false]' |
        jq -er '.result|[.number,.hash,.stateRoot,.receiptsRoot]|join(":")')"
      if test -z "$expected"; then
        expected="$identity"
      elif test "$identity" != "$expected"; then
        exact=false
        break
      fi
    done
    test "$exact" = true && return 0
    sleep 1
  done
  return 1
}

assert_nonce() {
  local expected="$1" port latest pending
  for port in $ports; do
    latest="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","latest"]' | jq -er '.result')"
    pending="$(rpc "$port" eth_getTransactionCount \
      '["0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","pending"]' | jq -er '.result')"
    test "$latest" = "$expected"
    test "$pending" = "$expected"
  done
}

assert_static() {
  local version
  if test -n "$expected_self_sha"; then
    test "$(sha256 "$script_path")" = "$expected_self_sha"
  fi
  require_file "$latest_binary"
  require_file "$harness"
  test "$(sha256 "$latest_binary")" = "$expected_binary_sha"
  test "$(sha256 "$harness")" = "$expected_harness_sha"
  version="$("$latest_binary" --version)"
  grep -F 'Reth Version: 2.4.1' <<<"$version" >/dev/null
  grep -F "Commit SHA: $expected_reth_commit" <<<"$version" >/dev/null
  assert_sources
  assert_live_chain
}

assert_static
if test "$preflight_only" = 1; then
  assert_nonce 0x11
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg self "$(sha256 "$script_path")" \
    '{at:$at,event:"latest_reth_independent_verifier_preflight",status:"PASS",
      verifierSha256:$self,officialStableTag:"v2.4.1",
      liveSixEndpointIdentityExact:true,genesisExact:true,
      latestAndPendingNonce:"0x11",mutationPerformed:false}'
  exit 0
fi

for path in "$summary" "$heads" "$head_audit" "$resources" \
  "$resource_audit" "$rollover" "$leaders" "$timeouts" "$runtime_logs" \
  "$latest_log" "$source_manifest" "$snapshot_manifest" "$strict_independent"; do
  require_file "$path"
done
test -d "$snapshot"
test ! -s "$failures"
cmp -s "$source_manifest" "$snapshot_manifest"
jq -e '.status=="PASS"' "$strict_independent" >/dev/null

jq -e --arg binary_sha "$expected_binary_sha" --arg reth "$expected_reth_commit" '
  .status=="PASS" and .binarySha256==$binary_sha and .rethVersion=="2.4.1" and
  .rethCommit==$reth and .officialStableTag=="v2.4.1" and
  .officialStableTagExact==true and .sourceAndGovUpstreamStillExact==true and
  .headGrowth>0 and .headAudit.status=="PASS" and
  .headAudit.elapsedSeconds>=3600 and .resourceAudit.status=="PASS" and
  .resourceAudit.elapsedSeconds>=3600 and .rustLeaderAudit.status=="PASS" and
  .rustLeaderAudit.rustAuthoredBlocks>=1 and
  .rustLeaderAudit.leaderCommitLog.allVotesFivePlusFive==true and
  .timeoutAudit.status=="PASS" and .timeoutAudit.completedTimeouts>=1 and
  .timeoutAudit.pendingTimeouts==0 and
  .timeoutAudit.everyCompletedTimeoutRecoveredAtNextView==true and
  .timeoutAudit.recoveredByRustVotesFivePlusFive==true and
  .runtimeLogAudit.status=="PASS" and .runtimeLogAudit.unexpectedWarnings==0 and
  .runtimeLogAudit.criticalSignals==0 and .consensus.hasCommittedQc==true and
  .consensus.validatorCount==7 and .equivocations.total==0 and
  .preRolloverDataSnapshot.byteExact==true and .latestBinaryStillRunning==true
' "$summary" >/dev/null

test "$(jq -er '.headEvidenceSha256' "$summary")" = "$(sha256 "$heads")"
test "$(jq -er '.resourceEvidenceSha256' "$summary")" = "$(sha256 "$resources")"
test "$(jq -er '.leaderEvidenceSha256' "$summary")" = "$(sha256 "$leaders")"
test "$(jq -er '.timeoutEvidenceSha256' "$summary")" = "$(sha256 "$timeouts")"
test "$(jq -er '.runtimeLogAuditSha256' "$summary")" = "$(sha256 "$runtime_logs")"
test "$(jq -er '.rolloverEvidenceSha256' "$summary")" = "$(sha256 "$rollover")"
test "$(jq -er '.latestRustLogSha256' "$summary")" = "$(sha256 "$latest_log")"
test "$(jq -er '.strictIndependentVerificationSha256' "$summary")" = \
  "$(sha256 "$strict_independent")"
test "$(jq -er '.preRolloverDataSnapshot.sourceManifestSha256' "$summary")" = \
  "$(sha256 "$source_manifest")"
test "$(jq -er '.preRolloverDataSnapshot.snapshotManifestSha256' "$summary")" = \
  "$(sha256 "$snapshot_manifest")"

jq -e --slurpfile artifact "$head_audit" '.headAudit==$artifact[0]' "$summary" >/dev/null
jq -e --slurpfile artifact "$resource_audit" '.resourceAudit==$artifact[0]' "$summary" >/dev/null
jq -e --slurpfile artifact "$leaders" '.rustLeaderAudit==$artifact[-1]' "$summary" >/dev/null
jq -e --slurpfile artifact "$timeouts" '.timeoutAudit==$artifact[-1]' "$summary" >/dev/null
jq -e --slurpfile artifact "$runtime_logs" '.runtimeLogAudit==$artifact[-1]' "$summary" >/dev/null

audit_dir="$(mktemp -d /tmp/n42-latest-reth-independent.XXXXXX)"
env N42_QUAL_RUNTIME="$runtime" "$harness" audit-soak "$heads" 3600 120 6 0 \
  >"$audit_dir/heads.json"
env N42_QUAL_RUNTIME="$runtime" "$harness" audit-rust-resources "$resources" 3600 \
  "$audit_dir/resources.json" >/dev/null
start_height="$(jq -er '.rustLeaderAudit.startHeight' "$summary")"
end_height="$(jq -er '.rustLeaderAudit.endHeight' "$summary")"
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" N42_QUAL_RUST_PORT=29545 \
  N42_QUAL_RUST_MINER=0x81d4c1f92ddb837cb46f82280d9b491b101fa582 \
  N42_QUAL_RUST_LOG="$latest_log" "$harness" audit-rust-leaders \
  "$start_height" "$end_height" "$audit_dir/leaders.jsonl" >/dev/null
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_RUST_PORT=29545 \
  "$harness" audit-timeout-recovery "$latest_log" "$audit_dir/timeouts.jsonl" >/dev/null
env N42_QUAL_RUNTIME="$runtime" "$harness" audit-runtime-logs "$latest_log" \
  "$audit_dir/runtime-logs.jsonl" >/dev/null

for pair in \
  "$audit_dir/heads.json:$head_audit" \
  "$audit_dir/resources.json:$resource_audit" \
  "$audit_dir/leaders.jsonl:$leaders" \
  "$audit_dir/runtime-logs.jsonl:$runtime_logs"; do
  actual="${pair%%:*}"
  expected="${pair#*:}"
  jq -e --slurpfile expected "$expected" 'del(.at)==($expected[-1]|del(.at))' \
    "$actual" >/dev/null
done
jq -e --slurpfile expected "$timeouts" \
  'del(.at,.latestCommittedView)==($expected[-1]|del(.at,.latestCommittedView))' \
  "$audit_dir/timeouts.jsonl" >/dev/null

pid="$(jq -er '.pidAfter' "$summary")"
test "$(<"$runtime/pids/rust.pid")" = "$pid"
kill -0 "$pid"
case "$(ps -p "$pid" -o command=)" in
  "$latest_binary node "*) ;;
  *) echo "latest Reth process command mismatch" >&2; exit 1 ;;
esac
assert_live_chain
assert_nonce 0x22
assert_sources

jq -nc --arg at "$(date -u +%FT%TZ)" --arg summary "$summary" \
  --arg summary_sha "$(sha256 "$summary")" --arg verifier_sha "$(sha256 "$script_path")" \
  --arg audit_dir "$audit_dir" --argjson pid "$pid" \
  '{at:$at,event:"latest_reth_independent_final_verification",status:"PASS",
    summary:$summary,summarySha256:$summary_sha,verifierSha256:$verifier_sha,
    reexecutedAudits:true,recomputedEvidenceHashes:true,
    snapshotManifestsByteExact:true,officialStableTag:"v2.4.1",
    sourceAndRemotesExact:true,liveSixEndpointIdentityExact:true,
    genesisExact:true,latestAndPendingNonce:"0x22",latestRustPid:$pid,
    temporaryAuditDirectory:$audit_dir}'
