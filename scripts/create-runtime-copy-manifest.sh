#!/usr/bin/env bash
set -euo pipefail

source_runtime="${1:?source runtime required}"
target_runtime="${2:?target runtime required}"
event="${3:?event name required}"

for runtime in "$source_runtime" "$target_runtime"; do
  test -d "$runtime/gov"
  test -d "$runtime/rust"
done

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

make_manifest() {
  local runtime="$1"
  local output="$2"
  (
    cd "$runtime"
    find gov rust -type f ! -name n42.pid -print0 |
      LC_ALL=C sort -z |
      while IFS= read -r -d '' path; do
        size="$(stat -f '%z' "$path")"
        hash="$(shasum -a 256 "$path" | awk '{print $1}')"
        jq -nc --arg path "$path" --argjson size "$size" --arg sha256 "$hash" \
          '{path:$path,size:$size,sha256:$sha256}'
      done
  ) >"$output"
}

make_manifest "$source_runtime" "$work_dir/source.jsonl"
make_manifest "$target_runtime" "$work_dir/target.jsonl"
cmp -s "$work_dir/source.jsonl" "$work_dir/target.jsonl"

files="$(wc -l <"$work_dir/source.jsonl" | tr -d ' ')"
bytes="$(jq -s 'map(.size) | add // 0' "$work_dir/source.jsonl")"
source_sha="$(shasum -a 256 "$work_dir/source.jsonl" | awk '{print $1}')"
target_sha="$(shasum -a 256 "$work_dir/target.jsonl" | awk '{print $1}')"

jq -n \
  --arg at "$(date -u +%FT%TZ)" \
  --arg event "$event" \
  --arg source "$source_runtime" \
  --arg target "$target_runtime" \
  --argjson files "$files" \
  --argjson bytes "$bytes" \
  --arg source_sha "$source_sha" \
  --arg target_sha "$target_sha" \
  --slurpfile entries "$work_dir/source.jsonl" \
  '{at:$at,event:$event,status:"PASS",source:$source,target:$target,
    files:$files,bytes:$bytes,sourceManifestSha256:$source_sha,
    targetManifestSha256:$target_sha,allPathsSizesAndHashesExact:true,
    entries:$entries}'
