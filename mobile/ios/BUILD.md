# N42 Verifier iOS Build Guide

## Prerequisites

- Xcode 15+ with iOS SDK
- Rust toolchain with `aarch64-apple-ios` target:
  ```bash
  rustup target add aarch64-apple-ios
  ```

## Build the static library

```bash
# From project root
cd ../../

# Temporarily set crate-type to staticlib only (cdylib won't link on iOS due to blst assembly)
# Option 1: Edit Cargo.toml
# crate-type = ["staticlib"]

# Build with BLST_PORTABLE to avoid __chkstk_darwin symbol issue
BLST_PORTABLE=1 cargo build --release --target aarch64-apple-ios -p n42-mobile-ffi

# Copy the static library
cp target/aarch64-apple-ios/release/libn42_mobile_ffi.a mobile/ios/N42Verifier/
```

## Build the iOS app

```bash
# Open in Xcode
open mobile/ios/N42Verifier.xcodeproj

# Or build from command line
xcodebuild -project mobile/ios/N42Verifier.xcodeproj \
  -scheme N42Verifier \
  -destination 'generic/platform=iOS' \
  -configuration Release
```

## Notes

- The static library is ~40MB (release, ARM64)
- Minimum iOS deployment target: 16.0
- The bridging header references the C header at `crates/n42-mobile-ffi/include/n42_mobile.h`
- `BLST_PORTABLE=1` is required because blst's aarch64 assembly uses `___chkstk_darwin` (macOS only)
- For iOS simulator builds, use `aarch64-apple-ios-sim` target instead
