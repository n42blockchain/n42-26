use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use rand::RngCore;
use scrypt::{Params as ScryptParams, scrypt};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Scrypt-AES-GCM encrypted keystore for BLS validator private keys.
///
/// Format inspired by Ethereum web3 keystore v3 but simplified:
/// - KDF: scrypt (N=8192, r=8, p=1 — balanced for server use)
/// - Cipher: AES-256-GCM (authenticated encryption)
///
/// The keystore file is a JSON document containing the encrypted key
/// material, salt, nonce, and scrypt parameters for reproducibility.
#[derive(Serialize, Deserialize)]
pub struct Keystore {
    /// Scrypt salt (32 bytes, hex-encoded).
    pub salt: String,
    /// AES-GCM nonce (12 bytes, hex-encoded).
    pub nonce: String,
    /// Encrypted BLS secret key (32 bytes plaintext → 48 bytes ciphertext+tag, hex-encoded).
    pub ciphertext: String,
    /// Scrypt log2(N) parameter for reproducibility.
    pub scrypt_log_n: u8,
    /// Scrypt r parameter.
    pub scrypt_r: u32,
    /// Scrypt p parameter.
    pub scrypt_p: u32,
}

impl Keystore {
    /// Encrypts a 32-byte BLS secret key with the given password.
    pub fn encrypt(secret_key_bytes: &[u8; 32], password: &str) -> Result<Self, String> {
        let mut salt = [0u8; 32];
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut salt);
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        // Derive 32-byte AES key from password using scrypt.
        let scrypt_log_n: u8 = 13; // N=8192
        let scrypt_r: u32 = 8;
        let scrypt_p: u32 = 1;
        let params = ScryptParams::new(scrypt_log_n, scrypt_r, scrypt_p, 32)
            .map_err(|e| format!("scrypt params: {e}"))?;

        let mut derived_key = [0u8; 32];
        scrypt(password.as_bytes(), &salt, &params, &mut derived_key)
            .map_err(|e| format!("scrypt KDF: {e}"))?;

        // Encrypt with AES-256-GCM.
        let cipher = Aes256Gcm::new_from_slice(&derived_key)
            .map_err(|e| format!("AES init: {e}"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, secret_key_bytes.as_ref())
            .map_err(|e| format!("AES encrypt: {e}"))?;

        Ok(Self {
            salt: hex::encode(salt),
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(ciphertext),
            scrypt_log_n,
            scrypt_r,
            scrypt_p,
        })
    }

    /// Decrypts the keystore with the given password, returning the 32-byte BLS secret key.
    pub fn decrypt(&self, password: &str) -> Result<[u8; 32], String> {
        let salt = hex::decode(&self.salt)
            .map_err(|e| format!("invalid salt hex: {e}"))?;
        let nonce_bytes = hex::decode(&self.nonce)
            .map_err(|e| format!("invalid nonce hex: {e}"))?;
        let ciphertext = hex::decode(&self.ciphertext)
            .map_err(|e| format!("invalid ciphertext hex: {e}"))?;

        // Derive the same AES key from password.
        let params = ScryptParams::new(self.scrypt_log_n, self.scrypt_r, self.scrypt_p, 32)
            .map_err(|e| format!("scrypt params: {e}"))?;
        let mut derived_key = [0u8; 32];
        scrypt(password.as_bytes(), &salt, &params, &mut derived_key)
            .map_err(|e| format!("scrypt KDF: {e}"))?;

        // Decrypt with AES-256-GCM.
        let cipher = Aes256Gcm::new_from_slice(&derived_key)
            .map_err(|e| format!("AES init: {e}"))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = cipher.decrypt(nonce, ciphertext.as_ref())
            .map_err(|_| "decryption failed: wrong password or corrupted keystore".to_string())?;

        plaintext.try_into()
            .map_err(|v: Vec<u8>| format!("decrypted key is {} bytes, expected 32", v.len()))
    }

    /// Saves the keystore to a JSON file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("serialize keystore: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| format!("write keystore to {}: {e}", path.display()))
    }

    /// Loads a keystore from a JSON file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let json = std::fs::read_to_string(path)
            .map_err(|e| format!("read keystore {}: {e}", path.display()))?;
        serde_json::from_str(&json)
            .map_err(|e| format!("parse keystore {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let secret = [0xABu8; 32];
        let password = "test-password-123";

        let keystore = Keystore::encrypt(&secret, password)
            .expect("encryption should succeed");

        let recovered = keystore.decrypt(password)
            .expect("decryption should succeed");

        assert_eq!(recovered, secret, "decrypted key should match original");
    }

    #[test]
    fn test_wrong_password_fails() {
        let secret = [0xCDu8; 32];
        let keystore = Keystore::encrypt(&secret, "correct-password")
            .expect("encryption should succeed");

        let result = keystore.decrypt("wrong-password");
        assert!(result.is_err(), "decryption with wrong password should fail");
    }

    #[test]
    fn test_save_load_roundtrip() {
        let secret = [0xEFu8; 32];
        let password = "file-test";

        let keystore = Keystore::encrypt(&secret, password)
            .expect("encryption should succeed");

        let dir = std::env::temp_dir().join("n42-keystore-test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test-keystore.json");

        keystore.save(&path).expect("save should succeed");
        let loaded = Keystore::load(&path).expect("load should succeed");
        let recovered = loaded.decrypt(password).expect("decrypt should succeed");

        assert_eq!(recovered, secret);

        // Cleanup
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }
}
