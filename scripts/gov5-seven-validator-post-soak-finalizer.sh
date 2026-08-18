#!/usr/bin/env bash
set -Eeuo pipefail

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
resume_existing_burst="${N42_POST_RESUME_EXISTING_BURST:-0}"
current_stage='startup'

fail() {
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason "$1" \
    '{at:$at,event:"gov5_post_soak_execution_finalizer",status:"FAIL",reason:$reason}' \
    >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit 1
}

record_unexpected_failure() {
  local status="${1:?exit status required}"
  local command="${2:-unknown}"
  local line="${3:-0}"
  trap - ERR
  jq -nc --arg at "$(date -u +%FT%TZ)" --arg reason 'unexpected command failure' \
    --arg stage "$current_stage" --arg command "$command" \
    --argjson line "$line" --argjson exitStatus "$status" '
    {at:$at,event:"gov5_post_soak_execution_finalizer",status:"FAIL",reason:$reason,
     stage:$stage,command:$command,line:$line,exitStatus:$exitStatus}
  ' >"$failure.pending"
  mv "$failure.pending" "$failure"
  exit "$status"
}

rpc() {
  local port="${1:?port required}" method="${2:?method required}"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" \
      '{jsonrpc:"2.0",id:1,method:$method,params:[]}')" \
    "http://127.0.0.1:$port"
}

trap 'record_unexpected_failure "$?" "$BASH_COMMAND" "$LINENO"' ERR

current_stage='validate_configuration'
[[ "$preflight_only" =~ ^[01]$ ]] || fail 'preflight flag must be 0 or 1'
[[ "$resume_existing_burst" =~ ^[01]$ ]] || fail 'resume-existing-burst flag must be 0 or 1'
test -x "$qualification" || fail "missing executable: $qualification"
test -s "$artifact" || fail "missing transaction artifact: $artifact"
artifact_sha256="$(shasum -a 256 "$artifact" | awk '{print $1}')"
if test "$resume_existing_burst" = 1; then
  test -s "$burst" || fail "transaction evidence is required for resume: $burst"
else
  test ! -e "$burst" || fail "transaction evidence already exists: $burst"
fi
if test "$resume_existing_burst" = 1; then
  test ! -e "$heads" || test -s "$heads" || \
    fail "post-transaction head evidence is empty: $heads"
else
  test ! -e "$heads" || fail "post-transaction head evidence already exists: $heads"
fi
test ! -e "$output" || fail "total verification output already exists: $output"
test ! -e "$failure" || exit 1

current_stage='wait_for_formal_verification'
while ! test -f "$formal"; do
  test ! -f "$formal_failure" || fail 'formal 24h finalizer reported failure'
  kill -0 "$formalizer_pid" 2>/dev/null || \
    fail 'formal 24h finalizer exited without verification artifact'
  sleep 60
done

current_stage='validate_formal_verification'
jq -e --arg artifact_sha256 "$artifact_sha256" '
  .event == "gov5_seven_validator_final_verification" and .status == "PASS" and
  .headAudit.elapsedSeconds >= 86400 and .headAudit.maximumLag <= 6 and
  (.milestoneAudits | length) == 4 and
  [.milestoneAudits[].milestoneSeconds] == [3600,21600,43200,64800] and
  all(.milestoneAudits[]; .status == "PASS") and
  .liveCommonHeightIdentityExact and .bothRustLeaderAuditsExact and
  .sevenEndpointEvmExecutionExact and .rustRestartCatchupExact and
  .executionAudit.artifactSha256 == $artifact_sha256 and
  .rust0ResourceAudit.status == "PASS" and .rust6ResourceAudit.status == "PASS" and
  .rust0FormalLogAudit.unexpectedWarnings == 0 and
  .rust6FormalLogAudit.unexpectedWarnings == 0 and
  .rust0FormalLogAudit.criticalSignals == 0 and
  .rust6FormalLogAudit.criticalSignals == 0 and .equivocations == 0
