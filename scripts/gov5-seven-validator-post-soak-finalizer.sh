#!/usr/bin/env bash
set -euo pipefail

runtime="${N42_POST_RUNTIME:?runtime is required}"
formalizer_pid="${N42_POST_FORMALIZER_PID:?formal finalizer PID is required}"
qualification="${N42_POST_QUALIFICATION_SCRIPT:?qualification script is required}"
formal="${N42_POST_FORMAL_VERIFICATION:?formal verification artifact is required}"
formal_failure="${N42_POST_FORMAL_FAILURE:?formal failure artifact is required}"
artifact="${N42_POST_TRANSACTION_ARTIFACT:-$runtime/artifacts/p4-signed-transaction-burst.json}"
burst="${N42_POST_BURST_EVIDENCE:?transaction burst evidence is required}"
heads="${N42_POST_HEADS:?post-transaction head evidence is required}"
output="${N42_POST_OUTPUT:?total final verification output is required}"
failure="${N42_POST_FAILURE:?post-soak failure output is required}"
ports='28501 28502 28503 28504 28505 29545 29546'
preflight_only="${N42_POST_PREFLIGHT_ONLY:-0}"
[[ "$preflight_only" =~ ^[01]$ ]]

fail() {
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason "$1" \
    '{at:$at,event:"gov5_post_soak_execution_finalizer",status:"FAIL",reason:$reason}' \
    >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit 1
}

rpc() {
  local port="${1:?port required}" method="${2:?method required}"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" \
      '{jsonrpc:"2.0",id:1,method:$method,params:[]}')" \
    "http://127.0.0.1:$port"
}

test -x "$qualification" || fail "missing executable: $qualification"
test -s "$artifact" || fail "missing transaction artifact: $artifact"
test ! -e "$burst" || fail "transaction evidence already exists: $burst"
test ! -e "$heads" || fail "post-transaction head evidence already exists: $heads"
test ! -e "$output" || fail "total verification output already exists: $output"
test ! -e "$failure" || exit 1

while ! test -f "$formal"; do
  test ! -f "$formal_failure" || fail 'formal 24h finalizer reported failure'
  kill -0 "$formalizer_pid" 2>/dev/null || \
    fail 'formal 24h finalizer exited without verification artifact'
  sleep 60
done

jq -e '
  .event == "gov5_seven_validator_final_verification" and .status == "PASS" and
  .headAudit.elapsedSeconds >= 86400 and .headAudit.maximumLag <= 6 and
  (.milestoneAudits | length) == 4 and
  [.milestoneAudits[].milestoneSeconds] == [3600,21600,43200,64800] and
  all(.milestoneAudits[]; .status == "PASS") and
  .liveCommonHeightIdentityExact and .bothRustLeaderAuditsExact and
  .sevenEndpointEvmExecutionExact and .rustRestartCatchupExact and
  .rust0ResourceAudit.status == "PASS" and .rust6ResourceAudit.status == "PASS" and
  .rust0FormalLogAudit.unexpectedWarnings == 0 and
  .rust6FormalLogAudit.unexpectedWarnings == 0 and
  .rust0FormalLogAudit.criticalSignals == 0 and
  .rust6FormalLogAudit.criticalSignals == 0 and .equivocations == 0
' "$formal" >/dev/null || fail 'formal 24h verification artifact is not a complete PASS'

jq -e '.transactions | length == 17' "$artifact" >/dev/null || \
  fail 'transaction artifact does not contain exactly 17 transactions'

if test "$preflight_only" = 1; then
  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    N42_QUAL_GOV_INGRESS_PORT=28501 N42_QUAL_RUST_INGRESS_PORT=29545 \
    N42_QUAL_BURST_PREFLIGHT_ONLY=1 \
    "$qualification" transaction-burst "$artifact" "$burst" >/dev/null || \
    fail 'mixed Go/Rust transaction burst preflight failed'
  jq -e '
    .event == "p4_transaction_burst_preflight" and .transactionsSent == 0 and
    .expectedNonce == "0x11" and .allConfiguredEndpointNoncesExact and
    ([.ports[]] | sort) == [28501,28502,28503,28504,28505,29545,29546]
  ' "$burst" >/dev/null || fail 'transaction burst preflight evidence is incomplete'
  cat "$burst"
  exit 0
fi

transaction_started_at="$(date -u +%FT%TZ)"
env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_GOV_INGRESS_PORT=28501 N42_QUAL_RUST_INGRESS_PORT=29545 \
  "$qualification" transaction-burst "$artifact" "$burst" >/dev/null || \
  fail 'mixed Go/Rust transaction burst failed'

