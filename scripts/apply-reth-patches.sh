#!/usr/bin/env bash
# Apply the N42 APIs required on top of the pinned Reth CI baseline.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
reth_dir=${1:-"$script_dir/../../reth"}
patch_dir=${N42_RETH_PATCH_DIR:-"$script_dir/../patches"}
base_revision=${N42_RETH_BASE_REVISION:-d763342fa185e4e90ee79dc65f6a5706168b0bcb}
adapted_revision=${N42_RETH_ADAPTED_REVISION:-7aee67f58fe34b4bd9eef6a5e6caa603d3a25127}
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
    reth-cached-execution-refusal
    reth-native-import-path
    reth-executed-block-inbox
    reth-import-batch-tail
)
# Later patches may edit lines an earlier patch introduced, so a patch cannot
# always be reversed on its own against the final tree. Reverse the whole
# series in opposite order on a temporary index that mirrors the tracked
# working tree, and compare the result with the pinned base tree.
series_reverses_to_base() {
    local index tree base_tree i
    base_tree=$(git -C "$reth_dir" rev-parse "$base_revision^{tree}" 2>/dev/null) || return 1
    index=$(mktemp "${TMPDIR:-/tmp}/n42-reth-check.XXXXXX")
    GIT_INDEX_FILE="$index" git -C "$reth_dir" read-tree HEAD
    GIT_INDEX_FILE="$index" git -C "$reth_dir" add -u
    # Files the series creates are untracked on a patched base checkout.
    local name path
    for name in "${names[@]}"; do
        while IFS=$'\t' read -r _ _ path; do
            if [[ -n "${path:-}" && -e "$reth_dir/$path" ]]; then
                GIT_INDEX_FILE="$index" git -C "$reth_dir" add -- "$path"
            fi
        done < <(git -C "$reth_dir" apply --numstat "$patch_dir/$name.patch")
    done
    for ((i = ${#names[@]} - 1; i >= 0; i--)); do
        if ! GIT_INDEX_FILE="$index" git -C "$reth_dir" apply --cached --reverse \
            "$patch_dir/${names[i]}.patch" 2>/dev/null; then
            rm -f "$index"
            return 1
        fi
    done
    tree=$(GIT_INDEX_FILE="$index" git -C "$reth_dir" write-tree)
    rm -f "$index"
    [[ "$tree" == "$base_tree" ]]
}

for name in "${names[@]}"; do
    if [[ ! -f "$patch_dir/$name.patch" ]]; then
        echo "Required Reth patch is missing: $patch_dir/$name.patch" >&2
        exit 1
    fi
done

if series_reverses_to_base; then
    echo "Reth source already carries the full N42 patch series"
    exit 0
fi
if [[ "$current_revision" == "$adapted_revision" ]]; then
    echo "Pinned adapted Reth revision diverges from the N42 patch series." >&2
    exit 1
fi
if [[ "${N42_RETH_CHECK_ONLY:-0}" == "1" ]]; then
    echo "Reth source does not carry the full N42 patch series." >&2
    exit 1
fi

# Resume a partly patched base checkout: skip the patches that still reverse
# cleanly on their own and apply the rest in order.
pending=()
for name in "${names[@]}"; do
    patch_file="$patch_dir/$name.patch"
    if git -C "$reth_dir" apply --reverse --check "$patch_file" 2>/dev/null; then
        echo "Reth $name patch already applied"
    else
        pending+=("$patch_file")
    fi
done

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
