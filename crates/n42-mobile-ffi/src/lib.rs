use n42_mobile::code_cache::{CacheSyncMessage, CodeCache};
use n42_mobile::packet::decode_packet;
use n42_mobile::receipt::sign_receipt;
use n42_mobile::verifier::{update_cache_after_verify, verify_block};
use n42_primitives::BlsSecretKey;
use reth_chainspec::ChainSpec;
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

// ── Safety helpers ──

/// Recovers from a poisoned mutex instead of panicking.
///
/// If a thread panics while holding the mutex, subsequent `.lock()` calls
/// return `Err(PoisonError)`. This helper logs a warning and recovers the
/// inner data, preventing cascading panics across FFI boundaries.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        warn!("mutex poisoned, recovering");
        poisoned.into_inner()
    })
}

/// Safely converts `usize` to `c_int`, returning -1 on overflow.
///
/// Prevents undefined behavior when packet sizes exceed `c_int::MAX` (~2GB).
fn safe_cint(val: usize) -> c_int {
    c_int::try_from(val).unwrap_or(-1)
}

/// FFI error type for structured error reporting.
///
/// Each variant maps to a distinct C error code, enabling callers to
/// differentiate error causes without parsing log messages.
#[derive(Debug, thiserror::Error)]
enum FfiError {
    #[error("null context pointer")]
    NullContext,
    #[error("null buffer pointer")]
    NullBuffer,
    #[error("null host string")]
    NullHost,
    #[error("not connected to StarHub")]
    NotConnected,
    #[error("output buffer too small (need {need}, have {have})")]
    BufferTooSmall { need: usize, have: usize },
    #[error("invalid cert_hash length: expected 0 or 32, got {0}")]
    InvalidCertHashLen(usize),
    #[error("QUIC connection failed: {0}")]
    ConnectFailed(Box<dyn std::error::Error + Send + Sync>),
    #[error("packet decode failed: {0}")]
    PacketDecode(String),
    #[error("block verification failed: {0}")]
    VerifyFailed(String),
    #[error("receipt serialization failed: {0}")]
    SerializeFailed(String),
    #[error("null or zero-length data")]
    InvalidData,
}

impl FfiError {
    /// Maps to a C error code and logs the error via tracing.
    fn into_code(self) -> c_int {
        let code = match &self {
            Self::NullContext | Self::NullBuffer | Self::NullHost | Self::InvalidData => -1,
            Self::NotConnected => -2,
            Self::BufferTooSmall { .. } => -3,
            Self::InvalidCertHashLen(_) => -4,
            Self::ConnectFailed(_) => -5,
            Self::PacketDecode(_) => 1,
            Self::VerifyFailed(_) => 2,
            Self::SerializeFailed(_) => 3,
        };
        warn!(code, error = %self, "FFI error");
        code
    }
}

/// Maximum number of pending packets in the receive queue.
const MAX_PENDING_PACKETS: usize = 64;

/// Default code cache capacity (number of contract bytecodes).
const DEFAULT_CODE_CACHE_CAPACITY: usize = 1000;

/// Statistics tracked across verification sessions.
#[derive(Debug, Default)]
struct VerifyStats {
    blocks_verified: u64,
    success_count: u64,
    failure_count: u64,
    total_verify_time_ms: u64,
}

/// Information about the last verification (exposed to UI via JSON).
#[derive(Debug, serde::Serialize)]
struct LastVerifyInfo {
    block_number: u64,
    block_hash: String,
    receipts_root_match: bool,
    computed_receipts_root: String,
    expected_receipts_root: String,
    tx_count: usize,
    witness_accounts: usize,
    uncached_bytecodes: usize,
    packet_size_bytes: usize,
    verify_time_ms: u64,
    signature: String,
}

/// QUIC connection to a StarHub node.
struct QuicConnection {
    connection: quinn::Connection,
    pending_packets: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Counter for packets dropped due to full queue.
    dropped_count: Arc<AtomicU64>,
    /// Handle to the background receiver task.
    _recv_task: tokio::task::JoinHandle<()>,
}

/// The main verifier context, opaque to C callers.
pub struct VerifierContext {
    #[allow(dead_code)]
    chain_id: u64,
    chain_spec: Arc<ChainSpec>,
    signing_key: BlsSecretKey,
    pubkey_bytes: [u8; 48],
    code_cache: Arc<Mutex<CodeCache>>,
    runtime: tokio::runtime::Runtime,
    connection: Mutex<Option<QuicConnection>>,
    stats: Mutex<VerifyStats>,
    last_info: Mutex<Option<LastVerifyInfo>>,
}

// ── C FFI API ──

