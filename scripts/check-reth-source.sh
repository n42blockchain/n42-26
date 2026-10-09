#!/usr/bin/env bash
# Require the local Reth source to match the pinned base plus every N42 patch.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/.." && pwd)
reth_dir=${1:-"$repo_dir/../reth"}
patch_dir="$repo_dir/patches"
lock_file="$repo_dir/reth-source.lock"

mapfile -t revisions < <(python3 - "$lock_file" <<'PY'
import json
import sys
from pathlib import Path

lock = json.loads(Path(sys.argv[1]).read_text())
print(lock["upstream_commit"])
print(lock["adapted_commit"])
print(len(lock["patch_series"]))
PY
)
base_revision=${revisions[0]}
adapted_revision=${revisions[1]}
expected_patch_count=${revisions[2]}
head=$(git -C "$reth_dir" rev-parse HEAD 2>/dev/null) || {
    echo "Reth source is not a Git checkout: $reth_dir" >&2
    exit 1
}
if [[ "$head" != "$base_revision" && "$head" != "$adapted_revision" ]]; then
    echo "Reth HEAD $head is not pinned base $base_revision or adapted revision $adapted_revision." >&2
    exit 1
fi

# This directory also holds patches for the sibling n42-rs repository.
# Only the Reth series participates in this source lock.
patch_files=("$patch_dir"/reth-*.patch)
if ((${#patch_files[@]} != expected_patch_count)); then
    echo "Expected $expected_patch_count Reth patches from $lock_file; found ${#patch_files[@]}." >&2
    exit 1
fi
for patch_file in "${patch_files[@]}"; do
    if ! git -C "$reth_dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "Reth source is missing or diverges from $(basename "$patch_file")." >&2
        exit 1
    fi
done

# Reuse the applier's source-revision and patch-series validation after all
# reverse checks have proved this invocation cannot modify the source.
bash "$script_dir/apply-reth-patches.sh" "$reth_dir"
echo "Reth source matches the v2.7.0 N42 patch lock."
