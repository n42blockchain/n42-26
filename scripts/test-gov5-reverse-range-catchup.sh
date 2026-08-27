#!/usr/bin/env bash
set -euo pipefail

# Real reverse-direction interoperability gate:
#
#   1. a Rust/N42-26 canonical chain is already at least 3072 blocks ahead;
#   2. start one stopped Gov5 member from the declared old snapshot;
#   3. require at least three completed 1024-block range RPCs;
#   4. compare the exact target block's hash and state root on both clients.
#
# The launcher must run Gov5 in the foreground and use a datadir whose canonical
# height is N42_RANGE_GOV_START_HEIGHT. It is intentionally supplied by the
# deployment because validator keys, genesis, ports and datadirs are
# environment-specific and must never be synthesized or overwritten here.
# The running Rust node must already authorize the late Gov5 Noise PeerId via
# its committee binding or N42_TRUSTED_PEERS; matching a public genesis alone
# intentionally does not grant access to canonical history.

: "${N42_RANGE_RUST_RPC:?set the running Rust JSON-RPC URL}"
: "${N42_RANGE_RUST_METRICS:?set the running Rust Prometheus metrics URL}"
: "${N42_RANGE_GOV_RPC:?set the late Gov5 member JSON-RPC URL}"
: "${N42_RANGE_GOV_START:?set an executable foreground Gov5 launcher}"
: "${N42_RANGE_GOV_START_HEIGHT:?set the canonical height in the Gov5 snapshot}"

minimum_gap="${N42_RANGE_MIN_GAP:-3072}"
timeout_seconds="${N42_RANGE_TIMEOUT_SECONDS:-900}"
poll_seconds="${N42_RANGE_POLL_SECONDS:-2}"
artifact_dir="${N42_RANGE_ARTIFACT_DIR:-$(pwd)/artifacts/gov5-reverse-range-catchup}"
keep_gov5="${N42_RANGE_KEEP_GOV5:-0}"
gov_pid=""

fail() {
    echo "[gov5-reverse-range] ERROR: $*" >&2
    exit 1
}

cleanup() {
    if [[ "$keep_gov5" != "1" && -n "$gov_pid" ]] && kill -0 "$gov_pid" 2>/dev/null; then
        kill "$gov_pid" 2>/dev/null || true
        wait "$gov_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT

for command in curl jq awk date; do
    command -v "$command" >/dev/null || fail "missing command: $command"
done
[[ -x "$N42_RANGE_GOV_START" ]] || fail "Gov5 launcher is not executable: $N42_RANGE_GOV_START"
[[ "$N42_RANGE_GOV_START_HEIGHT" =~ ^[0-9]+$ ]] || fail "start height must be an integer"
[[ "$minimum_gap" =~ ^[0-9]+$ ]] || fail "minimum gap must be an integer"
(( minimum_gap >= 3072 )) || fail "minimum gap must be at least 3072 blocks"
mkdir -p "$artifact_dir"

rpc() {
    local endpoint="$1"
    local method="$2"
    local params="$3"
    curl --fail --silent --show-error --max-time 10 \
        --header 'content-type: application/json' \
        --data "$(jq -cn --arg method "$method" --argjson params "$params" \
            '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
        "$endpoint"
}

block_number() {
    local endpoint="$1"
    local encoded
    encoded="$(rpc "$endpoint" eth_blockNumber '[]' | jq -er '.result')"
    printf '%d\n' "$((encoded))"
}

block_identity() {
    local endpoint="$1"
    local height="$2"
    rpc "$endpoint" eth_getBlockByNumber "[\"$(printf '0x%x' "$height")\",false]" |
        jq -ec '.result | select(. != null) | {number,hash,stateRoot}'
}

range_metric() {
    local metric="$1"
    curl --fail --silent --show-error --max-time 10 "$N42_RANGE_RUST_METRICS" |
        awk -v metric="$metric" '$1 == metric { total += $2 } END { printf "%.0f\n", total + 0 }'
}

rust_head="$(block_number "$N42_RANGE_RUST_RPC")"
gap=$((rust_head - N42_RANGE_GOV_START_HEIGHT))
(( gap >= minimum_gap )) ||
    fail "Rust head $rust_head is only $gap blocks ahead of snapshot $N42_RANGE_GOV_START_HEIGHT"

target_height="$rust_head"
target_rust="$(block_identity "$N42_RANGE_RUST_RPC" "$target_height")"
accepted_before="$(range_metric n42_gov5_range_requests_accepted_total)"
completed_before="$(range_metric n42_gov5_range_responses_completed_total)"
expected_batches=$(((gap + 1023) / 1024))
(( expected_batches >= 3 )) || fail "test must cross at least three 1024-block batches"

jq -n \
    --argjson snapshotHeight "$N42_RANGE_GOV_START_HEIGHT" \
    --argjson targetHeight "$target_height" \
    --argjson gap "$gap" \
    --argjson expectedBatches "$expected_batches" \
    --argjson target "$target_rust" \
    '{snapshotHeight:$snapshotHeight,targetHeight:$targetHeight,gap:$gap,
      expectedBatches:$expectedBatches,rustTarget:$target}' \
    >"$artifact_dir/preflight.json"