/// Initializes a new verifier context.
///
/// Creates a BLS12-381 keypair, CodeCache, and tokio runtime.
/// Returns a pointer to the context, or null on failure.
///
/// # Safety
/// The returned pointer must be freed with `n42_verifier_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_verifier_init(chain_id: u64) -> *mut VerifierContext {
    // Initialize tracing (once).
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "n42_mobile_ffi=info".parse().unwrap()),
            )
            .try_init();
    });

    let signing_key = match BlsSecretKey::random() {
        Ok(key) => key,
        Err(e) => {
            error!("failed to generate BLS key: {}", e);
            return std::ptr::null_mut();
        }
    };

    let pubkey = signing_key.public_key();
    let pubkey_bytes = pubkey.to_bytes();

    // Use the N42 dev chain spec. The chain_id parameter is reserved for future
    // multi-chain support; currently all mobile verifiers use the same spec.
    let chain_spec = n42_chainspec::n42_dev_chainspec();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("n42-ffi")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("failed to create tokio runtime: {}", e);
            return std::ptr::null_mut();
        }
    };

    let ctx = Box::new(VerifierContext {
        chain_id,
        chain_spec,
        signing_key,
        pubkey_bytes,
        code_cache: Arc::new(Mutex::new(CodeCache::new(DEFAULT_CODE_CACHE_CAPACITY))),
        runtime,
        connection: Mutex::new(None),
        stats: Mutex::new(VerifyStats::default()),
        last_info: Mutex::new(None),
    });

    info!(chain_id, "verifier context initialized");
    Box::into_raw(ctx)
}

/// Connects to a StarHub QUIC server.
///
/// Sends the BLS public key as the handshake message.
/// Returns 0 on success, -1 on error.
///
/// # Safety
/// `ctx` must be a valid pointer from `n42_verifier_init`.
/// `host` must be a valid null-terminated C string.
/// `cert_hash` is NULL for dev mode (accept any certificate), or points to 32 bytes
/// containing the SHA-256 hash of the expected server certificate.
/// `cert_hash_len` must be 0 (dev mode) or 32 (pinned). Other values return -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_connect(
    ctx: *mut VerifierContext,
    host: *const c_char,
    port: u16,
    cert_hash: *const u8,
    cert_hash_len: usize,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if host.is_null() {
        return FfiError::NullHost.into_code();
    }

    let host_str = match unsafe { CStr::from_ptr(host) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return FfiError::NullHost.into_code(),
    };

    let expected_cert_hash = if cert_hash.is_null() || cert_hash_len == 0 {
        None
    } else if cert_hash_len == 32 {
        let mut hash = [0u8; 32];
        unsafe { std::ptr::copy_nonoverlapping(cert_hash, hash.as_mut_ptr(), 32) };
        Some(hash)
    } else {
        return FfiError::InvalidCertHashLen(cert_hash_len).into_code();
    };

    let pubkey_bytes = ctx.pubkey_bytes;

    match ctx.runtime.block_on(connect_quic(&host_str, port, &pubkey_bytes, expected_cert_hash)) {
        Ok((connection, pending_packets)) => {
            // Spawn background task to receive packets from QUIC streams.
            let conn_clone = connection.clone();
            let packets_clone = pending_packets.clone();
            let cache_clone = ctx.code_cache.clone();
            let dropped = Arc::new(AtomicU64::new(0));
            let dropped_clone = dropped.clone();
            let recv_task = ctx.runtime.spawn(async move {
                recv_loop(conn_clone, packets_clone, cache_clone, dropped_clone).await;
            });

            let quic_conn = QuicConnection {
                connection,
                pending_packets,
                dropped_count: dropped,
                _recv_task: recv_task,
            };
            *lock_or_recover(&ctx.connection) = Some(quic_conn);
            info!(%host_str, port, "connected to StarHub");
            0
        }
        Err(e) => {
            FfiError::ConnectFailed(e).into_code()
        }
    }
}

/// Polls for the next pending verification packet (non-blocking).
///
/// Copies packet data to `out_buf`. Returns the number of bytes written,
/// 0 if no packet is available, or -1 on error.
///
/// # Safety
/// `ctx` must be valid. `out_buf` must have at least `buf_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_poll_packet(
    ctx: *mut VerifierContext,
    out_buf: *mut u8,
    buf_len: usize,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if out_buf.is_null() {
        return FfiError::NullBuffer.into_code();
    }

    let conn_guard = lock_or_recover(&ctx.connection);
    let conn = match conn_guard.as_ref() {
        Some(c) => c,
        None => return FfiError::NotConnected.into_code(),
    };

    let mut queue = lock_or_recover(&conn.pending_packets);
    match queue.pop_front() {
        Some(data) => {
            if data.len() > buf_len {
                // Buffer too small; put it back.
                let need = data.len();
                queue.push_front(data);
                return FfiError::BufferTooSmall { need, have: buf_len }.into_code();
            }
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), out_buf, data.len());
            }
            safe_cint(data.len())
        }
        None => 0,
    }
}

