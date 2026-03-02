# Low-End Device Test Procedure

This document describes how to test the N42 mobile verifier on low-end Android and iOS devices before production release.

## Target Specifications

| Metric | Minimum Spec |
|--------|-------------|
| RAM | ≤ 2 GB |
| CPU | ARM Cortex-A55 (or equivalent) |
| OS | Android 10 (API 29) / iOS 14 |
| Network | LTE (10–50 Mbps down) |

## Android Testing

### Prerequisites

- ADB installed: `brew install android-platform-tools`
- A physical device meeting the spec above, connected via USB with USB debugging enabled.
- The N42 verifier APK built from the project's Android module.

### Build the Native Library

```bash
# Add the Android cross-compilation target (once):
rustup target add aarch64-linux-android

# Set NDK path (adjust to your local NDK installation):
export ANDROID_NDK_HOME="$HOME/Library/Android/sdk/ndk/27.0.12077973"

# Build the FFI library for Android arm64:
cargo build --target aarch64-linux-android --release -p n42-mobile-ffi
# Output: target/aarch64-linux-android/release/libn42_mobile_ffi.so
```

### Memory Baseline

Before starting the test, capture the baseline RSS:

```bash
# Find the app's PID (replace com.n42.verifier with your actual package name):
adb shell "ps -e | grep com.n42.verifier"

# Capture memory snapshot:
adb shell dumpsys meminfo com.n42.verifier
```

Key fields to record:
- `TOTAL PSS` (Proportional Set Size) — target: < 100 MB at idle
- `TOTAL RSS` — target: < 150 MB under load

### Connection Latency Test

```bash
# Capture logcat filtered to n42:
adb logcat -s "n42_mobile_ffi:*" &

# Measure time from app launch to first "connected to StarHub" log:
# Target: < 3 seconds
```

### Verification Latency Test

On device, trigger a verification cycle and measure the time in `n42_last_verify_info`:

```swift
// Swift snippet for instrumentation:
let start = Date()
let result = verifier.verifyAndSend(data: packetData)
let elapsed = Date().timeIntervalSince(start) * 1000
print("verify_time_ms: \(elapsed)")
```

Target: `verify_time_ms < 500` on Cortex-A55.

### Performance Targets

| Metric | Target |
|--------|--------|
| QUIC connection time | < 3 s |
| Block verification latency | < 500 ms |
| Idle memory (RSS) | < 100 MB |
| Peak memory (RSS) under load | < 150 MB |
| Battery drain (1h continuous) | < 5% |

## iOS Testing

### Prerequisites

- Xcode 15+ with iPhone connected.
- iOS deployment target ≥ 14.

### Build

```bash
./scripts/build-ios-ffi.sh --release
```

### Instruments

Use Xcode Instruments → *Allocations* and *Energy Log* profiles to measure:

- **Allocations**: peak live bytes < 150 MB.
- **Energy**: CPU impact < 20% over the 5-minute attestation window.

### Memory Test via `memgraph`

```bash
# From a connected device with Xcode:
leaks --outputGraph /tmp/n42.memgraph com.n42.verifier
```

## Automated Reporting

After running the tests, record the results in the release checklist:

```
Device: <model>
OS: <version>
Date: <YYYY-MM-DD>
QUIC connect: <ms>
verify_time_ms (p50): <ms>
verify_time_ms (p99): <ms>
peak_rss_mb: <MB>
PASS/FAIL: <result>
Notes: <any anomalies>
```

File the report as a GitHub issue with label `perf-report` before each mainnet release.
