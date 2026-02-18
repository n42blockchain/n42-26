#ifndef N42_MOBILE_H
#define N42_MOBILE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Error codes returned by FFI functions.
 *
 * Negative values indicate errors; zero indicates success; positive values
 * are function-specific status codes (see n42_verify_and_send).
 */
enum {
    N42_OK                    =  0,
    N42_ERR_INVALID_INPUT     = -1,  /* null pointer, empty data */
    N42_ERR_NOT_CONNECTED     = -2,  /* no active QUIC connection */
    N42_ERR_BUFFER_TOO_SMALL  = -3,  /* output buffer insufficient */
    N42_ERR_INVALID_CERT_HASH = -4,  /* cert_hash_len != 0 and != 32 */
    N42_ERR_CONNECT_FAILED    = -5,  /* QUIC connection establishment failed */
    N42_VERIFY_DECODE_ERROR   =  1,  /* packet decode failed (verify_and_send) */
    N42_VERIFY_EXEC_ERROR     =  2,  /* block verification failed (verify_and_send) */
    N42_VERIFY_SEND_ERROR     =  3,  /* receipt serialization failed (verify_and_send) */
};

/**
 * Opaque verifier context. Created by n42_verifier_init, freed by n42_verifier_free.
 */
typedef struct VerifierContext VerifierContext;

/**
 * Initialize a new verifier context.
 *
 * Creates a BLS12-381 keypair, code cache, and async runtime.
 *
 * @param chain_id  The chain ID (e.g., 4242 for N42 devnet).
 * @return  Pointer to the context, or NULL on failure.
 */
VerifierContext* n42_verifier_init(uint64_t chain_id);

/**
 * Connect to a StarHub QUIC server.
 *
 * Performs the BLS public key handshake after connecting.
 *
 * @param ctx            Valid context from n42_verifier_init.
 * @param host           Null-terminated hostname or IP address.
 * @param port           Port number (default: 9443).
 * @param cert_hash      SHA-256 hash of expected server certificate (32 bytes),
 *                       or NULL for dev mode (accept any certificate).
 * @param cert_hash_len  Must be 0 (dev mode) or 32 (pinned).
 * @return  N42_OK on success, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or host is NULL,
 *          N42_ERR_INVALID_CERT_HASH (-4) if cert_hash_len is not 0 or 32,
 *          N42_ERR_CONNECT_FAILED (-5) if QUIC connection fails.
 */
int n42_connect(VerifierContext* ctx, const char* host, uint16_t port,
                const uint8_t* cert_hash, size_t cert_hash_len);

/**
 * Poll for the next pending verification packet (non-blocking).
 *
 * @param ctx      Valid context.
 * @param out_buf  Buffer to receive packet data.
 * @param buf_len  Size of the output buffer.
 * @return  Number of bytes written (>0), 0 if no data, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or out_buf is NULL,
 *          N42_ERR_NOT_CONNECTED (-2) if not connected,
 *          N42_ERR_BUFFER_TOO_SMALL (-3) if buffer is too small.
 */
int n42_poll_packet(VerifierContext* ctx, uint8_t* out_buf, size_t buf_len);

/**
 * Verify a packet (EVM execution + BLS signature) and send the receipt.
 *
 * This is the main verification entry point. It:
 * 1. Decodes the verification packet
 * 2. Re-executes all transactions using the EVM
 * 3. Verifies the receipts root matches
 * 4. Signs the result with BLS12-381
 * 5. Sends the signed receipt back via QUIC
 *
 * @param ctx   Valid context.
 * @param data  Packet data from n42_poll_packet.
 * @param len   Length of packet data.
 * @return  N42_OK (0) on success, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or data is NULL or len is 0,
 *          N42_VERIFY_DECODE_ERROR (1) if packet decode fails,
 *          N42_VERIFY_EXEC_ERROR (2) if block verification fails,
 *          N42_VERIFY_SEND_ERROR (3) if receipt serialization fails.
 */
int n42_verify_and_send(VerifierContext* ctx, const uint8_t* data, size_t len);

/**
 * Get information about the last verification as a JSON string.
 *
 * JSON fields: block_number, block_hash, receipts_root_match,
 * computed_receipts_root, expected_receipts_root, tx_count,
 * witness_accounts, uncached_bytecodes, packet_size_bytes,
 * verify_time_ms, signature.
 *
 * @param ctx      Valid context.
 * @param out_buf  Buffer for the null-terminated JSON string.
 * @param buf_len  Size of the output buffer.
 * @return  Number of bytes written (excluding null), 0 if no info, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or out_buf is NULL,
 *          N42_ERR_BUFFER_TOO_SMALL (-3) if buffer is too small.
 */
int n42_last_verify_info(VerifierContext* ctx, char* out_buf, size_t buf_len);

/**
 * Get the BLS12-381 public key (48 bytes).
 *
 * @param ctx      Valid context.
 * @param out_buf  Buffer for the 48-byte public key.
 * @return  N42_OK on success, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or out_buf is NULL.
 */
int n42_get_pubkey(VerifierContext* ctx, uint8_t* out_buf);

/**
 * Get verifier statistics as a JSON string.
 *
 * JSON fields: blocks_verified, success_count, failure_count,
 * avg_time_ms, success_rate, dropped_packets.
 *
 * @param ctx      Valid context.
 * @param out_buf  Buffer for the null-terminated JSON string.
 * @param buf_len  Size of the output buffer.
 * @return  Number of bytes written (excluding null), or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx or out_buf is NULL,
 *          N42_ERR_BUFFER_TOO_SMALL (-3) if buffer is too small.
 */
int n42_get_stats(VerifierContext* ctx, char* out_buf, size_t buf_len);

/**
 * Disconnect from the StarHub server.
 *
 * @param ctx  Valid context.
 * @return  N42_OK on success, or:
 *          N42_ERR_INVALID_INPUT (-1) if ctx is NULL,
 *          N42_ERR_NOT_CONNECTED (-2) if not connected.
 */
int n42_disconnect(VerifierContext* ctx);

/**
 * Free the verifier context and all associated resources.
 *
 * Safe to call with NULL (no-op). Must not be called more than once
 * for the same pointer.
 *
 * @param ctx  Context to free, or NULL.
 */
void n42_verifier_free(VerifierContext* ctx);

#ifdef __cplusplus
}
#endif

#endif /* N42_MOBILE_H */