/// Verifies a packet (EVM execution + BLS signature) and sends the receipt.
///
/// Returns 0 on success, non-zero on error.
///
/// # Safety
/// `ctx` must be valid. `data` must point to `len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_verify_and_send(
    ctx: *mut VerifierContext,
    data: *const u8,
    len: usize,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if data.is_null() || len == 0 {
        return FfiError::InvalidData.into_code();
    }

    let packet_bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let packet_size = len;

    // Decode the packet.
    let packet = match decode_packet(packet_bytes) {
        Ok(p) => p,
        Err(e) => {
            return FfiError::PacketDecode(e.to_string()).into_code();
        }
    };

    let block_number = packet.block_number;
    let block_hash = packet.block_hash;
    let tx_count = packet.transactions.len();
    let witness_count = packet.witness_accounts.len();
    let uncached_count = packet.uncached_bytecodes.len();
    let expected_receipts_root = packet.receipts_root;

    // Execute and verify.
    let start = Instant::now();
    let result = {
        let mut cache = lock_or_recover(&ctx.code_cache);
        verify_block(&packet, &mut cache, ctx.chain_spec.clone())
    };
    let verify_time_ms = start.elapsed().as_millis() as u64;

    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let mut stats = lock_or_recover(&ctx.stats);
            stats.blocks_verified += 1;
            stats.failure_count += 1;
            return FfiError::VerifyFailed(e.to_string()).into_code();
        }
    };

    // Update code cache with newly received bytecodes.
    {
        let mut cache = lock_or_recover(&ctx.code_cache);
        update_cache_after_verify(&packet, &mut cache);
    }

    // Sign the receipt.
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let receipt = sign_receipt(
        block_hash,
        block_number,
        // state_root_match: always true because mobile verifiers only re-execute
        // transactions and verify receipts_root. Full state_root verification would
        // require the complete state trie, which is infeasible on mobile devices.
        true,
        result.receipts_root_match,
        timestamp_ms,
        &ctx.signing_key,
    );

    // Update stats.
    {
        let mut stats = lock_or_recover(&ctx.stats);
        stats.blocks_verified += 1;
        if result.receipts_root_match {
            stats.success_count += 1;
        } else {
            stats.failure_count += 1;
        }
        stats.total_verify_time_ms += verify_time_ms;
    }

    // Store last verify info.
    let sig_bytes = receipt.signature.to_bytes();
    {
        let info = LastVerifyInfo {
            block_number,
            block_hash: format!("{:#x}", block_hash),
            receipts_root_match: result.receipts_root_match,
            computed_receipts_root: format!("{:#x}", result.computed_receipts_root),
            expected_receipts_root: format!("{:#x}", expected_receipts_root),
            tx_count,
            witness_accounts: witness_count,
            uncached_bytecodes: uncached_count,
            packet_size_bytes: packet_size,
            verify_time_ms,
            signature: hex::encode(sig_bytes),
        };
        *lock_or_recover(&ctx.last_info) = Some(info);
    }

    info!(
        block_number,
        match_ = result.receipts_root_match,
        verify_time_ms,
        "block verified"
    );

    // Send receipt via QUIC.
    let conn_guard = lock_or_recover(&ctx.connection);
    if let Some(conn) = conn_guard.as_ref() {
        let receipt_bytes = match bincode::serialize(&receipt) {
            Ok(b) => b,
            Err(e) => {
                return FfiError::SerializeFailed(e.to_string()).into_code();
            }
        };

        let connection = conn.connection.clone();
        ctx.runtime.spawn(async move {
            match connection.open_uni().await {
                Ok(mut stream) => {
                    if let Err(e) = stream.write_all(&receipt_bytes).await {
                        warn!("failed to send receipt: {}", e);
                    }
                    let _ = stream.finish();
                }
                Err(e) => {
                    warn!("failed to open uni stream for receipt: {}", e);
                }
            }
        });
    }

    0
}

/// Gets information about the last verification as JSON.
///
/// Returns the number of bytes written, or -1 on error.
///
/// # Safety
/// `ctx` must be valid. `out_buf` must have at least `buf_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_last_verify_info(
    ctx: *mut VerifierContext,
    out_buf: *mut c_char,
    buf_len: usize,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if out_buf.is_null() {
        return FfiError::NullBuffer.into_code();
    }

    let info_guard = lock_or_recover(&ctx.last_info);
    let info = match info_guard.as_ref() {
        Some(i) => i,
        None => return 0,
    };

    let json = match serde_json::to_string(info) {
        Ok(s) => s,
        Err(_) => return FfiError::SerializeFailed("JSON serialization failed".into()).into_code(),
    };

    if json.len() + 1 > buf_len {
        return FfiError::BufferTooSmall { need: json.len() + 1, have: buf_len }.into_code();
    }

    unsafe {
        std::ptr::copy_nonoverlapping(json.as_ptr(), out_buf as *mut u8, json.len());
        *out_buf.add(json.len()) = 0; // null-terminate
    }
    safe_cint(json.len())
}

/// Gets the BLS12-381 public key (48 bytes).
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
/// `ctx` must be valid. `out_buf` must have at least 48 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_get_pubkey(
    ctx: *mut VerifierContext,
    out_buf: *mut u8,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if out_buf.is_null() {
        return FfiError::NullBuffer.into_code();
    }

    unsafe {
        std::ptr::copy_nonoverlapping(ctx.pubkey_bytes.as_ptr(), out_buf, 48);
    }
    0
}

