#!/usr/bin/env bash
set -euo pipefail

input="${1:?usage: $0 RESOURCE_JSONL OUTPUT MIN_ELAPSED_SECONDS TARGET_SECONDS [RSS_LIMIT_KIB]}"
output="${2:?output evidence path is required}"
minimum_elapsed="${3:?minimum elapsed seconds is required}"
target_seconds="${4:?target duration seconds is required}"
rss_limit_kib="${5:-1048576}"
fd_final_growth_limit=4

test -s "$input"
test ! -e "$output"
[[ "$minimum_elapsed" =~ ^[0-9]+$ ]]
[[ "$target_seconds" =~ ^[0-9]+$ ]]
[[ "$rss_limit_kib" =~ ^[0-9]+$ ]]
test "$minimum_elapsed" -gt 0
test "$target_seconds" -ge "$minimum_elapsed"

jq -e -s '
  length>=3 and ([.[].pid]|unique|length)==1 and
  all(.[];.event=="rust_resource_snapshot" and
    (.at|fromdateiso8601|type)=="number" and (.head|type)=="number" and
    (.rssKiB|type)=="number" and .rssKiB>0 and
    (.threads|type)=="number" and .threads>0 and
    (.fileDescriptors|type)=="number" and .fileDescriptors>0 and
    (.rethDataKiB|type)=="number" and (.consensusDataKiB|type)=="number" and
    (.logBytes|type)=="number" and (.qmdbWalBytes|type)=="number") and
  ([range(1;length) as $i |
    (.[ $i ].at|fromdateiso8601) > (.[ $i-1 ].at|fromdateiso8601) and
    .[$i].head >= .[$i-1].head] | all)
' "$input" >/dev/null

read -r samples first_epoch last_epoch elapsed pid first_rss last_rss min_rss \
  max_rss first_threads last_threads max_threads first_fd last_fd max_fd \
  head_growth reth_growth consensus_growth log_growth wal_growth < <(
  jq -r -s '
    [length,(.[0].at|fromdateiso8601),(.[-1].at|fromdateiso8601),
     ((.[-1].at|fromdateiso8601)-(.[0].at|fromdateiso8601)),.[0].pid,
     .[0].rssKiB,.[-1].rssKiB,(map(.rssKiB)|min),(map(.rssKiB)|max),
     .[0].threads,.[-1].threads,(map(.threads)|max),
     .[0].fileDescriptors,.[-1].fileDescriptors,(map(.fileDescriptors)|max),
     (.[-1].head-.[0].head),(.[-1].rethDataKiB-.[0].rethDataKiB),
     (.[-1].consensusDataKiB-.[0].consensusDataKiB),
     (.[-1].logBytes-.[0].logBytes),(.[-1].qmdbWalBytes-.[0].qmdbWalBytes)] |
    @tsv
  ' "$input"
)

test "$elapsed" -ge "$minimum_elapsed"
test "$head_growth" -gt 0
test "$max_rss" -le "$rss_limit_kib"
test "$max_threads" -le 256
test "$last_threads" -le $((first_threads + 4))
test "$max_fd" -le 256
test "$last_fd" -le $((first_fd + fd_final_growth_limit))
test "$reth_growth" -ge 0
test "$consensus_growth" -ge 0
test "$log_growth" -ge 0
test "$wal_growth" -ge 0

rss_slope_kib_per_second="$(jq -r '[(.at|fromdateiso8601),.rssKiB]|@tsv' \
  "$input" | awk '
  NR==1 {origin=$1}
  {x=$1-origin; y=$2; n++; sx+=x; sy+=y; sxx+=x*x; sxy+=x*y}
  END {
    denominator=n*sxx-sx*sx
    if (n<3 || denominator<=0) exit 1
    printf "%.12f", (n*sxy-sx*sy)/denominator
  }')"

remaining_seconds=$((target_seconds - elapsed))
if test "$remaining_seconds" -lt 0; then
  remaining_seconds=0