' "$formal" >/dev/null || fail 'formal 24h verification artifact is not a complete PASS'

jq -e '
  .chainId == 1143 and (.transactions | length) == 17 and
  .transactions[0].nonce == 17 and
  [.transactions[].nonce] == [range(17;34)] and
  [.transactions[].intendedIngress] ==
    [range(0;17) | if (. % 2) == 0 then "rust" else "gov" end]
' "$artifact" >/dev/null || \
  fail 'transaction artifact is not the exact alternating 9-Rust/8-Go sequence'

if test "$preflight_only" = 1; then
  current_stage='transaction_preflight'
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

current_stage='execute_transaction_burst'
if test "$resume_existing_burst" = 1; then
  transaction_started_at="$(jq -ers '
    map(select(.event == "p4_transaction_finalized"))[0].at // empty
  ' "$burst")"
  test -n "$transaction_started_at" || fail 'resume evidence has no finalized transaction start'
else
  transaction_started_at="$(date -u +%FT%TZ)"
fi
burst_already_passed=false
if test "$resume_existing_burst" = 1 && jq -e -s '
  (map(select(.event == "p4_transaction_burst_pass")) | length) == 1
' "$burst" >/dev/null; then
  # A repair may finish the read-only parity phase before this outer
  # finalizer is relaunched.  Consume that PASS instead of trying to resume a
  # burst which is already complete.
  burst_already_passed=true
else
  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    N42_QUAL_GOV_INGRESS_PORT=28501 N42_QUAL_RUST_INGRESS_PORT=29545 \
    N42_QUAL_BURST_RESUME_EXISTING="$resume_existing_burst" \
    "$qualification" transaction-burst "$artifact" "$burst" >/dev/null || \
    fail 'mixed Go/Rust transaction burst failed'
fi

jq -e -s --slurpfile artifact "$artifact" --arg artifact_sha256 "$artifact_sha256" '
  (map(select(.event == "p4_transaction_finalized"))) as $finalized |
  (map(select(.event == "p4_transaction_burst_pass"))) as $passes |
  ($finalized | length) == ($artifact[0].transactions | length) and
  ($passes | length) == 1 and
  $passes[0].artifactSha256 == $artifact_sha256 and
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

current_stage='monitor_post_transaction_heads'
if ! test "$resume_existing_burst" = 1 || ! test -s "$heads"; then
  env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
    N42_QUAL_RUST_PORT=29546 N42_QUAL_MAX_LAG=6 N42_QUAL_REQUIRE_ZERO_TX=0 \
    "$qualification" monitor-heads 180 5 "$heads" || \
    fail 'post-transaction mixed-client head monitor failed'
fi
post_audit="$(env N42_QUAL_RUNTIME="$runtime" N42_QUAL_PORTS="$ports" \
  "$qualification" audit-soak "$heads" 180 15 6 0)" || \
  fail 'post-transaction mixed-client head audit failed'
printf '%s\n' "$post_audit" | jq -e '
  .status == "PASS" and .elapsedSeconds >= 180 and .blockGrowth > 0 and
  .maximumLag <= 6
' >/dev/null || fail 'post-transaction head audit did not satisfy thresholds'

current_stage='audit_post_transaction_consensus'
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

current_stage='audit_post_transaction_logs'
post_rust0_log_audit="$(env N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_LOG_START="$transaction_started_at" N42_QUAL_RUST_PORT=29545 \
  N42_QUAL_REQUIRE_TIMEOUTS=0 N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=1 \
  "$qualification" audit-runtime-logs "$runtime/logs/rust.log")" || \
  fail 'post-transaction Rust0 log audit failed'
post_rust6_log_audit="$(env N42_QUAL_RUNTIME="$runtime" \
  N42_QUAL_LOG_START="$transaction_started_at" N42_QUAL_RUST_PORT=29546 \
  N42_QUAL_REQUIRE_TIMEOUTS=0 N42_QUAL_REQUIRE_TIMESTAMP_BUMPS=1 \
  "$qualification" audit-runtime-logs "$runtime/logs/rust2.log")" || \
  fail 'post-transaction Rust6 log audit failed'
