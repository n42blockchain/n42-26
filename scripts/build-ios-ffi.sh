#!/usr/bin/env bash
# Build script for the N42 mobile verifier iOS static library.
#
# Prerequisites:
#   - Rust toolchain with iOS targets installed:
#       rustup target add aarch64-apple-ios
#       rustup target add aarch64-apple-ios-sim   # Apple Silicon simulator
#       rustup target add x86_64-apple-ios         # Intel simulator (legacy)
#   - Xcode command line tools: xcode-select --install
#   - Run on macOS (cross-compilation from Linux is not supported for iOS).
#
# Usage:
#   ./scripts/build-ios-ffi.sh [--release | --debug]
#
# Outputs (relative to repo root):
#   target/aarch64-apple-ios/release/libn42_mobile_ffi.a       (real device)
#   target/aarch64-apple-ios-sim/release/libn42_mobile_ffi.a   (Apple Silicon sim)
#   target/x86_64-apple-ios/release/libn42_mobile_ffi.a        (Intel sim, optional)
#   target/universal-ios-sim/release/libn42_mobile_ffi.a       (fat lib for sim)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

PROFILE="${1:---release}"
CARGO_PROFILE_ARG=""
TARGET_SUBDIR="debug"
if [[ "$PROFILE" == "--release" ]]; then
    CARGO_PROFILE_ARG="--release"
    TARGET_SUBDIR="release"
fi

cd "$REPO_ROOT"

echo "==> Building N42 mobile FFI for iOS (profile: $TARGET_SUBDIR)"

# ── Real device (arm64) ──
echo "[1/3] Building for aarch64-apple-ios (real device)..."
cargo build $CARGO_PROFILE_ARG --target aarch64-apple-ios -p n42-mobile-ffi
echo "      Output: target/aarch64-apple-ios/$TARGET_SUBDIR/libn42_mobile_ffi.a"

# ── Apple Silicon simulator ──
echo "[2/3] Building for aarch64-apple-ios-sim (Apple Silicon simulator)..."
cargo build $CARGO_PROFILE_ARG --target aarch64-apple-ios-sim -p n42-mobile-ffi
echo "      Output: target/aarch64-apple-ios-sim/$TARGET_SUBDIR/libn42_mobile_ffi.a"

# ── Intel simulator (optional) ──
if rustup target list --installed | grep -q "x86_64-apple-ios"; then
    echo "[3/3] Building for x86_64-apple-ios (Intel simulator)..."
    cargo build $CARGO_PROFILE_ARG --target x86_64-apple-ios -p n42-mobile-ffi
    echo "      Output: target/x86_64-apple-ios/$TARGET_SUBDIR/libn42_mobile_ffi.a"

    # ── Universal simulator fat library (lipo) ──
    FAT_DIR="$REPO_ROOT/target/universal-ios-sim/$TARGET_SUBDIR"
    mkdir -p "$FAT_DIR"
    lipo -create \
        "$REPO_ROOT/target/aarch64-apple-ios-sim/$TARGET_SUBDIR/libn42_mobile_ffi.a" \
        "$REPO_ROOT/target/x86_64-apple-ios/$TARGET_SUBDIR/libn42_mobile_ffi.a" \
        -output "$FAT_DIR/libn42_mobile_ffi.a"
    echo "      Universal sim fat lib: target/universal-ios-sim/$TARGET_SUBDIR/libn42_mobile_ffi.a"
else
    echo "[3/3] Skipping x86_64-apple-ios (target not installed)"
    # Promote the Apple Silicon sim lib as the universal one if no Intel target.
    FAT_DIR="$REPO_ROOT/target/universal-ios-sim/$TARGET_SUBDIR"
    mkdir -p "$FAT_DIR"
    cp "$REPO_ROOT/target/aarch64-apple-ios-sim/$TARGET_SUBDIR/libn42_mobile_ffi.a" \
       "$FAT_DIR/libn42_mobile_ffi.a"
    echo "      Simulator lib (arm64 only): target/universal-ios-sim/$TARGET_SUBDIR/libn42_mobile_ffi.a"
fi

echo ""
echo "==> Build complete.  Integration steps:"
echo ""
echo "1. Add target/aarch64-apple-ios/$TARGET_SUBDIR/libn42_mobile_ffi.a to Xcode"
echo "   (Build Phases → Link Binary With Libraries)"
echo ""
echo "2. Create a bridging header in your Xcode project:"
echo "   File → New → Header File → N42Verifier-Bridging-Header.h"
echo "   Content: copy from crates/n42-mobile-ffi/src/ios.rs (N42_C_HEADER constant)"
echo ""
echo "3. Set Swift Compiler → Objective-C Bridging Header to the header path above."
echo ""
echo "See crates/n42-mobile-ffi/src/ios.rs for full Swift integration guide."