echo "[gov5-reverse-range] Rust target=$target_height snapshot=$N42_RANGE_GOV_START_HEIGHT gap=$gap"
"$N42_RANGE_GOV_START" >"$artifact_dir/gov5.log" 2>&1 &
gov_pid="$!"
echo "$gov_pid" >"$artifact_dir/gov5.pid"

deadline=$(( $(date +%s) + timeout_seconds ))
gov_head=-1
while (( $(date +%s) < deadline )); do
    if ! kill -0 "$gov_pid" 2>/dev/null; then
        wait "$gov_pid" || true
        fail "Gov5 exited before reaching the target; see $artifact_dir/gov5.log"
    fi
    if gov_head="$(block_number "$N42_RANGE_GOV_RPC" 2>/dev/null)"; then
        echo "[gov5-reverse-range] Gov5 head=$gov_head target=$target_height"
        if (( gov_head >= target_height )); then
            break
        fi
    fi
    sleep "$poll_seconds"
done
(( gov_head >= target_height )) || fail "Gov5 did not reach $target_height within ${timeout_seconds}s"

target_gov="$(block_identity "$N42_RANGE_GOV_RPC" "$target_height")"
rust_hash="$(jq -r '.hash' <<<"$target_rust")"
gov_hash="$(jq -r '.hash' <<<"$target_gov")"
rust_root="$(jq -r '.stateRoot' <<<"$target_rust")"
gov_root="$(jq -r '.stateRoot' <<<"$target_gov")"
[[ "$gov_hash" == "$rust_hash" ]] || fail "target hash mismatch: Rust=$rust_hash Gov5=$gov_hash"
[[ "$gov_root" == "$rust_root" ]] || fail "target state root mismatch: Rust=$rust_root Gov5=$gov_root"

accepted_after="$(range_metric n42_gov5_range_requests_accepted_total)"
completed_after="$(range_metric n42_gov5_range_responses_completed_total)"
accepted_delta=$((accepted_after - accepted_before))
completed_delta=$((completed_after - completed_before))
(( completed_delta >= expected_batches )) ||
    fail "only $completed_delta range responses completed ($accepted_delta accepted); expected at least $expected_batches; verify the Gov5 Noise PeerId is trusted by Rust"

jq -n \
    --argjson snapshotHeight "$N42_RANGE_GOV_START_HEIGHT" \
    --argjson targetHeight "$target_height" \
    --argjson finalGovHeight "$gov_head" \
    --argjson gap "$gap" \
    --argjson expectedBatches "$expected_batches" \
    --argjson acceptedRangeRequests "$accepted_delta" \
    --argjson completedRangeResponses "$completed_delta" \
    --argjson rustTarget "$target_rust" \
    --argjson govTarget "$target_gov" \
    '{result:"PASS",snapshotHeight:$snapshotHeight,targetHeight:$targetHeight,
      finalGovHeight:$finalGovHeight,gap:$gap,expectedBatches:$expectedBatches,
      acceptedRangeRequests:$acceptedRangeRequests,completedRangeResponses:$completedRangeResponses,
      rustTarget:$rustTarget,govTarget:$govTarget}' \
    >"$artifact_dir/result.json"

echo "[gov5-reverse-range] PASS: $completed_delta completed range batches; hash/root match at $target_height"
echo "[gov5-reverse-range] evidence: $artifact_dir/result.json"
