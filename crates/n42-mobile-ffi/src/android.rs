//! JNI bridge for Android.
//!
//! Maps Kotlin `N42Verifier` native methods to the C FFI API functions.
//! These functions are only compiled when targeting Android (`cfg(target_os = "android")`).

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;

use crate::VerifierContext;

/// `N42Verifier.nativeInit(chainId: Long): Long`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeInit(
    _env: JNIEnv,
    _class: JClass,
    chain_id: jlong,
) -> jlong {
    let ptr = unsafe { crate::n42_verifier_init(chain_id as u64) };
    ptr as jlong
}

/// `N42Verifier.nativeConnect(ptr: Long, host: String, port: Int, certHash: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeConnect(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    host: JString,
    port: jint,
    cert_hash: JByteArray,
) -> jint {
    let host_str: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "JNI string conversion failed in nativeConnect");
            return -1;
        }
    };

    let c_host = match std::ffi::CString::new(host_str) {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "CString conversion failed in nativeConnect");
            return -1;
        }
    };

    // Extract cert_hash bytes: empty array = dev mode, 32 bytes = pinned
    let hash_bytes = match env.convert_byte_array(&cert_hash) {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "cert_hash byte array conversion failed in nativeConnect");
            return -1;
        }
    };

    let (hash_ptr, hash_len) = if hash_bytes.is_empty() {
        (std::ptr::null(), 0)
    } else {
        (hash_bytes.as_ptr(), hash_bytes.len())
    };

    unsafe { crate::n42_connect(ptr as *mut VerifierContext, c_host.as_ptr(), port as u16, hash_ptr, hash_len) }
}

/// `N42Verifier.nativePollPacket(ptr: Long, buffer: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativePollPacket(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    buffer: JByteArray,
) -> jint {
    let buf_len = match env.get_array_length(&buffer) {
        Ok(len) => len as usize,
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "get_array_length failed in nativePollPacket");
            return -1;
        }
    };

    let mut temp = vec![0u8; buf_len];
    let result =
        unsafe { crate::n42_poll_packet(ptr as *mut VerifierContext, temp.as_mut_ptr(), buf_len) };

    if result > 0 {
        // Copy data back to Java byte array.
        if env
            .set_byte_array_region(&buffer, 0, bytemuck_cast_slice(&temp[..result as usize]))
            .is_err()
        {
            tracing::warn!(target: "n42::ffi::android", "set_byte_array_region failed in nativePollPacket");
            return -1;
        }
    }

    result
}

/// `N42Verifier.nativeVerifyAndSend(ptr: Long, data: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeVerifyAndSend(
    env: JNIEnv,
    _class: JClass,
    ptr: jlong,
    data: JByteArray,
) -> jint {
    let bytes = match env.convert_byte_array(&data) {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "convert_byte_array failed in nativeVerifyAndSend");
            return -1;
        }
    };

    unsafe { crate::n42_verify_and_send(ptr as *mut VerifierContext, bytes.as_ptr(), bytes.len()) }
}

/// `N42Verifier.nativeLastVerifyInfo(ptr: Long): String?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeLastVerifyInfo(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jstring {
    let mut buf = vec![0u8; 8192];
    let len = unsafe {
        crate::n42_last_verify_info(
            ptr as *mut VerifierContext,
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };

    if len <= 0 {
        return std::ptr::null_mut();
    }

    let s = String::from_utf8_lossy(&buf[..len as usize]);
    match env.new_string(&*s) {
        Ok(js) => js.into_raw(),
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "new_string failed in nativeLastVerifyInfo");
            std::ptr::null_mut()
        }
    }
}

/// `N42Verifier.nativeGetPubkey(ptr: Long): ByteArray?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeGetPubkey(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jbyteArray {
    let mut buf = [0u8; 48];
    let result = unsafe { crate::n42_get_pubkey(ptr as *mut VerifierContext, buf.as_mut_ptr()) };

    if result != 0 {
        return std::ptr::null_mut();
    }

    match env.byte_array_from_slice(&buf) {
        Ok(arr) => arr.into_raw(),
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "byte_array_from_slice failed in nativeGetPubkey");
            std::ptr::null_mut()
        }
    }
}

/// `N42Verifier.nativeGetStats(ptr: Long): String?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeGetStats(
    mut env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jstring {
    let mut buf = vec![0u8; 4096];
    let len = unsafe {
        crate::n42_get_stats(
            ptr as *mut VerifierContext,
            buf.as_mut_ptr() as *mut std::ffi::c_char,
            buf.len(),
        )
    };

    if len <= 0 {
        return std::ptr::null_mut();
    }

    let s = String::from_utf8_lossy(&buf[..len as usize]);
    match env.new_string(&*s) {
        Ok(js) => js.into_raw(),
        Err(_) => {
            tracing::warn!(target: "n42::ffi::android", "new_string failed in nativeGetStats");
            std::ptr::null_mut()
        }
    }
}

/// `N42Verifier.nativeDisconnect(ptr: Long): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeDisconnect(
    _env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) -> jint {
    unsafe { crate::n42_disconnect(ptr as *mut VerifierContext) }
}

/// `N42Verifier.nativeFree(ptr: Long)`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeFree(
    _env: JNIEnv,
    _class: JClass,
    ptr: jlong,
) {
    unsafe { crate::n42_verifier_free(ptr as *mut VerifierContext) };
}

/// Helper to cast `&[u8]` to `&[i8]` for JNI's `set_byte_array_region`.
fn bytemuck_cast_slice(bytes: &[u8]) -> &[i8] {
    // Safety: u8 and i8 have the same size and alignment.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i8, bytes.len()) }
}