for audit in "$post_rust0_log_audit" "$post_rust6_log_audit"; do
  printf '%s\n' "$audit" | jq -e '
    .status == "PASS" and .warningPartitionExact and
    .unexpectedWarnings == 0 and .criticalSignals == 0
  ' >/dev/null || fail 'post-transaction Rust log audit is incomplete'
done
# Gov5 has no Rust recovery guard to classify, so retain the direct fatal scan.
for log in "$runtime"/logs/gov{1,2,3,4,5}.log; do
  awk -v start="${transaction_started_at%Z}" '
    {
      prefix=substr($0,1,19)
      timestamped=(substr(prefix,5,1)=="-" && substr(prefix,8,1)=="-" &&
                   substr(prefix,11,1)=="T" && substr(prefix,14,1)==":" &&
                   substr(prefix,17,1)==":")
      in_scope=(!timestamped || prefix >= start)
    }
    in_scope &&
    (index($0," ERROR ") || tolower($0) ~ /(^|[^[:alpha:]])(panic|fatal|equivocat)/) {
      bad=1
    }
    END {exit bad}
  ' "$log" || fail "critical log signal after transaction burst: $log"
done

current_stage='write_total_final_verification'
burst_pass="$(jq -sc 'map(select(.event == "p4_transaction_burst_pass"))[0]' "$burst")"
jq -nc \
  --arg at "$(date -u +%FT%TZ)" \
  --arg transaction_started_at "$transaction_started_at" \
  --arg transaction_artifact "$artifact" \
  --arg transaction_artifact_sha256 "$artifact_sha256" \
  --arg formal "$formal" --arg formal_sha256 "$(shasum -a 256 "$formal" | awk '{print $1}')" \
  --arg burst "$burst" --arg burst_sha256 "$(shasum -a 256 "$burst" | awk '{print $1}')" \
  --arg heads "$heads" --arg heads_sha256 "$(shasum -a 256 "$heads" | awk '{print $1}')" \
  --argjson formal_verification "$(<"$formal")" \
  --argjson burst_pass "$burst_pass" --argjson post_audit "$post_audit" \
  --argjson post_consensus "$post_consensus" \
  --argjson post_equivocations "$post_equivocations" \
  --argjson post_rust0_log_audit "$post_rust0_log_audit" \
  --argjson post_rust6_log_audit "$post_rust6_log_audit" \
  --argjson burst_already_passed "$burst_already_passed" '
  {at:$at,event:"gov5_seven_validator_total_final_verification",status:"PASS",
   transactionStartedAt:$transaction_started_at,
   topology:{gov5:5,rust:2,validators:7},
   formal24h:{artifact:$formal,sha256:$formal_sha256,verification:$formal_verification},
   onChainExecution:{artifact:$burst,sha256:$burst_sha256,pass:$burst_pass,
     transactionArtifact:$transaction_artifact,
     transactionArtifactSha256:$transaction_artifact_sha256,
     signedTransactions:17,alternatingGoRustIngress:true,allSevenEndpointsExact:true,
     receiptsLogsStateAndStorageExact:true,
     resumedAfterAuditFailure:($burst_pass.resumedFromFinalizedTransactionsOnly == true),
     noTransactionsResentDuringResume:($burst_pass.noTransactionsResentDuringResume == true),
     resumedFromExistingPass:$burst_already_passed},
   postTransactionConvergence:{artifact:$heads,sha256:$heads_sha256,audit:$post_audit},
   postTransactionConsensus:{rust:$post_consensus,equivocations:$post_equivocations,
     validatorCount:7,committedQc:true,equivocationCount:0},
   postTransactionLogs:{rust0:$post_rust0_log_audit,rust6:$post_rust6_log_audit,
     unexpectedWarnings:0,criticalSignals:0},
   criticalSignalsAfterTransactions:0}' >"$output.pending"
mv "$output.pending" "$output"
cat "$output"
