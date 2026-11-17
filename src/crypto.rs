//! Storage encryption: Argon2id (KDF) + AES-256-GCM.
//!
//! File layout:
//!   "RBM1" | salt[16] | nonce[12] | ciphertext
//!
//! The password is never stored anywhere: without it the file stays locked.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use rand::RngCore;

pub const MAGIC: &[u8; 4] = b"RBM1";
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

/// Decrypt the file. Returns (plaintext, salt, key).
pub fn decrypt(
    password: &str,
    blob: &[u8],
) -> Result<(Vec<u8>, [u8; SALT_LEN], [u8; KEY_LEN]), String> {
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