fi
projected_rss_kib="$(awk -v last="$last_rss" \
  -v slope="$rss_slope_kib_per_second" -v remaining="$remaining_seconds" '
  BEGIN {
    if (slope < 0) slope=0
    projected=last+slope*remaining
    printf "%d", int(projected+0.999999)
  }')"
test "$projected_rss_kib" -le "$rss_limit_kib"

temporary="$(mktemp "$(dirname "$output")/.resource-trend.XXXXXX")"
jq -nc --arg at "$(date -u +%FT%TZ)" --arg input "$input" \
  --arg input_sha "$(shasum -a 256 "$input" | awk '{print $1}')" \
  --argjson samples "$samples" --argjson pid "$pid" \
  --argjson first_epoch "$first_epoch" --argjson last_epoch "$last_epoch" \
  --argjson elapsed "$elapsed" --argjson minimum_elapsed "$minimum_elapsed" \
  --argjson target_seconds "$target_seconds" --argjson head_growth "$head_growth" \
  --argjson first_rss "$first_rss" --argjson last_rss "$last_rss" \
  --argjson min_rss "$min_rss" --argjson max_rss "$max_rss" \
  --argjson slope "$rss_slope_kib_per_second" \
  --argjson projected_rss "$projected_rss_kib" \
  --argjson rss_limit "$rss_limit_kib" --argjson first_threads "$first_threads" \
  --argjson last_threads "$last_threads" --argjson max_threads "$max_threads" \
  --argjson first_fd "$first_fd" --argjson last_fd "$last_fd" \
  --argjson max_fd "$max_fd" --argjson fd_final_growth_limit "$fd_final_growth_limit" \
  --argjson reth_growth "$reth_growth" \
  --argjson consensus_growth "$consensus_growth" --argjson log_growth "$log_growth" \
  --argjson wal_growth "$wal_growth" '
  {at:$at,event:"rust_resource_trend_audit",status:"PASS",mutationPerformed:false,
   input:$input,inputSha256:$input_sha,samples:$samples,pid:$pid,
   firstEpoch:$first_epoch,lastEpoch:$last_epoch,elapsedSeconds:$elapsed,
   minimumElapsedSeconds:$minimum_elapsed,targetSeconds:$target_seconds,
   headGrowth:$head_growth,rssKiB:{first:$first_rss,last:$last_rss,min:$min_rss,
     max:$max_rss,slopePerSecond:$slope,
     slopeMiBPerHour:($slope*3600/1024),projectedAtTarget:$projected_rss,
     limit:$rss_limit,projectionWithinLimit:($projected_rss<=$rss_limit)},
   threads:{first:$first_threads,last:$last_threads,max:$max_threads,limit:256},
   fileDescriptors:{first:$first_fd,last:$last_fd,max:$max_fd,limit:256,
     finalGrowth:($last_fd-$first_fd),finalGrowthLimit:$fd_final_growth_limit,
     finalWithinGrowthLimit:(($last_fd-$first_fd)<=$fd_final_growth_limit)},
   growth:{rethDataKiB:$reth_growth,consensusDataKiB:$consensus_growth,
     logBytes:$log_growth,qmdbWalBytes:$wal_growth},singleProcess:true,
   timestampsStrictlyIncreasing:true,headsMonotonic:true,
   resourceProjectionWithin24hBudget:true}' >"$temporary"
jq -e '.status=="PASS" and .mutationPerformed==false and .singleProcess and
  .timestampsStrictlyIncreasing and .headsMonotonic and
  .rssKiB.projectionWithinLimit and .threads.max<=.threads.limit and
  .fileDescriptors.max<=.fileDescriptors.limit and
  .fileDescriptors.finalWithinGrowthLimit and
  .resourceProjectionWithin24hBudget' "$temporary" >/dev/null
mv "$temporary" "$output"
cat "$output"