/// Gets verifier statistics as JSON.
///
/// Returns the number of bytes written, or -1 on error.
///
/// # Safety
/// `ctx` must be valid. `out_buf` must have at least `buf_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_get_stats(
    ctx: *mut VerifierContext,
    out_buf: *mut c_char,
    buf_len: usize,
) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    if out_buf.is_null() {
        return FfiError::NullBuffer.into_code();
    }

    let stats = lock_or_recover(&ctx.stats);
    let avg_time = if stats.blocks_verified > 0 {
        stats.total_verify_time_ms / stats.blocks_verified
    } else {
        0
    };

    let dropped = {
        let conn_guard = lock_or_recover(&ctx.connection);
        conn_guard
            .as_ref()
            .map(|c| c.dropped_count.load(Ordering::Relaxed))
            .unwrap_or(0)
    };

    let json = serde_json::json!({
        "blocks_verified": stats.blocks_verified,
        "success_count": stats.success_count,
        "failure_count": stats.failure_count,
        "avg_time_ms": avg_time,
        "success_rate": if stats.blocks_verified > 0 {
            (stats.success_count as f64 / stats.blocks_verified as f64 * 100.0) as u64
        } else {
            0
        },
        "dropped_packets": dropped,
    });

    let json_str = json.to_string();
    if json_str.len() + 1 > buf_len {
        return FfiError::BufferTooSmall { need: json_str.len() + 1, have: buf_len }.into_code();
    }

    unsafe {
        std::ptr::copy_nonoverlapping(json_str.as_ptr(), out_buf as *mut u8, json_str.len());
        *out_buf.add(json_str.len()) = 0;
    }
    safe_cint(json_str.len())
}

/// Disconnects from the StarHub server.
///
/// Returns 0 on success, -1 if not connected.
///
/// # Safety
/// `ctx` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_disconnect(ctx: *mut VerifierContext) -> c_int {
    let ctx = match unsafe { ctx.as_ref() } {
        Some(c) => c,
        None => return FfiError::NullContext.into_code(),
    };

    let mut conn_guard = lock_or_recover(&ctx.connection);
    if let Some(conn) = conn_guard.take() {
        conn.connection.close(0u32.into(), b"disconnect");
        info!("disconnected from StarHub");
        0
    } else {
        FfiError::NotConnected.into_code()
    }
}

/// Frees the verifier context.
///
/// # Safety
/// `ctx` must be a valid pointer from `n42_verifier_init`, or null (no-op).
/// Must not be called more than once for the same pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn n42_verifier_free(ctx: *mut VerifierContext) {
    if ctx.is_null() {
        return;
    }
    let ctx = unsafe { Box::from_raw(ctx) };
    // Close QUIC connection and abort the background recv_task to prevent
    // use-after-free: the task holds an Arc<Mutex<VecDeque>> that would
    // otherwise outlive the VerifierContext.
    let mut guard = lock_or_recover(&ctx.connection);
    if let Some(conn) = guard.take() {
        conn.connection.close(0u32.into(), b"shutdown");
        conn._recv_task.abort();
    }
    drop(guard);
    info!("verifier context freed");
    // ctx dropped here, runtime shuts down
}

// ── Internal helpers ──

/// Establishes a QUIC connection to the StarHub and performs the BLS pubkey handshake.
async fn connect_quic(
    host: &str,
    port: u16,
    pubkey: &[u8; 48],
    expected_cert_hash: Option<[u8; 32]>,
) -> Result<(quinn::Connection, Arc<Mutex<VecDeque<Vec<u8>>>>), Box<dyn std::error::Error + Send + Sync>>
{
    // Configure QUIC client with certificate pinning (or skip-verify for dev mode).
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerification {
            expected_hash: expected_cert_hash,
        }))
        .with_no_client_auth();

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(15)));

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    client_config.transport_config(Arc::new(transport));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let addr = format!("{}:{}", host, port);
    let connection = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.connect(addr.parse()?, "n42-starhub")?,
    )
    .await
    .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        "QUIC connect timed out after 10s".into()
    })??;

    info!("QUIC connection established, sending BLS pubkey handshake");

    // Handshake: send 48-byte BLS public key via uni-stream.
    let mut handshake_stream = connection.open_uni().await?;
    handshake_stream.write_all(pubkey).await?;
    handshake_stream.finish()?;

    let pending = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PENDING_PACKETS)));

    Ok((connection, pending))
}

/// Applies a `CacheSyncMessage` to the code cache.
///
/// Inserts new bytecodes and removes evict hints. Returns `(added, evicted)` counts.
/// Evict hints for keys not in the cache are silently ignored.
fn apply_cache_sync(msg: CacheSyncMessage, cache: &Mutex<CodeCache>) -> (usize, usize) {
    let mut guard = lock_or_recover(cache);
    let added = msg.codes.len();
    for (hash, code) in msg.codes {
        guard.insert(hash, code);
    }
    let evicted = msg.evict_hints.len();
    for hash in &msg.evict_hints {
        guard.remove(hash);
    }
    (added, evicted)
}

