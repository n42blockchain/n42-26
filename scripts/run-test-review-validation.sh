#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MODE="${1:-validate}"

if [[ "$MODE" != "validate" && "$MODE" != "coverage" ]]; then
    echo "usage: $0 [validate|coverage]" >&2
    exit 2
fi

cd "$REPO_ROOT"

if [[ "$MODE" == "coverage" ]]; then
    TARGET_DIR="${TARGET_DIR:-$REPO_ROOT/target/test-review-coverage}"
else
    TARGET_DIR="${TARGET_DIR:-$REPO_ROOT/target/test-review}"
fi
CARGO_TARGET_DIR="$TARGET_DIR"
export CARGO_TARGET_DIR
export CARGO_INCREMENTAL=0

COMMON_CRATES=(
  "n42-chainspec"
  "n42-jmt"
  "n42-mobile-ffi"
)

echo "==> test-review validation mode: $MODE"
echo "==> target dir: $CARGO_TARGET_DIR"

run_validate() {
    echo "==> running targeted cargo test passes"
    for crate in "${COMMON_CRATES[@]}"; do
        echo "-- cargo test -p $crate"
        cargo test -p "$crate"
    done

    echo "-- cargo test --manifest-path crates/n42-zkproof-guest/Cargo.toml"
    cargo test --manifest-path crates/n42-zkproof-guest/Cargo.toml

    echo "-- cargo test -p e2e-test --bin e2e-test rpc_client::tests::test_get_transaction_receipt_none_for_null_result -- --exact"
    cargo test -p e2e-test --bin e2e-test rpc_client::tests::test_get_transaction_receipt_none_for_null_result -- --exact

    echo "-- cargo test -p e2e-test --bin e2e-test rpc_client::tests::test_wait_for_receipt_retries_until_result_available -- --exact"
    cargo test -p e2e-test --bin e2e-test rpc_client::tests::test_wait_for_receipt_retries_until_result_available -- --exact
}

run_coverage() {
    command -v xcrun >/dev/null 2>&1 || {
        echo "xcrun is required for coverage mode" >&2
        exit 1
    }

    local out_dir="$REPO_ROOT/.artifacts/test-review-coverage"
    local profraw_dir="$out_dir/profraw"
    local profdata="$out_dir/coverage.profdata"
    mkdir -p "$profraw_dir"
    rm -f "$profraw_dir"/*.profraw "$profdata"
    rm -rf "$CARGO_TARGET_DIR"

    export RUSTFLAGS="${RUSTFLAGS:-} -Cinstrument-coverage -Ccodegen-units=1"
    export LLVM_PROFILE_FILE="$profraw_dir/%p-%m.profraw"

    run_validate

    echo "==> merging coverage profiles"
    xcrun llvm-profdata merge -sparse "$profraw_dir"/*.profraw -o "$profdata"

    local -a objects=()
    local stem
    for stem in n42_chainspec n42_jmt n42_mobile_ffi n42_zkproof_guest e2e_test; do
        while IFS= read -r file; do
            objects+=("$file")
        done < <(find "$CARGO_TARGET_DIR/debug/deps" -maxdepth 1 -type f -perm -111 -name "${stem}-*" 2>/dev/null | sort)
    done

    if [[ ${#objects[@]} -eq 0 ]]; then
        echo "no coverage objects found under $CARGO_TARGET_DIR/debug/deps" >&2
        exit 1
    fi

    echo "==> writing coverage summary"
    xcrun llvm-cov report \
        "${objects[@]}" \
        --instr-profile "$profdata" \
        > "$out_dir/coverage-summary.txt"

    echo "==> writing coverage export"
    xcrun llvm-cov export \
        "${objects[@]}" \
        --instr-profile "$profdata" \
        > "$out_dir/coverage.json"

    echo "coverage artifacts:"
    echo "  $out_dir/coverage-summary.txt"
    echo "  $out_dir/coverage.json"
}

case "$MODE" in
    validate)
        run_validate
        ;;
    coverage)
        run_coverage
        ;;
esac
