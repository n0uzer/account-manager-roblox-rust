//! Storage encryption: Argon2id (KDF) + AES-256-GCM.
//!
//! File layout:
//!   "RBM1" | salt[16] | nonce[12] | ciphertext
//!
//! With DPAPI binding enabled the whole blob gets one extra layer:
//!   "RBMD" | DPAPI("RBM1" | salt | nonce | ciphertext)
//!
//! The password is never stored anywhere: without it the file stays locked.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use rand::RngCore;

pub const MAGIC: &[u8; 4] = b"RBM1";
pub const MAGIC_DPAPI: &[u8; 4] = b"RBMD";
pub const SALT_LEN: usize = 16;
pub const NONCE_LEN: usize = 12;
pub const KEY_LEN: usize = 32;

/// Generate a fresh random salt.
pub fn new_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    salt
}

/// Derive the 256-bit key from the master password.
pub fn derive_key(password: &str, salt: &[u8; SALT_LEN]) -> Result<[u8; KEY_LEN], String> {
    let mut key = [0u8; KEY_LEN];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|err| format!("Argon2: {err}"))?;
    Ok(key)
}

/// Encrypt data: magic + salt + nonce + ciphertext.
pub fn encrypt(
    key: &[u8; KEY_LEN],
    salt: &[u8; SALT_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|err| err.to_string())?;

    let mut nonce = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| "Ошибка шифрования".to_string())?;

    let mut out = Vec::with_capacity(4 + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Plaintext plus everything needed to re-encrypt the file later.
pub type Decrypted = (Vec<u8>, [u8; SALT_LEN], [u8; KEY_LEN]);

/// Decrypt the file. Returns (plaintext, salt, key).
pub fn decrypt(password: &str, blob: &[u8]) -> Result<Decrypted, String> {
    let header = 4 + SALT_LEN + NONCE_LEN;
    if blob.len() <= header {
        return Err("Файл хранилища повреждён или пуст".into());
    }
    if &blob[..4] != MAGIC {
        return Err("Неизвестный формат файла хранилища".into());
    }

    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&blob[4..4 + SALT_LEN]);

    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&blob[4 + SALT_LEN..header]);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|err| err.to_string())?;

    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), &blob[header..])
        .map_err(|_| "Неверный пароль (или файл повреждён)".to_string())?;

    Ok((plaintext, salt, key))
}

/// Add the DPAPI layer, binding the file to the current Windows user.
pub fn dpapi_wrap(blob: &[u8]) -> Result<Vec<u8>, String> {
    let protected = crate::dpapi::protect(blob)?;
    let mut out = Vec::with_capacity(MAGIC_DPAPI.len() + protected.len());
    out.extend_from_slice(MAGIC_DPAPI);
    out.extend_from_slice(&protected);
    Ok(out)
}

/// Strip the DPAPI layer when present. Returns `(inner blob, was_wrapped)`.
pub fn dpapi_unwrap(blob: &[u8]) -> Result<(Vec<u8>, bool), String> {
    if blob.len() > MAGIC_DPAPI.len() && &blob[..MAGIC_DPAPI.len()] == MAGIC_DPAPI {
        let inner = crate::dpapi::unprotect(&blob[MAGIC_DPAPI.len()..])?;
        Ok((inner, true))
    } else {
        Ok((blob.to_vec(), false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_password() {
        let salt = new_salt();
        let key = derive_key("correct horse", &salt).unwrap();
        let blob = encrypt(&key, &salt, b"secret payload").unwrap();

        assert_eq!(&blob[..4], MAGIC);

        let (plaintext, salt2, key2) = decrypt("correct horse", &blob).unwrap();
        assert_eq!(plaintext, b"secret payload");
        assert_eq!(salt, salt2);
        assert_eq!(key, key2);
    }

    #[test]
    fn wrong_password_is_rejected() {
        let salt = new_salt();
        let key = derive_key("right", &salt).unwrap();
        let blob = encrypt(&key, &salt, b"data").unwrap();

        assert!(decrypt("wrong", &blob).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let salt = new_salt();
        let key = derive_key("pw", &salt).unwrap();
        let mut blob = encrypt(&key, &salt, b"data").unwrap();

        let last = blob.len() - 1;
        blob[last] ^= 0xFF;

        assert!(decrypt("pw", &blob).is_err());
    }

    #[test]
    fn foreign_format_is_rejected() {
        assert!(decrypt("pw", b"NOPE0000000000000000000000000000").is_err());
        assert!(decrypt("pw", b"short").is_err());
    }

    #[test]
    fn dpapi_unwrap_passes_plain_blobs_through() {
        let salt = new_salt();
        let key = derive_key("pw", &salt).unwrap();
        let blob = encrypt(&key, &salt, b"data").unwrap();

        let (inner, wrapped) = dpapi_unwrap(&blob).unwrap();
        assert!(!wrapped);
        assert_eq!(inner, blob);
    }
}