jq -e -s --slurpfile artifact "$artifact" '
  (map(select(.event == "p4_transaction_finalized"))) as $finalized |
  (map(select(.event == "p4_transaction_burst_pass"))) as $passes |
  ($finalized | length) == ($artifact[0].transactions | length) and
  ($passes | length) == 1 and
  ([range(0; $finalized | length) as $index |
    $finalized[$index].nonce == $artifact[0].transactions[$index].nonce and
    $finalized[$index].kind == $artifact[0].transactions[$index].kind and
    $finalized[$index].ingress == $artifact[0].transactions[$index].intendedIngress and
    $finalized[$index].transactionHash == $artifact[0].transactions[$index].hash and
    $finalized[$index].status == "0x1"] | all) and
  $passes[0].transactions == 17 and $passes[0].endpointCount == 7 and
  $passes[0].allSevenEndpointsExact and $passes[0].receiptAndLogParity and
  $passes[0].stateAndStorageParity and $passes[0].exactRpcComparisons >= 301
' "$burst" >/dev/null || fail 'transaction burst evidence is incomplete'

env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  N42_QUAL_RUST_PORT=29546 N42_QUAL_MAX_LAG=6 N42_QUAL_REQUIRE_ZERO_TX=0 \
  "$qualification" monitor-heads 180 5 "$heads" || \
  fail 'post-transaction mixed-client head monitor failed'
post_audit="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  "$qualification" audit-soak "$heads" 180 15 6 0)" || \
  fail 'post-transaction mixed-client head audit failed'
printf '%s\n' "$post_audit" | jq -e '
  .status == "PASS" and .elapsedSeconds >= 180 and .blockGrowth > 0 and
  .maximumLag <= 6
' >/dev/null || fail 'post-transaction head audit did not satisfy thresholds'

post_consensus='[]'
post_equivocations='[]'
for port in 29545 29546; do
  consensus="$(rpc "$port" n42_consensusStatus | jq -ec --argjson port "$port" '
    {port:$port,view:.result.latestCommittedView,
     hash:.result.latestCommittedBlockHash,
     validatorCount:.result.validatorCount,hasCommittedQc:.result.hasCommittedQc} |
    select(.validatorCount == 7 and .hasCommittedQc == true)
  ')" || fail "invalid post-transaction consensus status at port $port"
  equivocation="$(rpc "$port" n42_equivocations | jq -ec --argjson port "$port" '
    {port:$port,total:.result.total,evidenceCount:(.result.evidence | length)} |
    select(.total == 0 and .evidenceCount == 0)
  ')" || fail "post-transaction equivocation evidence at port $port"
  post_consensus="$(jq -nc --argjson current "$post_consensus" \
    --argjson item "$consensus" '$current + [$item]')"
  post_equivocations="$(jq -nc --argjson current "$post_equivocations" \
    --argjson item "$equivocation" '$current + [$item]')"
done

for log in "$runtime"/logs/gov{1,2,3,4,5}.log "$runtime"/logs/rust.log "$runtime"/logs/rust2.log; do
  ! awk -v start="${transaction_started_at%Z}" '
    substr($0,1,19) >= start &&
    (index($0," ERROR ") || tolower($0) ~ /(^|[^[:alpha:]])(panic|fatal|equivocat)/) {
      bad=1
    }
    END {exit bad}
  ' "$log" || fail "critical log signal after transaction burst: $log"
done

burst_pass="$(jq -sc 'map(select(.event == "p4_transaction_burst_pass"))[0]' "$burst")"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg transaction_started_at "$transaction_started_at" \
  --arg formal "$formal" --arg formal_sha256 "$(shasum -a 256 "$formal" | awk '{print $1}')" \
  --arg burst "$burst" --arg burst_sha256 "$(shasum -a 256 "$burst" | awk '{print $1}')" \
  --arg heads "$heads" --arg heads_sha256 "$(shasum -a 256 "$heads" | awk '{print $1}')" \
  --argjson formal_verification "$(<"$formal")" \
  --argjson burst_pass "$burst_pass" --argjson post_audit "$post_audit" \
  --argjson post_consensus "$post_consensus" \
  --argjson post_equivocations "$post_equivocations" '
  {at:$at,event:"gov5_seven_validator_total_final_verification",status:"PASS",
   transactionStartedAt:$transaction_started_at,
   topology:{gov5:5,rust:2,validators:7},
   formal24h:{artifact:$formal,sha256:$formal_sha256,verification:$formal_verification},
   onChainExecution:{artifact:$burst,sha256:$burst_sha256,pass:$burst_pass,
     signedTransactions:17,alternatingGoRustIngress:true,allSevenEndpointsExact:true,
     receiptsLogsStateAndStorageExact:true},
   postTransactionConvergence:{artifact:$heads,sha256:$heads_sha256,audit:$post_audit},
   postTransactionConsensus:{rust:$post_consensus,equivocations:$post_equivocations,
     validatorCount:7,committedQc:true,equivocationCount:0},
   criticalSignalsAfterTransactions:0}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
