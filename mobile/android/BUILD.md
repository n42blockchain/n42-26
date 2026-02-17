# N42 Verifier Android Build Guide

## Prerequisites

1. **Android SDK & NDK**:
   ```bash
   # Install via Android Studio or command-line tools
   sdkmanager "ndk;27.0.12077973"
   export ANDROID_NDK_HOME=$HOME/Library/Android/sdk/ndk/27.0.12077973
   ```

2. **Rust Android targets**:
   ```bash
   rustup target add aarch64-linux-android
   ```

3. **cargo-ndk** (optional, simplifies cross-compilation):
   ```bash
   cargo install cargo-ndk
   ```

## Build the shared library

### Option A: With cargo-ndk (recommended)

```bash
# From project root
cargo ndk -t arm64-v8a -o mobile/android/app/src/main/jniLibs build --release -p n42-mobile-ffi
```

### Option B: Direct cross-compilation

```bash
# Set up the NDK toolchain
export CC_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android26-clang
export AR_aarch64_linux_android=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64/bin/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android

cargo build --release --target aarch64-linux-android -p n42-mobile-ffi

# Copy the .so file
cp target/aarch64-linux-android/release/libn42_mobile_ffi.so \
   mobile/android/app/src/main/jniLibs/arm64-v8a/
```

## Build the Android app

```bash
cd mobile/android
./gradlew assembleRelease
```

The APK will be at `app/build/outputs/apk/release/app-release.apk`.

## Architecture

The JNI bridge is implemented in `crates/n42-mobile-ffi/src/android.rs`:
- Compiled only on `target_os = "android"` via `#[cfg(target_os = "android")]`
- Maps Kotlin `N42Verifier` native methods to the C FFI API
- Uses the `jni` crate (v0.21) for Java ↔ Rust interop

## Notes

- Only ARM64 (arm64-v8a) is supported; most modern Android devices use ARM64
- Minimum Android SDK: 26 (Android 8.0)
- The `.so` file will be ~30-40MB (release build)
- `BLST_PORTABLE=1` may be needed if blst assembly causes issues on Android
