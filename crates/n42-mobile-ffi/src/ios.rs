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
pub const N42_C_HEADER: &str = r#"
#pragma once
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct VerifierContext VerifierContext;

/**
 * Initialises a new verifier context for the given chain ID.
 * Returns NULL on failure. The caller must free with `n42_verifier_free`.
 */
VerifierContext* n42_verifier_init(uint64_t chain_id);

/**
 * Connects to a StarHub QUIC server at host:port.
 * `cert_hash` is NULL (dev/accept-any) or a 32-byte SHA-256 of the server cert.
 * Returns 0 on success, negative on error.
 */
int n42_connect(
    VerifierContext* ctx,
    const char* host,
    uint16_t port,
    const uint8_t* cert_hash,
    size_t cert_hash_len
);

/**
 * Non-blocking poll for the next pending verification packet.
 * Copies bytes into `out_buf`. Returns bytes written, 0 if empty, or negative on error.
 */
int n42_poll_packet(VerifierContext* ctx, uint8_t* out_buf, size_t buf_len);

/**
 * Verifies a packet (EVM re-execution + BLS signature) and sends the receipt.
 * Auto-detects V1 (legacy bincode) and V2 (magic "N2") wire formats.
 * Returns 0 on success, non-zero on error.
 */
int n42_verify_and_send(VerifierContext* ctx, const uint8_t* data, size_t len);

/**
 * Copies the last verification result as a null-terminated JSON string into `out_buf`.
 * Returns bytes written (excluding null terminator), 0 if no data, or negative on error.
 */
int n42_last_verify_info(VerifierContext* ctx, char* out_buf, size_t buf_len);

/**
 * Copies the verifier's BLS12-381 public key (48 bytes) into `out_buf`.
 * Returns 0 on success, -1 on error.
 */
int n42_get_pubkey(VerifierContext* ctx, uint8_t* out_buf);

/**
 * Copies runtime statistics as a null-terminated JSON string into `out_buf`.
 * Returns bytes written (excluding null terminator), or negative on error.
 */
int n42_get_stats(VerifierContext* ctx, char* out_buf, size_t buf_len);

/**
 * Disconnects from the StarHub server.
 * Returns 0 on success, -2 if not connected.
 */
int n42_disconnect(VerifierContext* ctx);

/**
 * Frees the verifier context. Safe to call with NULL (no-op).
 * Must not be called more than once for the same pointer.
 */
void n42_verifier_free(VerifierContext* ctx);

#ifdef __cplusplus
}
#endif
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_contains_all_functions() {
        assert!(N42_C_HEADER.contains("n42_verifier_init"));
        assert!(N42_C_HEADER.contains("n42_connect"));
        assert!(N42_C_HEADER.contains("n42_poll_packet"));
        assert!(N42_C_HEADER.contains("n42_verify_and_send"));
        assert!(N42_C_HEADER.contains("n42_last_verify_info"));
        assert!(N42_C_HEADER.contains("n42_get_pubkey"));
        assert!(N42_C_HEADER.contains("n42_get_stats"));
        assert!(N42_C_HEADER.contains("n42_disconnect"));
        assert!(N42_C_HEADER.contains("n42_verifier_free"));
    }

    #[test]
    fn test_header_is_valid_c_syntax_structure() {
        // Verify the header has proper include guards and extern "C".
        assert!(N42_C_HEADER.contains("#pragma once"));
        assert!(N42_C_HEADER.contains("extern \"C\""));
        assert!(N42_C_HEADER.contains("VerifierContext"));
    }
}
