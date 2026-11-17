//! Local account storage.
//!
//! File: `%APPDATA%\RbxAccountManager\accounts.dat`
//! Encrypted as a whole (see `crypto`), so cookies only appear as plaintext
//! inside the encrypted blob — never on disk as-is.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::crypto;

const SCHEMA: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub username: String,
    pub user_id: u64,
    pub note: String,
    pub cookie: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Payload {
    schema: u32,
    accounts: Vec<Account>,
}

pub struct Store {
    path: PathBuf,
    salt: [u8; crypto::SALT_LEN],
    key: [u8; crypto::KEY_LEN],
    pub accounts: Vec<Account>,
}

impl Store {
    /// `%APPDATA%\RbxAccountManager\accounts.dat`
    pub fn default_path() -> PathBuf {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        base.join("RbxAccountManager").join("accounts.dat")
    }

    /// Whether a store file already exists at this path.
    pub fn exists(path: &PathBuf) -> bool {
        path.exists()
    }

    /// Create a new empty store protected by `password`.
    pub fn create(path: PathBuf, password: &str) -> Result<Self, String> {
        if password.len() < 6 {
            return Err("Пароль должен быть не короче 6 символов".into());
        }
        let salt = crypto::new_salt();
        let key = crypto::derive_key(password, &salt)?;
        let store = Store {
            path,
            salt,
            key,
            accounts: Vec::new(),
        };
        store.save()?;
        Ok(store)
    }

    /// Open an existing store with the master password.
    pub fn unlock(path: PathBuf, password: &str) -> Result<Self, String> {
        let blob = fs::read(&path).map_err(|err| format!("Не могу прочитать файл: {err}"))?;
        let (plaintext, salt, key) = crypto::decrypt(password, &blob)?;

        let payload: Payload = serde_json::from_slice(&plaintext)
            .map_err(|err| format!("Повреждённое содержимое хранилища: {err}"))?;

        Ok(Store {
            path,
            salt,
            key,
            accounts: payload.accounts,
        })
    }

    /// Encrypt the payload and replace the file atomically (tmp + rename).
    pub fn save(&self) -> Result<(), String> {
        let payload = Payload {
            schema: SCHEMA,
            accounts: self.accounts.clone(),
        };
        let json = serde_json::to_vec(&payload).map_err(|err| err.to_string())?;
        let blob = crypto::encrypt(&self.key, &self.salt, &json)?;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("Не могу создать папку: {err}"))?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &blob).map_err(|err| format!("Не могу записать файл: {err}"))?;
        fs::rename(&tmp, &self.path).map_err(|err| format!("Не могу заменить файл: {err}"))?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.id == id)
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|account| account.id == id)
    }

    /// Append an account and return its generated id.
    pub fn add(&mut self, label: &str, cookie: &str, note: &str) -> String {
        let id = format!("{:016x}", rand::random::<u64>());
        self.accounts.push(Account {
            id: id.clone(),
            label: if label.trim().is_empty() {
                "account".to_string()
            } else {
                label.trim().to_string()
            },
            username: String::new(),
            user_id: 0,
            note: note.trim().to_string(),
            cookie: cookie.to_string(),
        });
        id
    }

    pub fn update(
        &mut self,
        id: &str,
        label: Option<&str>,
        note: Option<&str>,
        cookie: Option<&str>,
        username: Option<&str>,
        user_id: Option<u64>,
    ) {
        if let Some(account) = self.find_mut(id) {
            if let Some(value) = label {
                if !value.trim().is_empty() {
                    account.label = value.trim().to_string();
                }
            }
            if let Some(value) = note {
                account.note = value.trim().to_string();
            }
            if let Some(value) = cookie {
                account.cookie = value.to_string();
            }
            if let Some(value) = username {
                account.username = value.to_string();
            }
            if let Some(value) = user_id {
                account.user_id = value;
            }
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.accounts.retain(|account| account.id != id);
    }
}
