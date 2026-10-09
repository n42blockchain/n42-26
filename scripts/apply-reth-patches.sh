#!/usr/bin/env bash
# Apply the N42 APIs required on top of the pinned Reth CI baseline.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
reth_dir=${1:-"$script_dir/../../reth"}
patch_dir=${N42_RETH_PATCH_DIR:-"$script_dir/../patches"}
base_revision=${N42_RETH_BASE_REVISION:-3d592ece6de8c4559987416a544fc215fd6d6921}
adapted_revision=${N42_RETH_ADAPTED_REVISION:-3452d7d0217095446b0f2632a283e314951da273}
current_revision=$(git -C "$reth_dir" rev-parse HEAD 2>/dev/null || true)
if [[ "$current_revision" != "$base_revision" && "$current_revision" != "$adapted_revision" ]]; then
    echo "Reth checkout must be exactly pinned base $base_revision or adapted revision $adapted_revision." >&2
    exit 1
fi

# Keep dependency order: the performance patch creates payload_cache.rs, which
# the transaction-root patch extends. Preflight applies pending patches to a
# temporary index so related patches are validated as one sequence before the
# working tree changes.
names=(
    reth-n42-perf
    reth-payload-transactions-root
    reth-qmdb-state-reader
    reth-import-batch
    reth-gov5-cached-state-root
    reth-engine-state-provider-factory
)
pending=()
for name in "${names[@]}"; do
    patch_file="$patch_dir/$name.patch"
    if [[ ! -f "$patch_file" ]]; then
        echo "Required Reth patch is missing: $patch_file" >&2
        exit 1
    fi
    if git -C "$reth_dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "Reth $name patch already applied"
    else
        pending+=("$patch_file")
    fi
done

if [[ "$current_revision" == "$adapted_revision" ]]; then
    for name in "${names[@]}"; do
        if ! git -C "$reth_dir" apply --reverse --check "$patch_dir/$name.patch" 2>/dev/null; then
            echo "Pinned adapted Reth revision is missing or diverges from patch $name." >&2
            exit 1
        fi
    done
    echo "Reth is already at the pinned adapted revision $adapted_revision"
    exit 0
fi

if ((${#pending[@]})); then
    temp_index=$(mktemp "${TMPDIR:-/tmp}/n42-reth-index.XXXXXX")
    trap 'rm -f "$temp_index"' EXIT
    GIT_INDEX_FILE="$temp_index" git -C "$reth_dir" read-tree HEAD
    index_paths=()
    declare -A seen_paths=()
    for name in "${names[@]}"; do
        patch_file="$patch_dir/$name.patch"
        while IFS=$'\t' read -r _ _ path; do
            [[ -n "${path:-}" ]] || continue
            if [[ -e "$reth_dir/$path" || -L "$reth_dir/$path" ]] ||
               git -C "$reth_dir" ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
                if [[ -z "${seen_paths[$path]+x}" ]]; then
                    index_paths+=("$path")
                    seen_paths[$path]=1
                fi
            fi
        done < <(git -C "$reth_dir" apply --numstat "$patch_file")
    done
    if ((${#index_paths[@]})); then
        GIT_INDEX_FILE="$temp_index" git -C "$reth_dir" add -A -- "${index_paths[@]}"
    fi
    for patch_file in "${pending[@]}"; do
        GIT_INDEX_FILE="$temp_index" git -C "$reth_dir" apply --cached "$patch_file"
    done
    # The ordered series has passed against the same current tracked files.
    git -C "$reth_dir" apply "${pending[@]}"
fi
