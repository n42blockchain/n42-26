//! JNI bridge for Android.
//!
//! Maps Kotlin `N42Verifier` native methods to the C FFI API functions.
//! These functions are only compiled when targeting Android (`cfg(target_os = "android")`).

use jni::EnvUnowned;
use jni::errors::ThrowRuntimeExAndDefault;
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jint, jlong, jstring};

use crate::VerifierContext;

/// Casts `&[u8]` to `&[i8]` for JNI's `set_byte_array_region`.
///
/// # Safety
/// u8 and i8 have the same size and alignment.
fn as_jbytes(bytes: &[u8]) -> &[i8] {
    unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const i8, bytes.len()) }
}

/// `N42Verifier.nativeInit(chainId: Long): Long`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeInit<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    chain_id: jlong,
) -> jlong {
    let ptr = unsafe { crate::n42_verifier_init(chain_id as u64) };
    ptr as jlong
}

/// `N42Verifier.nativeConnect(ptr: Long, host: String, port: Int, certHash: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeConnect<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
    host: JString<'caller>,
    port: jint,
    cert_hash: JByteArray<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
            let host_str = match host.try_to_string(env) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "JNI string conversion failed in nativeConnect");
                    return Ok(-1);
                }
            };
            let c_host = match std::ffi::CString::new(host_str) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "CString conversion failed in nativeConnect");
                    return Ok(-1);
                }
            };

            let hash_bytes = match env.convert_byte_array(&cert_hash) {
                Ok(b) => b,
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "cert_hash byte array conversion failed in nativeConnect");
                    return Ok(-1);
                }
            };
            let (hash_ptr, hash_len) = if hash_bytes.is_empty() {
                (std::ptr::null(), 0)
            } else {
                (hash_bytes.as_ptr(), hash_bytes.len())
            };

            Ok(unsafe {
                crate::n42_connect(
                    ptr as *mut VerifierContext,
                    c_host.as_ptr(),
                    port as u16,
                    hash_ptr,
                    hash_len,
                )
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativePollPacket(ptr: Long, buffer: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativePollPacket<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
    buffer: JByteArray<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
            let buf_len = match buffer.len(env) {
                Ok(len) => len,
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "get_array_length failed in nativePollPacket");
                    return Ok(-1);
                }
            };

            let mut temp = vec![0u8; buf_len];
            let result = unsafe {
                crate::n42_poll_packet(ptr as *mut VerifierContext, temp.as_mut_ptr(), buf_len)
            };

            if result > 0
                && buffer
                    .set_region(env, 0, as_jbytes(&temp[..result as usize]))
                    .is_err()
            {
                tracing::warn!(target: "n42::ffi::android", "set_byte_array_region failed in nativePollPacket");
                return Ok(-1);
            }

            Ok(result)
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeVerifyAndSend(ptr: Long, data: ByteArray): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeVerifyAndSend<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
    data: JByteArray<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
            let bytes = match env.convert_byte_array(&data) {
                Ok(b) => b,
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "convert_byte_array failed in nativeVerifyAndSend");
                    return Ok(-1);
                }
            };
            Ok(unsafe {
                crate::n42_verify_and_send(
                    ptr as *mut VerifierContext,
                    bytes.as_ptr(),
                    bytes.len(),
                )
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeVerifyStateProof(proof: ByteArray, stateRoot: ByteArray): Int`
///
/// Stateless SBMT state-proof verification (no context). `proof` is
/// `bincode(ShardedBmtProof)`, `stateRoot` is the 32-byte combined SBMT root.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeVerifyStateProof<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    proof: JByteArray<'caller>,
    state_root: JByteArray<'caller>,
) -> jint {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jint> {
            let proof_bytes = match env.convert_byte_array(&proof) {
                Ok(b) => b,
                Err(_) => return Ok(-1),
            };
            let root_bytes = match env.convert_byte_array(&state_root) {
                Ok(b) => b,
                Err(_) => return Ok(-1),
            };
            if root_bytes.len() != 32 {
                return Ok(-1);
            }
            Ok(unsafe {
                crate::n42_verify_state_proof(
                    proof_bytes.as_ptr(),
                    proof_bytes.len(),
                    root_bytes.as_ptr(),
                )
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeLastVerifyInfo(ptr: Long): String?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeLastVerifyInfo<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
) -> jstring {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jstring> {
            let mut buf = vec![0u8; 8192];
            let len = unsafe {
                crate::n42_last_verify_info(
                    ptr as *mut VerifierContext,
                    buf.as_mut_ptr() as *mut std::ffi::c_char,
                    buf.len(),
                )
            };
            if len <= 0 {
                return Ok(std::ptr::null_mut());
            }
            let s = String::from_utf8_lossy(&buf[..len as usize]);
            Ok(match env.new_string(&*s) {
                Ok(js) => js.into_raw(),
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "new_string failed in nativeLastVerifyInfo");
                    std::ptr::null_mut()
                }
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeGetPubkey(ptr: Long): ByteArray?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeGetPubkey<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
) -> jbyteArray {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jbyteArray> {
            let mut buf = [0u8; 48];
            let result =
                unsafe { crate::n42_get_pubkey(ptr as *mut VerifierContext, buf.as_mut_ptr()) };
            if result != 0 {
                return Ok(std::ptr::null_mut());
            }
            Ok(match env.byte_array_from_slice(&buf) {
                Ok(arr) => arr.into_raw(),
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "byte_array_from_slice failed in nativeGetPubkey");
                    std::ptr::null_mut()
                }
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeGetStats(ptr: Long): String?`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeGetStats<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
) -> jstring {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jstring> {
            let mut buf = vec![0u8; 4096];
            let len = unsafe {
                crate::n42_get_stats(
                    ptr as *mut VerifierContext,
                    buf.as_mut_ptr() as *mut std::ffi::c_char,
                    buf.len(),
                )
            };
            if len <= 0 {
                return Ok(std::ptr::null_mut());
            }
            let s = String::from_utf8_lossy(&buf[..len as usize]);
            Ok(match env.new_string(&*s) {
                Ok(js) => js.into_raw(),
                Err(_) => {
                    tracing::warn!(target: "n42::ffi::android", "new_string failed in nativeGetStats");
                    std::ptr::null_mut()
                }
            })
        })
        .resolve::<ThrowRuntimeExAndDefault>()
}

/// `N42Verifier.nativeDisconnect(ptr: Long): Int`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeDisconnect<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
) -> jint {
    unsafe { crate::n42_disconnect(ptr as *mut VerifierContext) }
}

/// `N42Verifier.nativeFree(ptr: Long)`
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_n42_verifier_N42Verifier_nativeFree<'caller>(
    _env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    ptr: jlong,
) {
    unsafe { crate::n42_verifier_free(ptr as *mut VerifierContext) };
}
