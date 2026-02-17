package com.n42.verifier

import org.json.JSONObject

/**
 * JNI wrapper for the N42 mobile verifier FFI library.
 *
 * Loads `libn42_mobile_ffi.so` and provides Kotlin-friendly methods
 * around the C API defined in `n42_mobile.h`.
 */
class N42Verifier private constructor(private val nativePtr: Long) {

    companion object {
        init {
            System.loadLibrary("n42_mobile_ffi")
        }

        /**
         * Creates a new verifier instance with a BLS12-381 keypair.
         * @param chainId The chain ID (default: 4242 for N42 devnet).
         * @return A new N42Verifier, or null on failure.
         */
        fun create(chainId: Long = 4242L): N42Verifier? {
            val ptr = nativeInit(chainId)
            return if (ptr != 0L) N42Verifier(ptr) else null
        }

        // Native methods (JNI)
        @JvmStatic private external fun nativeInit(chainId: Long): Long
        @JvmStatic private external fun nativeConnect(ptr: Long, host: String, port: Int): Int
        @JvmStatic private external fun nativePollPacket(ptr: Long, buffer: ByteArray): Int
        @JvmStatic private external fun nativeVerifyAndSend(ptr: Long, data: ByteArray): Int
        @JvmStatic private external fun nativeLastVerifyInfo(ptr: Long): String?
        @JvmStatic private external fun nativeGetPubkey(ptr: Long): ByteArray?
        @JvmStatic private external fun nativeGetStats(ptr: Long): String?
        @JvmStatic private external fun nativeDisconnect(ptr: Long): Int
        @JvmStatic private external fun nativeFree(ptr: Long)
    }

    var isConnected = false
        private set

    /**
     * Connect to a StarHub QUIC server.
     */
    fun connect(host: String, port: Int = 9443): Boolean {
        val result = nativeConnect(nativePtr, host, port)
        isConnected = result == 0
        return isConnected
    }

    /**
     * Poll for the next pending verification packet.
     * @return Packet data, or null if no packet is available.
     */
    fun pollPacket(): ByteArray? {
        val buffer = ByteArray(4 * 1024 * 1024) // 4MB max packet
        val size = nativePollPacket(nativePtr, buffer)
        return when {
            size > 0 -> buffer.copyOf(size)
            else -> null
        }
    }

    /**
     * Verify a packet and send the signed receipt.
     * @return true on success.
     */
    fun verifyAndSend(data: ByteArray): Boolean {
        return nativeVerifyAndSend(nativePtr, data) == 0
    }

    /**
     * Get the last verification info as a parsed JSON object.
     */
    fun lastVerifyInfo(): JSONObject? {
        val json = nativeLastVerifyInfo(nativePtr) ?: return null
        return JSONObject(json)
    }

    /**
     * Get the BLS12-381 public key (48 bytes).
     */
    fun getPublicKey(): ByteArray? {
        return nativeGetPubkey(nativePtr)
    }

    /**
     * Get the public key as a hex string (for display).
     */
    fun getPublicKeyHex(): String {
        val bytes = getPublicKey() ?: return ""
        return bytes.joinToString("") { "%02x".format(it) }
    }

    /**
     * Get verifier statistics as a parsed JSON object.
     */
    fun getStats(): JSONObject? {
        val json = nativeGetStats(nativePtr) ?: return null
        return JSONObject(json)
    }

    /**
     * Disconnect from the StarHub server.
     */
    fun disconnect(): Boolean {
        val result = nativeDisconnect(nativePtr)
        if (result == 0) isConnected = false
        return result == 0
    }

    /**
     * Release native resources. Must be called when done.
     */
    fun destroy() {
        isConnected = false
        nativeFree(nativePtr)
    }

    protected fun finalize() {
        destroy()
    }
}
