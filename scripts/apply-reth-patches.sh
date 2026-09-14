#!/usr/bin/env bash
# Apply the N42 cache API required on top of the reth CI baseline.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
reth_dir=${1:-"$script_dir/../../reth"}
patch_file="$script_dir/../patches/reth-payload-transactions-root.patch"

if git -C "$reth_dir" apply --reverse --check "$patch_file" 2>/dev/null; then
    echo "Reth payload transaction-root cache patch already applied"
elif git -C "$reth_dir" apply --check "$patch_file"; then
    git -C "$reth_dir" apply "$patch_file"
    echo "Applied reth payload transaction-root cache patch"
else
    echo "Reth patch does not match this checkout; use the baseline documented in README.md." >&2
    exit 1
fi
