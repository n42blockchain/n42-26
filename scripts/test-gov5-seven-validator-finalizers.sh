#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
finalizer="$script_dir/gov5-seven-validator-finalize-after-monitors.sh"
verifier="$script_dir/gov5-seven-validator-final-verifier.sh"
post="$script_dir/gov5-seven-validator-post-soak-finalizer.sh"
milestones="$script_dir/gov5-seven-validator-milestone-monitor.sh"
qualification="$script_dir/gov5-interop-qualification.sh"
readonly="$script_dir/audit-gov5-burst-readonly.sh"

for script in "$finalizer" "$verifier" "$post" "$milestones"; do
  bash -n "$script"
done

# The formal leader proof must span the complete soak and bind each Rust
# validator to its own commit log.  Evidence is written atomically and the
# verifier rejects concatenated/replayed JSON objects.
rg -F 'first_height="$(jq -sr '\''.[0].commonHeight'\'' "$heads")"' "$finalizer" >/dev/null
rg -F 'last_height="$(jq -sr '\''.[-1].commonHeight'\'' "$heads")"' "$finalizer" >/dev/null
rg -F 'rust0_end="$last_height"' "$finalizer" >/dev/null
rg -F 'rust6_end="$last_height"' "$finalizer" >/dev/null
rg -F 'N42_QUAL_RUST_LOG="$runtime/logs/rust.log"' "$finalizer" >/dev/null
rg -F 'N42_QUAL_RUST_LOG="$runtime/logs/rust2.log"' "$finalizer" >/dev/null
rg -F '"$rust0_start" "$rust0_end" "$rust0_leaders.pending"' "$finalizer" >/dev/null
rg -F 'mv "$rust0_leaders.pending" "$rust0_leaders"' "$finalizer" >/dev/null
rg -F '"$rust6_start" "$rust6_end" "$rust6_leaders.pending"' "$finalizer" >/dev/null
rg -F 'mv "$rust6_leaders.pending" "$rust6_leaders"' "$finalizer" >/dev/null
rg -F 'jq -s -e --argjson first_height "$first_height"' "$verifier" >/dev/null
rg -F 'length == 1 and .[0] as $audit' "$verifier" >/dev/null
rg -F '$audit.endHeight == $last_height' "$verifier" >/dev/null
rg -F '$audit.leaderCommitLog.allVotesFivePlusFive' "$verifier" >/dev/null

# The readonly EVM audit and the later mutating burst must consume the same
# immutable signed transaction artifact.
rg -F 'N42_FINAL_TRANSACTION_ARTIFACT="$transaction_artifact"' "$finalizer" >/dev/null
rg -F '.artifactSha256 == $artifact_sha256' "$verifier" >/dev/null
rg -F 'artifact_sha256="$(shasum -a 256 "$artifact"' "$post" >/dev/null
rg -F '.executionAudit.artifactSha256 == $artifact_sha256' "$post" >/dev/null
rg -F '$passes[0].artifactSha256 == $artifact_sha256' "$post" >/dev/null

# The post-soak mutation is exact, alternating, and followed by a fresh
# consensus/equivocation audit instead of relying only on receipt success.
rg -F '[.transactions[].nonce] == [range(17;34)]' "$post" >/dev/null
rg -F '[range(0;17) | if (. % 2) == 0 then "rust" else "gov" end]' "$post" >/dev/null
rg -F 'current_stage='\''audit_post_transaction_consensus'\''' "$post" >/dev/null
rg -F 'n42_consensusStatus' "$post" >/dev/null
rg -F 'n42_equivocations' "$post" >/dev/null
rg -F 'postTransactionConsensus:' "$post" >/dev/null

# A completed burst can be resumed without resending transactions when a
# later cross-client RPC audit fails.  Block comparisons deliberately ignore
# only the known Gov5/Reth presentation-only fields while retaining exact
# canonical, transaction, receipt, log, state, and storage comparisons.
rg -F 'N42_POST_RESUME_EXISTING_BURST' "$post" >/dev/null
rg -F 'N42_QUAL_BURST_RESUME_EXISTING="$resume_existing_burst"' "$post" >/dev/null
rg -F 'burst_already_passed=true' "$post" >/dev/null
rg -F 'noTransactionsResentDuringResume' "$post" >/dev/null
rg -F '! test -s "$heads"' "$post" >/dev/null
rg -F 'in_scope=(!timestamped || prefix >= start)' "$post" >/dev/null
! rg -F '! awk -v start=' "$post" >/dev/null
rg -F 'del(.rewards,.verifier,.totalDifficulty,.sha3Uncles,.size)' "$qualification" >/dev/null
rg -F 'blockRpcComparison:"canonical-and-execution-fields"' "$qualification" >/dev/null
rg -F 'storageRpcComparison:"numeric-word-value"' "$qualification" >/dev/null
rg -F 'ports=(28501 28502 28503 28504 28505 29545 29546)' "$readonly" >/dev/null
rg -F 'allSevenEndpointsExact:(($rpc|length)==7)' "$readonly" >/dev/null

# All unattended controllers must fail close with atomic diagnostic evidence.
for script in "$finalizer" "$post" "$milestones"; do
  rg -F 'record_unexpected_failure' "$script" >/dev/null ||
    rg -F 'record_failure' "$script" >/dev/null
  rg -F '.pending' "$script" >/dev/null
done

jq -nc '{event:"gov5_seven_validator_finalizer_config_test",status:"PASS",
  fullSoakRustLeaderCoverage:true,rustLeaderLogsBound:true,
  atomicLeaderEvidence:true,singleLeaderArtifactRequired:true,
  transactionArtifactShaBoundEndToEnd:true,exactAlternatingMixedIngress:true,
  postTransactionConsensusRechecked:true,transactionBurstResumeWithoutResend:true,
  blockRpcPresentationNormalized:true,storageRpcWidthNormalized:true,
  readonlyEvmCoversSevenEndpoints:true,unattendedFailuresPersisted:true}'
