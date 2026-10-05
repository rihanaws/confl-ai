use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use crate::error::{ExchangeError, Result};

const NONCE_LEN: usize = 12;

/// AES-256-GCM key for encrypting exchange credentials at rest. Decryption
/// happens just-in-time inside route handlers only — never in `db.rs`,
/// never in this crate's callers' persistence layer.
pub struct EncryptionKey(Key<Aes256Gcm>);

impl EncryptionKey {
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(*Key::<Aes256Gcm>::from_slice(bytes))
    }

    /// Encrypts `plaintext`, binding `aad` (e.g. account_id) so a ciphertext
    /// cannot be replayed against a different account. Draws a fresh random
    /// nonce per call — GCM confidentiality/authenticity requires the
    /// (key, nonce) pair never repeat, so credential rotations must not
    /// reuse a deterministic nonce under the same key. Output layout:
    /// `nonce (12 bytes) || ciphertext+tag`.
    pub fn encrypt_fresh(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng).into();
        self.encrypt(plaintext, aad, nonce)
    }

    /// Lower-level variant taking an explicit nonce. Only for tests that
    /// need deterministic output — production callers must use
    /// `encrypt_fresh` so nonces are never reused across encryptions under
    /// the same key.
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8], nonce: [u8; NONCE_LEN]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(&self.0);
        let n = Nonce::from_slice(&nonce);
        let ct = cipher
            .encrypt(n, Payload { msg: plaintext, aad })
            .map_err(|e| ExchangeError::EncryptionError(e.to_string()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn decrypt(&self, blob: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < NONCE_LEN {
            return Err(ExchangeError::EncryptionError("ciphertext too short".into()));
        }
        let (nonce, ct) = blob.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(&self.0);
        let n = Nonce::from_slice(nonce);
        cipher
            .decrypt(n, Payload { msg: ct, aad })
            .map_err(|e| ExchangeError::EncryptionError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_recovers_plaintext() {
        let key = EncryptionKey::from_bytes(&[7u8; 32]);
        let aad = b"account-123";
        let nonce = [1u8; NONCE_LEN];
        let ct = key.encrypt(b"api-secret-value", aad, nonce).unwrap();
        assert!(!ct.windows(b"api-secret-value".len()).any(|w| w == b"api-secret-value"));
        let pt = key.decrypt(&ct, aad).unwrap();
        assert_eq!(pt, b"api-secret-value");
    }

    #[test]
    fn wrong_aad_fails_to_decrypt() {
        let key = EncryptionKey::from_bytes(&[7u8; 32]);
        let nonce = [2u8; NONCE_LEN];
        let ct = key.encrypt(b"secret", b"account-1", nonce).unwrap();
        assert!(key.decrypt(&ct, b"account-2").is_err());
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let key = EncryptionKey::from_bytes(&[7u8; 32]);
        let nonce = [3u8; NONCE_LEN];
        let mut ct = key.encrypt(b"secret", b"acct", nonce).unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0xFF;
        assert!(key.decrypt(&ct, b"acct").is_err());
    }
}
