#!/usr/bin/env bash
set -euo pipefail

runtime="${1:?usage: $0 RUNTIME START_HEIGHT OUTPUT [END_HEIGHT]}"
start="${2:?usage: $0 RUNTIME START_HEIGHT OUTPUT [END_HEIGHT]}"
output="${3:?usage: $0 RUNTIME START_HEIGHT OUTPUT [END_HEIGHT]}"
requested_end="${4:-}"
ports=(28501 28502 28503 28504 28505 29545)
miners=(
  0x81d4c1f92ddb837cb46f82280d9b491b101fa582
  0xaa5f0ebd2c0b4a7c35aa9e7f0de765f7c0fffa51
  0xa5e99142c567fe398b483726927571b1040aadfd
  0x9464b8be1aa0e960ad4839298522eae0d5bbe71d
  0xa1de4e1c742e47bf805adf07538123b0ddda8dc5
  0xb9ef2bad950b795ed889de3aa0208365550cc86a
)

test -d "$runtime"
test ! -e "$output"
raw_dir="${output%.json}-raw"
test ! -e "$raw_dir"
[[ "$start" =~ ^[0-9]+$ ]]

rpc() {
  local port="$1" method="$2" params="$3"
  curl -fsS --max-time 10 -H 'content-type: application/json' \
    --data "$(jq -nc --arg method "$method" --argjson params "$params" \
      '{jsonrpc:"2.0",id:1,method:$method,params:$params}')" \
    "http://127.0.0.1:$port"
}

common_head=-1
for port in "${ports[@]}"; do
  head_hex="$(rpc "$port" eth_blockNumber '[]' | jq -er '.result')"
  head=$((head_hex))
  if test "$common_head" -lt 0 || test "$head" -lt "$common_head"; then
    common_head="$head"
  fi
done
test "$common_head" -ge "$start"

if test -n "$requested_end"; then
  [[ "$requested_end" =~ ^[0-9]+$ ]]
  end="$requested_end"
else
  complete_cycles=$(((common_head - start + 1) / 6))
  test "$complete_cycles" -gt 0
  end=$((start + complete_cycles * 6 - 1))
fi
test "$end" -le "$common_head"
blocks=$((end - start + 1))
test "$blocks" -gt 0
test $((blocks % 6)) -eq 0
cycles=$((blocks / 6))

audit_dir="$(mktemp -d "$(dirname "$output")/.six-producer-raw.XXXXXX")"
trap 'rm -rf "$audit_dir"' EXIT
endpoint_rows="$audit_dir/endpoints.jsonl"

for port in "${ports[@]}"; do
  rows="$audit_dir/port-$port.jsonl"
  chunk_start="$start"
  while test "$chunk_start" -le "$end"; do
    chunk_end=$((chunk_start + 249))
    test "$chunk_end" -le "$end" || chunk_end="$end"
    request="$(for height in $(seq "$chunk_start" "$chunk_end"); do
      jq -nc --argjson id "$height" --arg block "$(printf '0x%x' "$height")" \
        '{jsonrpc:"2.0",id:$id,method:"eth_getBlockByNumber",params:[$block,false]}'
    done | jq -s '.')"
    curl -fsS --max-time 30 -H 'content-type: application/json' \
      --data "$request" "http://127.0.0.1:$port" | jq -ec '
      sort_by(.id)[] | select(.error==null and .result!=null) |
      {height:.id,number:.result.number,hash:.result.hash,
       parentHash:.result.parentHash,stateRoot:.result.stateRoot,
       receiptsRoot:.result.receiptsRoot,transactionsRoot:.result.transactionsRoot,
       miner:(.result.miner|ascii_downcase),txCount:(.result.transactions|length)}
    ' >>"$rows"
    chunk_start=$((chunk_end + 1))
  done
  test "$(wc -l <"$rows" | tr -d ' ')" -eq "$blocks"
  jq -nc --argjson port "$port" \
    --arg raw_file "$raw_dir/port-$port.jsonl" \
    --arg sha "$(shasum -a 256 "$rows" | awk '{print $1}')" \
    '{port:$port,rawFile:$raw_file,sequenceSha256:$sha}' >>"$endpoint_rows"
done

reference="$audit_dir/port-${ports[0]}.jsonl"
reference_sha="$(shasum -a 256 "$reference" | awk '{print $1}')"
test "$(jq -r '.sequenceSha256' "$endpoint_rows" | sort -u | wc -l | tr -d ' ')" -eq 1
expected_miners="$(printf '%s\n' "${miners[@]}" | jq -R . | jq -s '.')"
jq -e -s --argjson start "$start" --argjson blocks "$blocks" \
  --argjson cycles "$cycles" --argjson miners "$expected_miners" '
  length==$blocks and
  ([range(0;length) as $i | .[$i].height==($start+$i) and
    .[$i].txCount==0 and .[$i].miner==$miners[$i % 6]] | all) and
  ([range(1;length) as $i | .[$i].parentHash==.[$i-1].hash] | all) and
  ([$miners[] as $miner |
    (map(select(.miner==$miner))|length)==$cycles] | all)
' "$reference" >/dev/null

temporary="$(mktemp "$(dirname "$output")/.six-producer-range.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --argjson start "$start" \
  --argjson end "$end" --argjson common "$common_head" --argjson blocks "$blocks" \
  --argjson cycles "$cycles" --arg sequence_sha "$reference_sha" \
  --argjson miners "$expected_miners" --slurpfile endpoints "$endpoint_rows" \
  --slurpfile rows "$reference" '
  (reduce $miners[] as $miner ({};
    .[$miner]=([$rows[]|select(.miner==$miner)]|length))) as $counts |
  {at:$at,event:"gov5_six_producer_full_range_audit",status:"PASS",
   mutationPerformed:false,startHeight:$start,endHeight:$end,
   commonHeadAtAudit:$common,blocksScanned:$blocks,completeCycles:$cycles,
   leaderStride:6,expectedSlotMiners:$miners,producerCounts:$counts,
   endpointSequences:$endpoints,sequenceSha256:$sequence_sha,
   allSixEndpointSequencesExact:true,parentChainContinuous:true,
   expectedProducerSlotsExact:true,allProducerCountsBalanced:true,
   zeroTransactions:true}' >"$temporary"
jq -e '
  .status=="PASS" and .blocksScanned==(.completeCycles*6) and
  .allSixEndpointSequencesExact and .parentChainContinuous and
  .expectedProducerSlotsExact and .allProducerCountsBalanced and
  .zeroTransactions and (.mutationPerformed|not)
' "$temporary" >/dev/null
mv "$audit_dir" "$raw_dir"
trap - EXIT
mv "$temporary" "$output"
cat "$output"
