use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use n42_primitives::BlsSecretKey;
use tracing::{debug, info};

use crate::receipt::VerificationReceipt;

/// Max decompressed size for zstd payloads (16 MB).
const ZSTD_MAX_SIZE: usize = 16 * 1024 * 1024;

fn zstd_decompress(payload: &[u8]) -> eyre::Result<Vec<u8>> {
    zstd::bulk::decompress(payload, ZSTD_MAX_SIZE)
        .map_err(|e| eyre::eyre!("zstd decompress failed: {e}"))
}

/// Message received from a StarHub QUIC stream.
pub enum ReceivedMessage {
    /// A VerificationPacket (decompressed if zstd).
    Packet(Vec<u8>),
    /// A CacheSyncMessage (decompressed if zstd).
    CacheSync(Vec<u8>),
}

/// QUIC-based mobile verification client.
///
/// Connects to a StarHub server, performs the BLS pubkey handshake,
/// then receives VerificationPackets and sends back signed receipts.
pub struct QuicMobileClient {
    conn: quinn::Connection,
    bls_key: BlsSecretKey,
    pubkey: [u8; 48],
}

impl QuicMobileClient {
    pub async fn connect(addr: SocketAddr, bls_key: BlsSecretKey) -> eyre::Result<Self> {
        let pubkey = bls_key.public_key().to_bytes();

        // Build TLS config that accepts self-signed certificates
        let mut client_crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();
        client_crypto.alpn_protocols = vec![b"n42-mobile/1".to_vec()];

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
                .map_err(|e| eyre::eyre!("QUIC client config error: {e}"))?,
        ));

        // Match StarHub's idle timeout (300s) and enable keep-alive to prevent
        // the connection from dying during the initial period before blocks start.
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(Duration::from_secs(300))
                .map_err(|e| eyre::eyre!("idle timeout config error: {e}"))?,
        ));
        transport.keep_alive_interval(Some(Duration::from_secs(10)));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config);

        let connection = endpoint.connect(addr, "n42-mobile")?;
        let conn = connection.await?;

        info!(
            remote = %conn.remote_address(),
            "QUIC connection established"
        );

        // Handshake: send 48-byte BLS public key via uni stream
        let mut send = conn.open_uni().await?;
        send.write_all(&pubkey).await?;
        send.finish()?;

        debug!("BLS pubkey handshake sent");

        Ok(Self {
            conn,
            bls_key,
            pubkey,
        })
    }

    /// Receives the next message (Packet or CacheSync) from the QUIC connection.
    pub async fn receive_message(&self, timeout: Duration) -> eyre::Result<ReceivedMessage> {
        let mut recv = tokio::time::timeout(timeout, self.conn.accept_uni())
            .await
            .map_err(|_| eyre::eyre!("timeout waiting for uni stream"))??;

        let data = recv.read_to_end(10 * 1024 * 1024).await?; // 10MB max

        if data.is_empty() {
            return Err(eyre::eyre!("received empty stream"));
        }

        let msg_type = data[0];
        let payload = &data[1..];

        match msg_type {
            0x01 => Ok(ReceivedMessage::Packet(payload.to_vec())),
            0x03 => Ok(ReceivedMessage::Packet(zstd_decompress(payload)?)),
            0x02 => Ok(ReceivedMessage::CacheSync(payload.to_vec())),
            0x04 => Ok(ReceivedMessage::CacheSync(zstd_decompress(payload)?)),
            other => Err(eyre::eyre!("unknown message type: 0x{:02x}", other)),
        }
    }

    /// Receives a single VerificationPacket, skipping CacheSync messages.
    /// The `timeout` applies as an overall deadline across all interim CacheSync messages.
    pub async fn receive_packet(&self, timeout: Duration) -> eyre::Result<Vec<u8>> {
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(eyre::eyre!("timeout waiting for verification packet"));
            }

            match self.receive_message(remaining).await? {
                ReceivedMessage::Packet(data) => return Ok(data),
                ReceivedMessage::CacheSync(_) => {
                    debug!("received cache sync message, skipping");
                    continue;
                }
            }
        }
    }

    pub async fn send_receipt(&self, receipt: &VerificationReceipt) -> eyre::Result<()> {
        let data = bincode::serialize(receipt)
            .map_err(|e| eyre::eyre!("receipt serialization failed: {e}"))?;

        let mut send = self.conn.open_uni().await?;
        send.write_all(&data).await?;
        send.finish()?;

        Ok(())
    }

    pub fn bls_key(&self) -> &BlsSecretKey {
        &self.bls_key
    }

    pub fn pubkey(&self) -> &[u8; 48] {
        &self.pubkey
    }

    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"client shutdown");
    }
}

/// Accepts any certificate (StarHub uses self-signed certs).
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
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
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}
