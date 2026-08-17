#!/usr/bin/env bash
set -euo pipefail

# Final, read-only acceptance gate for the complete mixed topology: five Gov5
# validators plus Rust validators in slots 0 and 6.  Older finalizers assume
# a six-endpoint topology and therefore must not be used for this run.

runtime="${N42_FINAL_RUNTIME:?runtime is required}"
gov_repo="${N42_FINAL_GOV_REPO:?Gov5 repository is required}"
expected_gov_main="${N42_FINAL_EXPECTED_GOV_MAIN:?expected Gov5 main SHA is required}"
expected_gov_binary="${N42_FINAL_GOV_BINARY_SHA:?Gov5 binary SHA-256 is required}"
expected_rust_binary="${N42_FINAL_RUST_BINARY_SHA:?Rust binary SHA-256 is required}"
log_start="${N42_FINAL_LOG_START:-}"
qualification="${N42_FINAL_QUALIFICATION_SCRIPT:-$runtime/artifacts/scripts/gov5-interop-qualification.sh}"
output="${N42_FINAL_OUTPUT:-$runtime/evidence/runtime42-seven-validator-final-verification.json}"

evidence_dir="${N42_FINAL_EVIDENCE_DIR:-$runtime/evidence}"
heads="${N42_FINAL_HEADS:-$evidence_dir/runtime42-seven-validator-24h-head-monitor.jsonl}"
upstream="${N42_FINAL_UPSTREAM:-$evidence_dir/runtime42-gov5-upstream-24h.jsonl}"
upstream_complete="${N42_FINAL_UPSTREAM_COMPLETE:-${upstream%.jsonl}-complete.json}"
rust0_resources="${N42_FINAL_RUST0_RESOURCES:-$evidence_dir/runtime42-rust0-resource-24h.jsonl}"
rust6_resources="${N42_FINAL_RUST6_RESOURCES:-$evidence_dir/runtime42-rust6-resource-24h.jsonl}"
rust0_leaders="${N42_FINAL_RUST0_LEADERS:?final Rust0 leader audit is required}"
rust6_leaders="${N42_FINAL_RUST6_LEADERS:?final Rust6 leader audit is required}"
ports=(28501 28502 28503 28504 28505 29545 29546)
expected_genesis="0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec"
maximum_sample_gap="${N42_FINAL_MAX_SAMPLE_GAP_SECONDS:-120}"
maximum_lag="${N42_FINAL_MAX_LAG:-6}"
copied_head="${N42_FINAL_COPIED_HEAD:-92605}"
expected_copied_hash="${N42_FINAL_COPIED_HASH:-0xb88a3571223cf8cd8291d608572a55f306ea88957cc7ede8ab6b8812ada85a82}"

require_file() { test -f "$1" || { echo "missing required file: $1" >&2; exit 2; }; }
sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
rpc() {
  local port="$1" method="$2"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" \
      '{jsonrpc:"2.0",id:1,method:$method,params:[]}')" \
    "http://127.0.0.1:$port"
}

require_file "$qualification"
require_file "$heads"
require_file "$upstream"
require_file "$upstream_complete"
require_file "$rust0_resources"
require_file "$rust6_resources"
require_file "$rust0_leaders"
require_file "$rust6_leaders"
test ! -e "$output"
test "$(sha256 "$runtime/geth-live")" = "$expected_gov_binary"
test "$(sha256 "$runtime/n42-node")" = "$expected_rust_binary"
test "$(git -C "$gov_repo" ls-remote origin refs/heads/main | awk 'NR==1{print $1}')" = "$expected_gov_main"

jq -e --arg expected "$expected_gov_main" '
  .status == "PASS" and .expectedMain == $expected and
  .requestedDurationSeconds >= 86400 and .elapsedSeconds >= 86400 and
  .samples >= 2
' "$upstream_complete" >/dev/null
jq -s -e --arg expected "$expected_gov_main" '
  length >= 2 and all(.[]; .baselineExact == true and .remoteReachable == true and
    .remoteMain == $expected and .baseline == $expected)
' "$upstream" >/dev/null

for pid_file in "$runtime"/pids/gov{1,2,3,4,5}.pid "$runtime/pids/rust.pid" "$runtime/pids/rust2.pid"; do
  require_file "$pid_file"
  kill -0 "$(<"$pid_file")"
done

for port in "${ports[@]}"; do
  test "$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["0x0",false]}' \
    "http://127.0.0.1:$port" | jq -er '.result.hash')" = "$expected_genesis"
done

copied_tag="$(printf '0x%x' "$copied_head")"
copied_identity=""
for port in "${ports[@]}"; do
  identity="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg tag "$copied_tag" \
      '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$tag,false]}')" \
    "http://127.0.0.1:$port" | jq -er '.result|[.number,.hash,.stateRoot,.receiptsRoot,.transactionsRoot]|join(":")')"
  test "$(cut -d: -f1 <<<"$identity")" = "$copied_tag"
  test "$(cut -d: -f2 <<<"$identity")" = "$expected_copied_hash"
  test -z "$copied_identity" && copied_identity="$identity"
  test "$identity" = "$copied_identity"
done