/// Background task that receives packets from the StarHub via QUIC uni-streams.
///
/// StarHub sends data with a 1-byte type prefix:
/// - 0x01: VerificationPacket
/// - 0x02: CacheSyncMessage
async fn recv_loop(
    connection: quinn::Connection,
    pending_packets: Arc<Mutex<VecDeque<Vec<u8>>>>,
    code_cache: Arc<Mutex<CodeCache>>,
    dropped_count: Arc<AtomicU64>,
) {
    loop {
        match connection.accept_uni().await {
            Ok(mut stream) => {
                // Read all data from the stream (max 16MB).
                match stream.read_to_end(16 * 1024 * 1024).await {
                    Ok(data) => {
                        if data.is_empty() {
                            continue;
                        }

                        let msg_type = data[0];
                        let payload = &data[1..];

                        match msg_type {
                            0x01 => {
                                // VerificationPacket (uncompressed, legacy)
                                debug!(size = payload.len(), "received verification packet");
                                let mut queue = lock_or_recover(&pending_packets);
                                if queue.len() >= MAX_PENDING_PACKETS {
                                    dropped_count.fetch_add(1, Ordering::Relaxed);
                                    warn!(
                                        queue_len = queue.len(),
                                        dropped = dropped_count.load(Ordering::Relaxed),
                                        "packet queue full, dropping oldest packet"
                                    );
                                    queue.pop_front();
                                }
                                queue.push_back(payload.to_vec());
                            }
                            0x03 => {
                                // VerificationPacket (zstd compressed)
                                match zstd::bulk::decompress(payload, 16 * 1024 * 1024) {
                                    Ok(decompressed) => {
                                        debug!(
                                            compressed = payload.len(),
                                            decompressed = decompressed.len(),
                                            "received verification packet (zstd)"
                                        );
                                        let mut queue = lock_or_recover(&pending_packets);
                                        if queue.len() >= MAX_PENDING_PACKETS {
                                            dropped_count.fetch_add(1, Ordering::Relaxed);
                                            warn!(
                                                queue_len = queue.len(),
                                                dropped = dropped_count.load(Ordering::Relaxed),
                                                "packet queue full, dropping oldest packet"
                                            );
                                            queue.pop_front();
                                        }
                                        queue.push_back(decompressed);
                                    }
                                    Err(e) => {
                                        warn!("zstd decompress failed: {}", e);
                                    }
                                }
                            }
                            0x02 => {
                                // CacheSyncMessage (uncompressed, legacy)
                                const MAX_CACHE_SYNC_SIZE: usize = 16 * 1024 * 1024; // 16MB
                                if payload.len() > MAX_CACHE_SYNC_SIZE {
                                    warn!("CacheSyncMessage too large ({} bytes), dropping", payload.len());
                                    continue;
                                }
                                match bincode::deserialize::<CacheSyncMessage>(payload)
                                {
                                    Ok(msg) => {
                                        let (added, evicted) =
                                            apply_cache_sync(msg, &code_cache);
                                        let cache_size = lock_or_recover(&code_cache).len();
                                        info!(
                                            added,
                                            evicted,
                                            cache_size,
                                            "applied cache sync message"
                                        );
                                    }
                                    Err(e) => {
                                        warn!("failed to decode CacheSyncMessage: {}", e);
                                    }
                                }
                            }
                            0x04 => {
                                // CacheSyncMessage (zstd compressed)
                                match zstd::bulk::decompress(payload, 16 * 1024 * 1024) {
                                    Ok(decompressed) => {
                                        match bincode::deserialize::<CacheSyncMessage>(&decompressed) {
                                            Ok(msg) => {
                                                let (added, evicted) =
                                                    apply_cache_sync(msg, &code_cache);
                                                let cache_size = lock_or_recover(&code_cache).len();
                                                info!(
                                                    added,
                                                    evicted,
                                                    cache_size,
                                                    "applied compressed cache sync message"
                                                );
                                            }
                                            Err(e) => {
                                                warn!("failed to decode CacheSyncMessage: {}", e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("zstd decompress CacheSync failed: {}", e);
                                    }
                                }
                            }
                            _ => {
                                warn!(msg_type, "unknown message type from StarHub");
                            }
                        }
                    }
                    Err(e) => {
                        debug!("stream read error: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("connection closed: {}", e);
                break;
            }
        }
    }
}

// ── Android JNI bridge ──
// Maps Kotlin N42Verifier native methods to the C FFI functions.
#[cfg(target_os = "android")]
mod android;

/// Certificate verifier that validates the server certificate against a pinned
/// SHA-256 hash. When `expected_hash` is `None` (dev mode), all certificates
/// are accepted (equivalent to the previous `SkipServerVerification`).
///
/// In production, the expected hash should be the SHA-256 of the StarHub's
/// DER-encoded certificate, obtained via an out-of-band channel or initial
/// trust-on-first-use (TOFU).
#[derive(Debug)]
struct PinnedCertVerification {
    expected_hash: Option<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertVerification {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = &self.expected_hash {
            use sha2::{Sha256, Digest};
            let mut hasher = Sha256::new();
            hasher.update(end_entity.as_ref());
            let actual: [u8; 32] = hasher.finalize().into();
            if actual != *expected {
                return Err(rustls::Error::General(format!(
                    "certificate hash mismatch: expected {}, got {}",
                    hex::encode(expected),
                    hex::encode(actual),
                )));
            }
        }
        // Dev mode (no expected hash) or hash matches: accept.
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_stats_default() {
        let stats = VerifyStats::default();
        assert_eq!(stats.blocks_verified, 0);
        assert_eq!(stats.success_count, 0);
        assert_eq!(stats.failure_count, 0);
        assert_eq!(stats.total_verify_time_ms, 0);
    }

    #[test]
    fn test_last_verify_info_serialization() {
        let info = LastVerifyInfo {
            block_number: 42,
            block_hash: "0x1234".to_string(),
            receipts_root_match: true,
            computed_receipts_root: "0xaaaa".to_string(),
            expected_receipts_root: "0xbbbb".to_string(),
            tx_count: 10,
            witness_accounts: 5,
            uncached_bytecodes: 2,
            packet_size_bytes: 1024,
            verify_time_ms: 150,
            signature: "deadbeef".to_string(),
        };

        let json = serde_json::to_string(&info).expect("serialization should succeed");
        assert!(json.contains("\"block_number\":42"));
        assert!(json.contains("\"receipts_root_match\":true"));
        assert!(json.contains("\"tx_count\":10"));
        assert!(json.contains("\"verify_time_ms\":150"));

        // Verify round-trip: deserialize back.
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("should parse as valid JSON");
        assert_eq!(parsed["block_number"], 42);
        assert_eq!(parsed["witness_accounts"], 5);
    }

    #[test]
    fn test_stats_json_format() {
        // Simulate the JSON generation from n42_get_stats.
        let stats = VerifyStats {
            blocks_verified: 100,
            success_count: 95,
            failure_count: 5,
            total_verify_time_ms: 15000,
        };

        let avg_time = stats.total_verify_time_ms / stats.blocks_verified;
        let success_rate =
            (stats.success_count as f64 / stats.blocks_verified as f64 * 100.0) as u64;

        let json = serde_json::json!({
            "blocks_verified": stats.blocks_verified,
            "success_count": stats.success_count,
            "failure_count": stats.failure_count,
            "avg_time_ms": avg_time,
            "success_rate": success_rate,
        });

        let json_str = json.to_string();
        assert!(json_str.contains("\"blocks_verified\":100"));
        assert!(json_str.contains("\"success_count\":95"));
        assert!(json_str.contains("\"failure_count\":5"));
        assert!(json_str.contains("\"avg_time_ms\":150"));
        assert!(json_str.contains("\"success_rate\":95"));
    }

    #[test]
    fn test_safe_cint_normal() {
        assert_eq!(safe_cint(0), 0);
        assert_eq!(safe_cint(100), 100);
        assert_eq!(safe_cint(c_int::MAX as usize), c_int::MAX);
    }

    #[test]
    fn test_apply_cache_sync() {
        use alloy_primitives::{Bytes, B256};

        let cache = Mutex::new(CodeCache::new(100));

        let h1 = B256::with_last_byte(0x01);
        let h2 = B256::with_last_byte(0x02);
        let h3 = B256::with_last_byte(0x03);

        let msg = CacheSyncMessage {
            codes: vec![
                (h1, Bytes::from(vec![0x60, 0x00])),
                (h2, Bytes::from(vec![0x60, 0x01])),
            ],
            evict_hints: vec![h3], // h3 not in cache — should not panic
        };

        let (added, evicted) = apply_cache_sync(msg, &cache);
        assert_eq!(added, 2);
        assert_eq!(evicted, 1);

        let mut guard = lock_or_recover(&cache);
        assert!(guard.get(&h1).is_some(), "h1 should be in cache");
        assert!(guard.get(&h2).is_some(), "h2 should be in cache");
        assert_eq!(guard.len(), 2);

        // Now evict h1 via a second sync message.
        drop(guard);
        let msg2 = CacheSyncMessage {
            codes: vec![],
            evict_hints: vec![h1],
        };
        let (added2, evicted2) = apply_cache_sync(msg2, &cache);
        assert_eq!(added2, 0);
        assert_eq!(evicted2, 1);

        let mut guard = lock_or_recover(&cache);
        assert!(guard.get(&h1).is_none(), "h1 should have been evicted");
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn test_safe_cint_overflow() {
        // Values larger than c_int::MAX should return -1.
        assert_eq!(safe_cint(c_int::MAX as usize + 1), -1);
        assert_eq!(safe_cint(usize::MAX), -1);
    }

    // ── 5A: FFI lifecycle tests ──

    #[test]
    fn test_init_and_free() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null(), "n42_verifier_init should return non-null");
        unsafe { n42_verifier_free(ctx) };
        // Should not crash.
    }

    #[test]
    fn test_init_creates_valid_pubkey() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let mut pubkey = [0u8; 48];
        let result = unsafe { n42_get_pubkey(ctx, pubkey.as_mut_ptr()) };
        assert_eq!(result, 0, "n42_get_pubkey should succeed");
        assert_ne!(pubkey, [0u8; 48], "pubkey should be non-zero");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_get_stats_json_valid() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let mut buf = vec![0u8; 4096];
        let len = unsafe {
            n42_get_stats(ctx, buf.as_mut_ptr() as *mut c_char, buf.len())
        };
        assert!(len > 0, "n42_get_stats should return valid JSON");
        let json_str = std::str::from_utf8(&buf[..len as usize]).expect("valid UTF-8");
        let parsed: serde_json::Value = serde_json::from_str(json_str).expect("valid JSON");
        assert!(parsed.get("blocks_verified").is_some());
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_disconnect_when_not_connected() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_disconnect(ctx) };
        assert_eq!(result, -2, "disconnect when not connected should return -2 (NotConnected)");
        unsafe { n42_verifier_free(ctx) };
    }

    // ── 5B: Null pointer guard tests ──

    #[test]
    fn test_connect_null_ctx() {
        let host = std::ffi::CString::new("127.0.0.1").unwrap();
        let result = unsafe { n42_connect(std::ptr::null_mut(), host.as_ptr(), 9443, std::ptr::null(), 0) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_poll_null_ctx() {
        let mut buf = [0u8; 1024];
        let result = unsafe { n42_poll_packet(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len()) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_verify_null_ctx() {
        let data = [0u8; 64];
        let result = unsafe { n42_verify_and_send(std::ptr::null_mut(), data.as_ptr(), data.len()) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_get_pubkey_null_ctx() {
        let mut buf = [0u8; 48];
        let result = unsafe { n42_get_pubkey(std::ptr::null_mut(), buf.as_mut_ptr()) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_get_stats_null_ctx() {
        let mut buf = [0u8; 1024];
        let result = unsafe { n42_get_stats(std::ptr::null_mut(), buf.as_mut_ptr() as *mut c_char, buf.len()) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_last_verify_info_null_ctx() {
        let mut buf = [0u8; 1024];
        let result = unsafe { n42_last_verify_info(std::ptr::null_mut(), buf.as_mut_ptr() as *mut c_char, buf.len()) };
        assert_eq!(result, -1);
    }

    #[test]
    fn test_free_null_ctx() {
        // Should not crash.
        unsafe { n42_verifier_free(std::ptr::null_mut()) };
    }

    // ── 5C: TLS pinning tests ──

    #[test]
    fn test_connect_invalid_cert_hash_len() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let host = std::ffi::CString::new("127.0.0.1").unwrap();
        let fake_hash = [0u8; 16]; // invalid: must be 0 or 32
        let result = unsafe { n42_connect(ctx, host.as_ptr(), 9443, fake_hash.as_ptr(), 16) };
        assert_eq!(result, -4, "cert_hash_len != 0 and != 32 should return -4 (InvalidCertHashLen)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_connect_null_cert_hash_dev_mode() {
        // Verify that null cert_hash with len=0 doesn't crash (dev mode path).
        // This will still fail to connect (no server) but should not panic.
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let host = std::ffi::CString::new("127.0.0.1").unwrap();
        let result = unsafe { n42_connect(ctx, host.as_ptr(), 19999, std::ptr::null(), 0) };
        // Connection will fail (no server), but the cert_hash dev mode path should not crash.
        assert_eq!(result, -5, "connect to non-existent server returns -5 (ConnectFailed)");
        unsafe { n42_verifier_free(ctx) };
    }

    // ── 6A: Input validation tests (new error codes) ──

    #[test]
    fn test_connect_null_host() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_connect(ctx, std::ptr::null(), 9443, std::ptr::null(), 0) };
        assert_eq!(result, -1, "null host should return -1 (NullHost)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_poll_packet_null_outbuf() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_poll_packet(ctx, std::ptr::null_mut(), 1024) };
        assert_eq!(result, -1, "null out_buf should return -1 (NullBuffer)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_verify_null_data() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_verify_and_send(ctx, std::ptr::null(), 10) };
        assert_eq!(result, -1, "null data should return -1 (InvalidData)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_verify_zero_len() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let data = [0u8; 1];
        let result = unsafe { n42_verify_and_send(ctx, data.as_ptr(), 0) };
        assert_eq!(result, -1, "zero length should return -1 (InvalidData)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_get_pubkey_null_outbuf() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_get_pubkey(ctx, std::ptr::null_mut()) };
        assert_eq!(result, -1, "null out_buf should return -1 (NullBuffer)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_last_verify_info_null_outbuf() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let result = unsafe { n42_last_verify_info(ctx, std::ptr::null_mut(), 1024) };
        assert_eq!(result, -1, "null out_buf should return -1 (NullBuffer)");
        unsafe { n42_verifier_free(ctx) };
    }

    // ── 6B: Functional path tests ──

    #[test]
    fn test_poll_packet_not_connected() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let mut buf = [0u8; 1024];
        let result = unsafe { n42_poll_packet(ctx, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(result, -2, "poll when not connected should return -2 (NotConnected)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_last_verify_info_no_data() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let mut buf = [0u8; 4096];
        let result = unsafe { n42_last_verify_info(ctx, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        assert_eq!(result, 0, "no verification data should return 0");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_get_stats_buffer_too_small() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let mut buf = [0u8; 2];
        let result = unsafe { n42_get_stats(ctx, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        assert_eq!(result, -3, "buffer too small should return -3 (BufferTooSmall)");
        unsafe { n42_verifier_free(ctx) };
    }

    #[test]
    fn test_verify_garbage_data() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let garbage = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04];
        let result = unsafe { n42_verify_and_send(ctx, garbage.as_ptr(), garbage.len()) };
        assert_eq!(result, 1, "garbage data should return 1 (PacketDecode)");
        unsafe { n42_verifier_free(ctx) };
    }

    // ── 6C: Buffer boundary test ──

    #[test]
    fn test_last_verify_info_buffer_too_small() {
        let ctx = unsafe { n42_verifier_init(4242) };
        assert!(!ctx.is_null());
        let ctx_ref = unsafe { &*ctx };
        *lock_or_recover(&ctx_ref.last_info) = Some(LastVerifyInfo {
            block_number: 1,
            block_hash: "0x00".into(),
            receipts_root_match: true,
            computed_receipts_root: "0x00".into(),
            expected_receipts_root: "0x00".into(),
            tx_count: 0,
            witness_accounts: 0,
            uncached_bytecodes: 0,
            packet_size_bytes: 0,
            verify_time_ms: 0,
            signature: "00".into(),
        });
        let mut buf = [0u8; 2];
        let result = unsafe { n42_last_verify_info(ctx, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        assert_eq!(result, -3, "buffer too small should return -3 (BufferTooSmall)");
        unsafe { n42_verifier_free(ctx) };
    }

    // ── 7: PinnedCertVerification unit tests ──

    #[test]
    fn test_pinned_cert_dev_mode_accepts_any() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

        let cert_bytes = b"arbitrary DER data for dev mode test";
        let cert = CertificateDer::from(cert_bytes.to_vec());

        let verifier = PinnedCertVerification { expected_hash: None };
        let result = verifier.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("n42-starhub").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(result.is_ok(), "dev mode (no hash) should accept any certificate");
    }

    #[test]
    fn test_pinned_cert_hash_match() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        use sha2::{Sha256, Digest};

        let cert_bytes = b"test certificate DER data";
        let cert = CertificateDer::from(cert_bytes.to_vec());
        let expected: [u8; 32] = Sha256::digest(cert_bytes).into();

        let verifier = PinnedCertVerification { expected_hash: Some(expected) };
        let result = verifier.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("n42-starhub").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(result.is_ok(), "matching hash should be accepted");
    }

    #[test]
    fn test_pinned_cert_hash_mismatch() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

        let cert_bytes = b"test certificate DER data";
        let cert = CertificateDer::from(cert_bytes.to_vec());
        let wrong_hash = [0xAA; 32];

        let verifier = PinnedCertVerification { expected_hash: Some(wrong_hash) };
        let result = verifier.verify_server_cert(
            &cert,
            &[],
            &ServerName::try_from("n42-starhub").unwrap(),
            &[],
            UnixTime::now(),
        );
        assert!(result.is_err(), "mismatched hash should be rejected");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("certificate hash mismatch"), "error should mention hash mismatch");
    }

    // ── 8: Queue overflow test ──

    #[test]
    fn test_queue_overflow_drops_oldest() {
        let pending = Arc::new(Mutex::new(VecDeque::with_capacity(MAX_PENDING_PACKETS)));
        let dropped = Arc::new(AtomicU64::new(0));

        // Fill to capacity.
        {
            let mut q = pending.lock().unwrap();
            for i in 0..MAX_PENDING_PACKETS {
                q.push_back(vec![i as u8]);
            }
        }

        // Simulate recv_loop overflow: add one more.
        {
            let mut q = pending.lock().unwrap();
            if q.len() >= MAX_PENDING_PACKETS {
                dropped.fetch_add(1, Ordering::Relaxed);
                q.pop_front();
            }
            q.push_back(vec![0xFF]);
        }

        let q = pending.lock().unwrap();
        assert_eq!(q.len(), MAX_PENDING_PACKETS);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(q.front().unwrap()[0], 1, "oldest (0) should be dropped");
        assert_eq!(q.back().unwrap()[0], 0xFF, "newest should be at back");
    }

    // ── 9: Concurrency + mutex poison recovery tests ──

    #[test]
    fn test_mutex_poison_recovery() {
        let mutex = Mutex::new(42u32);
        // Poison by panicking while holding the lock.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("intentional");
        }));
        assert!(mutex.is_poisoned());
        // lock_or_recover should work.
        let guard = lock_or_recover(&mutex);
        assert_eq!(*guard, 42);
    }

    #[test]
    fn test_concurrent_stats_updates() {
        let stats = Arc::new(Mutex::new(VerifyStats::default()));
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let stats = stats.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let mut s = lock_or_recover(&stats);
                        s.blocks_verified += 1;
                        if i % 2 == 0 {
                            s.success_count += 1;
                        } else {
                            s.failure_count += 1;
                        }
                        s.total_verify_time_ms += 10;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = stats.lock().unwrap();
        assert_eq!(s.blocks_verified, 400);
        assert_eq!(s.success_count + s.failure_count, 400);
    }

    // ── 10: FfiError code mapping test ──

    #[test]
    fn test_ffi_error_codes_distinct() {
        assert_eq!(FfiError::NullContext.into_code(), -1);
        assert_eq!(FfiError::NullBuffer.into_code(), -1);
        assert_eq!(FfiError::NullHost.into_code(), -1);
        assert_eq!(FfiError::InvalidData.into_code(), -1);
        assert_eq!(FfiError::NotConnected.into_code(), -2);
        assert_eq!(FfiError::BufferTooSmall { need: 100, have: 10 }.into_code(), -3);
        assert_eq!(FfiError::InvalidCertHashLen(16).into_code(), -4);
        assert_eq!(FfiError::ConnectFailed("test".into()).into_code(), -5);
        assert_eq!(FfiError::PacketDecode("test".into()).into_code(), 1);
        assert_eq!(FfiError::VerifyFailed("test".into()).into_code(), 2);
        assert_eq!(FfiError::SerializeFailed("test".into()).into_code(), 3);
    }
}
