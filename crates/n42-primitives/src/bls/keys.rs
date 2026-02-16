use alloy_primitives::hex;
use alloy_primitives::B256;
use blst::min_pk::{PublicKey, SecretKey, Signature};
use blst::BLST_ERROR;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

#[derive(Debug, Error)]
pub enum BlsError {
    #[error("BLS key generation failed")]
    KeyGeneration,
    #[error("BLS signing failed")]
    SigningFailed,
    #[error("BLS verification failed: {0:?}")]
    VerificationFailed(BLST_ERROR),
    #[error("invalid public key bytes")]
    InvalidPublicKey,
    #[error("invalid signature bytes")]
    InvalidSignature,
    #[error("invalid secret key bytes")]
    InvalidSecretKey,
}

#[derive(Clone)]
pub struct BlsSecretKey(SecretKey);

impl BlsSecretKey {
    pub fn random() -> Result<Self, BlsError> {
        let mut ikm = [0u8; 32];
        getrandom::fill(&mut ikm).map_err(|_| BlsError::KeyGeneration)?;
        let sk = SecretKey::key_gen(&ikm, &[]).map_err(|_| BlsError::KeyGeneration)?;
        Ok(Self(sk))
    }

    pub fn from_bytes(bytes: &[u8; 32]) -> Result<Self, BlsError> {
        let sk = SecretKey::from_bytes(bytes).map_err(|_| BlsError::InvalidSecretKey)?;
        Ok(Self(sk))
    }

    /// Derives a BLS secret key from input keying material (IKM) using the
    /// standard BLS key-generation algorithm (hash-to-scalar). This always
    /// produces a valid key, unlike `from_bytes` which may reject raw bytes
    /// that exceed the BLS12-381 curve order.
    pub fn key_gen(ikm: &[u8; 32]) -> Result<Self, BlsError> {
        let sk = SecretKey::key_gen(ikm, &[]).map_err(|_| BlsError::KeyGeneration)?;
        Ok(Self(sk))
    }

    pub fn public_key(&self) -> BlsPublicKey {
        BlsPublicKey(self.0.sk_to_pk())
    }

    pub fn sign(&self, message: &[u8]) -> BlsSignature {
        BlsSignature(self.0.sign(message, DST, &[]))
    }

    pub fn sign_hash(&self, hash: &B256) -> BlsSignature {
        self.sign(hash.as_slice())
    }
}

impl std::fmt::Debug for BlsSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlsSecretKey")
            .field("public_key", &self.public_key())
            .finish()
    }
}

#[derive(Clone)]
pub struct BlsPublicKey(PublicKey);

impl BlsPublicKey {
    pub fn from_bytes(bytes: &[u8; 48]) -> Result<Self, BlsError> {
        let pk = PublicKey::from_bytes(bytes).map_err(|_| BlsError::InvalidPublicKey)?;
        Ok(Self(pk))
    }

    pub fn to_bytes(&self) -> [u8; 48] {
        self.0.to_bytes()
    }

    pub fn verify(&self, message: &[u8], signature: &BlsSignature) -> Result<(), BlsError> {
        let result = signature.0.verify(true, message, DST, &[], &self.0, true);
        if result != BLST_ERROR::BLST_SUCCESS {
            return Err(BlsError::VerificationFailed(result));
        }
        Ok(())
    }

    pub(crate) fn inner(&self) -> &PublicKey {
        &self.0
    }
}

impl std::fmt::Debug for BlsPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.to_bytes();
        write!(f, "BlsPublicKey(0x{}..)", hex::encode(&bytes[..8]))
    }
}

impl PartialEq for BlsPublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes() == other.to_bytes()
    }
}

impl Eq for BlsPublicKey {}

impl std::hash::Hash for BlsPublicKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.to_bytes().hash(state);
    }
}

impl Serialize for BlsPublicKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.to_bytes())
    }
}

impl<'de> Deserialize<'de> for BlsPublicKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() != 48 {
            return Err(serde::de::Error::custom("expected 48 bytes for BLS public key"));
        }
        let mut arr = [0u8; 48];
        arr.copy_from_slice(&bytes);
        Self::from_bytes(&arr).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
pub struct BlsSignature(pub(crate) Signature);

