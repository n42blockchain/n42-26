use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use n42_primitives::BlsSecretKey;
use tracing::{debug, info};

use crate::receipt::VerificationReceipt;

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
    /// Connects to a StarHub server and performs the BLS pubkey handshake.
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
    ///
    /// Returns a `ReceivedMessage` enum. The caller is responsible for handling
    /// both Packet and CacheSync messages.
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
            0x03 => {
                let decompressed = zstd::bulk::decompress(payload, 16 * 1024 * 1024)
                    .map_err(|e| eyre::eyre!("zstd decompress failed: {e}"))?;
                Ok(ReceivedMessage::Packet(decompressed))
            }
            0x02 => Ok(ReceivedMessage::CacheSync(payload.to_vec())),
            0x04 => {
                let decompressed = zstd::bulk::decompress(payload, 16 * 1024 * 1024)
                    .map_err(|e| eyre::eyre!("zstd cache sync decompress failed: {e}"))?;
                Ok(ReceivedMessage::CacheSync(decompressed))
            }
            other => Err(eyre::eyre!("unknown message type: 0x{:02x}", other)),
        }
    }

    /// Receives a single VerificationPacket, skipping CacheSync messages.
    ///
    /// The `timeout` applies as an overall deadline: if no VerificationPacket arrives
    /// within `timeout` (even if CacheSync messages keep arriving), this returns an error.
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

    /// Sends a signed VerificationReceipt back to the node via QUIC.
    pub async fn send_receipt(&self, receipt: &VerificationReceipt) -> eyre::Result<()> {
        let data = bincode::serialize(receipt)
            .map_err(|e| eyre::eyre!("receipt serialization failed: {e}"))?;

        let mut send = self.conn.open_uni().await?;
        send.write_all(&data).await?;
        send.finish()?;

        Ok(())
    }

    /// Returns the BLS secret key.
    pub fn bls_key(&self) -> &BlsSecretKey {
        &self.bls_key
    }

    /// Returns the 48-byte BLS public key.
    pub fn pubkey(&self) -> &[u8; 48] {
        &self.pubkey
    }

    /// Returns true if the QUIC connection is closed.
    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    /// Closes the QUIC connection gracefully.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"client shutdown");
    }
}

/// Custom certificate verifier that accepts any self-signed certificate.
/// StarHub uses rcgen-generated self-signed certs.
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