latest_min=-1
latest_max=-1
for port in "${ports[@]}"; do
  height_hex="$(rpc "$port" eth_blockNumber | jq -er '.result')"
  height=$((height_hex))
  if test "$latest_min" -lt 0 || test "$height" -lt "$latest_min"; then
    latest_min="$height"
  fi
  if test "$latest_max" -lt 0 || test "$height" -gt "$latest_max"; then
    latest_max="$height"
  fi
done
test "$((latest_max - latest_min))" -le "$maximum_lag"
common_tag="$(printf '0x%x' "$latest_min")"
common_identity=""
for port in "${ports[@]}"; do
  identity="$(curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg tag "$common_tag" \
      '{jsonrpc:"2.0",id:1,method:"eth_getBlockByNumber",params:[$tag,false]}')" \
    "http://127.0.0.1:$port" | jq -ec '.result|[.number,.hash,.stateRoot,.receiptsRoot]')"
  test -z "$common_identity" && common_identity="$identity"
  test "$identity" = "$common_identity"
done
for port in 29545 29546; do
  rpc "$port" n42_consensusStatus | jq -e '.result.validatorCount == 7 and .result.hasCommittedQc == true' >/dev/null
  rpc "$port" n42_equivocations | jq -e '.result.total == 0 and (.result.evidence|length) == 0' >/dev/null
done

head_audit="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="${ports[*]}" \
  "$qualification" audit-soak "$heads" 86400 "$maximum_sample_gap" "$maximum_lag" 1)"
rust0_audit="$(env N42_QUAL_RUNTIME="$runtime" "$qualification" audit-rust-resources "$rust0_resources" 86400)"
rust6_audit="$(env N42_QUAL_RUNTIME="$runtime" "$qualification" audit-rust-resources "$rust6_resources" 86400)"
printf '%s\n' "$head_audit" | jq -e --argjson maximum_lag "$maximum_lag" \
  '.status == "PASS" and .maximumLag <= $maximum_lag and .zeroTransactionRequired == true' >/dev/null
printf '%s\n' "$rust0_audit" | jq -e '.status == "PASS" and .singleProcess and .logicalCountersMonotonic' >/dev/null
printf '%s\n' "$rust6_audit" | jq -e '.status == "PASS" and .singleProcess and .logicalCountersMonotonic' >/dev/null
for leader_file in "$rust0_leaders" "$rust6_leaders"; do
  jq -e '.status == "PASS" and .leaderStride == 7 and .rustAuthoredBlocks > 0 and
    .parentChainContinuous and .expectedLeaderSlotsExact and .allConfiguredEndpointsExact and
    (.ports | length == 7)' "$leader_file" >/dev/null
done
for log in "$runtime"/logs/gov{1,2,3,4,5}.log "$runtime"/logs/rust.log "$runtime"/logs/rust2.log; do
  require_file "$log"
  # Do not use a global case-insensitive `error` match: the consensus transport
  # records harmless startup de-duplication as `error=Duplicate`.  Structured
  # ERROR remains strict, while fatal signals are case-insensitive.  A reused
  # 905 runtime may contain an older run's log history; when the caller gives
  # this run's ISO-8601 start (seconds precision), only newer lines are audited.
  if [ -n "$log_start" ]; then
    ! awk -v start="$log_start" \
      'substr($0,1,19) >= start && index($0," ERROR ") {bad=1} END {exit bad}' "$log"
    ! awk -v start="$log_start" \
      'substr($0,1,19) >= start && tolower($0) ~ /(^|[^[:alpha:]])(panic|fatal|equivocat)/ {bad=1} END {exit bad}' "$log"
  else
    ! rg -q ' ERROR ' "$log"
    ! rg -qi '(^|[^[:alpha:]])(panic|fatal|equivocat)' "$log"
  fi
done

jq -nc --arg at "$(date -u +%FT%TZ)" --arg runtime "$runtime" \
  --arg gov_main "$expected_gov_main" --arg genesis "$expected_genesis" \
  --arg copied_head "$copied_tag" --arg copied_hash "$expected_copied_hash" \
  --arg copied_identity "$copied_identity" \
  --arg common_height "$common_tag" --arg common_identity "$common_identity" \
  --argjson latest_lag "$((latest_max - latest_min))" --argjson maximum_lag "$maximum_lag" \
  --argjson head_audit "$head_audit" --argjson rust0_audit "$rust0_audit" \
  --argjson rust6_audit "$rust6_audit" \
  '{at:$at,event:"gov5_seven_validator_final_verification",status:"PASS",
    runtime:$runtime,govMain:$gov_main,genesis:$genesis,ports:[28501,28502,28503,28504,28505,29545,29546],
    reused905Data:{copiedPersistedHead:$copied_head,copiedPersistedHash:$copied_hash,
      allConfiguredEndpointsExact:true,identity:$copied_identity},
    liveCommonHeightIdentityExact:true,commonHeight:$common_height,commonIdentity:$common_identity,
    latestLag:$latest_lag,maximumLagBound:$maximum_lag,
    validatorCount:7,rustValidators:2,committedQc:true,equivocations:0,
    headAudit:$head_audit,rust0ResourceAudit:$rust0_audit,rust6ResourceAudit:$rust6_audit,
    bothRustLeaderAuditsExact:true,criticalLogs:0}' >"$output"
cat "$output"