impl BlsSignature {
    pub fn from_bytes(bytes: &[u8; 96]) -> Result<Self, BlsError> {
        let sig = Signature::from_bytes(bytes).map_err(|_| BlsError::InvalidSignature)?;
        Ok(Self(sig))
    }

    pub fn to_bytes(&self) -> [u8; 96] {
        self.0.to_bytes()
    }

    pub(crate) fn inner(&self) -> &Signature {
        &self.0
    }
}

impl std::fmt::Debug for BlsSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.to_bytes();
        write!(f, "BlsSignature(0x{}..)", hex::encode(&bytes[..8]))
    }
}

impl PartialEq for BlsSignature {
    fn eq(&self, other: &Self) -> bool {
        self.to_bytes() == other.to_bytes()
    }
}

impl Eq for BlsSignature {}

impl std::hash::Hash for BlsSignature {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.to_bytes().hash(state);
    }
}

impl Serialize for BlsSignature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.to_bytes())
    }
}

impl<'de> Deserialize<'de> for BlsSignature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = Deserialize::deserialize(deserializer)?;
        if bytes.len() != 96 {
            return Err(serde::de::Error::custom("expected 96 bytes for BLS signature"));
        }
        let mut arr = [0u8; 96];
        arr.copy_from_slice(&bytes);
        Self::from_bytes(&arr).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    #[test]
    fn test_key_generation() {
        let sk = BlsSecretKey::random().expect("key generation should succeed");
        let pk = sk.public_key();
        // Public key bytes should be 48 bytes and not all zeros.
        let pk_bytes = pk.to_bytes();
        assert_eq!(pk_bytes.len(), 48);
        assert!(pk_bytes.iter().any(|&b| b != 0), "public key should not be all zeros");
    }

    #[test]
    fn test_sign_and_verify() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();
        let message = b"hello world";

        let sig = sk.sign(message);
        pk.verify(message, &sig).expect("verification should succeed for correct message");
    }

    #[test]
    fn test_verify_wrong_message() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let sig = sk.sign(b"message one");
        let result = pk.verify(b"message two", &sig);
        assert!(result.is_err(), "verification should fail for wrong message");
    }

    #[test]
    fn test_verify_wrong_key() {
        let sk1 = BlsSecretKey::random().unwrap();
        let sk2 = BlsSecretKey::random().unwrap();
        let pk2 = sk2.public_key();
        let message = b"test message";

        let sig = sk1.sign(message);
        let result = pk2.verify(message, &sig);
        assert!(result.is_err(), "verification should fail with wrong public key");
    }

    #[test]
    fn test_key_serialization_roundtrip() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let encoded = bincode::serialize(&pk).expect("serialize should succeed");
        let decoded: BlsPublicKey =
            bincode::deserialize(&encoded).expect("deserialize should succeed");
        assert_eq!(pk, decoded, "public key should survive bincode roundtrip");
    }

    #[test]
    fn test_signature_serialization_roundtrip() {
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"roundtrip test");

        let encoded = bincode::serialize(&sig).expect("serialize should succeed");
        let decoded: BlsSignature =
            bincode::deserialize(&encoded).expect("deserialize should succeed");
        assert_eq!(sig, decoded, "signature should survive bincode roundtrip");
    }

    #[test]
    fn test_public_key_from_bytes_roundtrip() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();

        let bytes = pk.to_bytes();
        let pk2 = BlsPublicKey::from_bytes(&bytes).expect("from_bytes should succeed");
        assert_eq!(pk, pk2, "public key should survive to_bytes/from_bytes roundtrip");
    }

    #[test]
    fn test_signature_from_bytes_roundtrip() {
        let sk = BlsSecretKey::random().unwrap();
        let sig = sk.sign(b"bytes roundtrip");

        let bytes = sig.to_bytes();
        let sig2 = BlsSignature::from_bytes(&bytes).expect("from_bytes should succeed");
        assert_eq!(sig, sig2, "signature should survive to_bytes/from_bytes roundtrip");
    }

    #[test]
    fn test_sign_hash() {
        let sk = BlsSecretKey::random().unwrap();
        let pk = sk.public_key();
        let hash = B256::from([0xab; 32]);

        let sig = sk.sign_hash(&hash);
        // sign_hash delegates to sign(hash.as_slice()), so verifying against the
        // raw slice should succeed.
        pk.verify(hash.as_slice(), &sig)
            .expect("sign_hash result should verify against hash bytes");
    }
}
