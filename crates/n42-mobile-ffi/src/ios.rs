//! iOS integration layer for the N42 mobile verifier.
//!
//! On iOS, Rust's C ABI (`extern "C"` with `#[unsafe(no_mangle)]`) is directly callable
//! from Swift and Objective-C via a bridging header.  No JNI wrapping is needed —
//! the public C functions defined in `lib.rs` work as-is on iOS targets.
//!
//! # Integration Guide
//!
//! ## Build
//!
//! ```bash
//! # Real device (arm64)
//! cargo build --target aarch64-apple-ios --release -p n42-mobile-ffi
//!
//! # Apple Silicon simulator
//! cargo build --target aarch64-apple-ios-sim --release -p n42-mobile-ffi
//!
//! # Intel simulator (legacy)
//! cargo build --target x86_64-apple-ios --release -p n42-mobile-ffi
//! ```
//!
//! Outputs: `target/<triple>/release/libn42_mobile_ffi.a` (static library).
//!
//! ## Xcode Setup
//!
//! 1. Drag `libn42_mobile_ffi.a` into your Xcode project → *Build Phases → Link Binary With Libraries*.
//! 2. Add a bridging header (e.g. `N42Verifier-Bridging-Header.h`) with the content from
//!    [`N42_C_HEADER`].
//! 3. Set *Swift Compiler → Objective-C Bridging Header* to your bridging header path.
//!
//! ## Swift Usage
//!
//! ```swift
//! import Foundation
//!
//! class N42Verifier {
//!     private var ctx: OpaquePointer?
//!
//!     init(chainId: UInt64) {
//!         ctx = n42_verifier_init(chainId)
//!     }
//!
//!     deinit {
//!         n42_verifier_free(ctx)
//!     }
//!
//!     func connect(host: String, port: UInt16) -> Int32 {
//!         return host.withCString { hostPtr in
//!             n42_connect(ctx, hostPtr, port, nil, 0)
//!         }
//!     }
//!
//!     func pollPacket(buffer: inout [UInt8]) -> Int32 {
//!         return buffer.withUnsafeMutableBytes { ptr in
//!             n42_poll_packet(ctx, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), buffer.count)
//!         }
//!     }
//!
//!     func verifyAndSend(data: [UInt8]) -> Int32 {
//!         return data.withUnsafeBytes { ptr in
//!             n42_verify_and_send(ctx, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), data.count)
//!         }
//!     }
//!
//!     func getPubkey() -> [UInt8]? {
//!         var buf = [UInt8](repeating: 0, count: 48)
//!         let result = n42_get_pubkey(ctx, &buf)
//!         return result == 0 ? buf : nil
//!     }
//!
//!     func getStats() -> String? {
//!         var buf = [CChar](repeating: 0, count: 4096)
//!         let len = n42_get_stats(ctx, &buf, buf.count)
//!         return len > 0 ? String(cString: buf) : nil
//!     }
//!
//!     func lastVerifyInfo() -> String? {
//!         var buf = [CChar](repeating: 0, count: 8192)
//!         let len = n42_last_verify_info(ctx, &buf, buf.count)
//!         return len > 0 ? String(cString: buf) : nil
//!     }
//!
//!     func disconnect() -> Int32 {
//!         return n42_disconnect(ctx)
//!     }
//! }
//! ```
//!
//! ## Error Codes
//!
//! | Code | Meaning |
//! |------|---------|
//! | `0`  | Success |
//! | `-1` | Null pointer / invalid argument |
//! | `-2` | Not connected to StarHub |
//! | `-3` | Output buffer too small |
//! | `-4` | Invalid cert hash length (expected 0 or 32 bytes) |
//! | `-5` | QUIC connection failed |
//! | `1`  | Packet decode failed |
//! | `2`  | Block verification failed |
//! | `3`  | Receipt serialization failed |
//!
//! ## Memory Management
//!
//! - `n42_verifier_init` returns a heap-allocated context.  **Always** call `n42_verifier_free`
//!   when done, even if intermediate calls fail.
//! - Buffers passed to `n42_poll_packet`, `n42_verify_and_send`, `n42_get_stats`, and
//!   `n42_last_verify_info` are caller-owned; the library copies data in/out.
//! - Do not share a context between threads without external synchronisation.

/// C header string for the N42 mobile FFI — copy into your Xcode bridging header.
///
/// ```objc
/// #include "n42_mobile_ffi.h"
/// ```
pub const N42_C_HEADER: &str = include_str!("../include/n42_mobile.h");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_contains_all_functions() {
        assert!(N42_C_HEADER.contains("n42_verifier_init"));
        assert!(N42_C_HEADER.contains("n42_connect"));
        assert!(N42_C_HEADER.contains("n42_poll_packet"));
        assert!(N42_C_HEADER.contains("n42_verify_and_send"));
        assert!(N42_C_HEADER.contains("n42_verify_state_proof"));
        assert!(N42_C_HEADER.contains("n42_verify_account_proof"));
        assert!(N42_C_HEADER.contains("n42_verify_storage_proof"));
        assert!(N42_C_HEADER.contains("n42_verify_twig_state_proof"));
        assert!(N42_C_HEADER.contains("n42_verify_twig_account_proof"));
        assert!(N42_C_HEADER.contains("n42_verify_twig_storage_proof"));
        assert!(N42_C_HEADER.contains("n42_last_verify_info"));
        assert!(N42_C_HEADER.contains("n42_get_pubkey"));
        assert!(N42_C_HEADER.contains("n42_get_stats"));
        assert!(N42_C_HEADER.contains("n42_disconnect"));
        assert!(N42_C_HEADER.contains("n42_verifier_free"));
    }

    #[test]
    fn test_header_is_valid_c_syntax_structure() {
        // Verify the embedded header keeps the actual public include shape.
        assert!(N42_C_HEADER.contains("#ifndef N42_MOBILE_H"));
        assert!(N42_C_HEADER.contains("#define N42_MOBILE_H"));
        assert!(N42_C_HEADER.contains("extern \"C\""));
        assert!(N42_C_HEADER.contains("VerifierContext"));
    }
}
