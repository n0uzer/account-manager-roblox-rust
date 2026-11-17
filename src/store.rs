//! Local account storage.
//!
//! File: `%APPDATA%\RbxAccountManager\accounts.dat`
//! Encrypted as a whole (see `crypto`), so cookies only appear as plaintext
//! inside the encrypted blob — never on disk as-is.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

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
    /// Unix timestamp (seconds) of when the account was added.
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct Payload {
    schema: u32,
    accounts: Vec<Account>,
}

/// Decrypted contents plus the material needed to re-save the file.
type StoreContents = (
    Vec<Account>,
    [u8; crypto::SALT_LEN],
    [u8; crypto::KEY_LEN],
    bool,
);

pub struct Store {
    path: PathBuf,
    salt: [u8; crypto::SALT_LEN],
    key: [u8; crypto::KEY_LEN],
    /// When true the file is additionally wrapped with Windows DPAPI.
    pub dpapi: bool,
    pub accounts: Vec<Account>,
}

/// `%APPDATA%\RbxAccountManager`
pub fn app_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("RbxAccountManager")
}

fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    /// `%APPDATA%\RbxAccountManager\accounts.dat`
    pub fn default_path() -> PathBuf {
        app_dir().join("accounts.dat")
    }

    /// Whether a store file already exists at this path.
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Create a new empty store protected by `password`.
    pub fn create(path: PathBuf, password: &str, dpapi: bool) -> Result<Self, String> {
        if password.len() < 6 {
            return Err("Пароль должен быть не короче 6 символов".into());
        }
        let salt = crypto::new_salt();
        let key = crypto::derive_key(password, &salt)?;
        let store = Store {
            path,
            salt,
            key,
            dpapi,
            accounts: Vec::new(),
        };
        store.save()?;
        Ok(store)
    }

    /// Open an existing store with the master password.
    pub fn unlock(path: PathBuf, password: &str) -> Result<Self, String> {
        let (accounts, salt, key, dpapi) = Self::read(&path, password)?;
        Ok(Store {
            path,
            salt,
            key,
            dpapi,
            accounts,
        })
    }

    /// Decrypt any store file and return its contents.
    fn read(path: &Path, password: &str) -> Result<StoreContents, String> {
        let raw = fs::read(path).map_err(|err| format!("Не могу прочитать файл: {err}"))?;
        let (blob, dpapi) = crypto::dpapi_unwrap(&raw)?;
        let (plaintext, salt, key) = crypto::decrypt(password, &blob)?;

        let payload: Payload = serde_json::from_slice(&plaintext)
            .map_err(|err| format!("Повреждённое содержимое хранилища: {err}"))?;

        Ok((payload.accounts, salt, key, dpapi))
    }

    /// Encrypt the payload and replace the file atomically (tmp + rename).
    pub fn save(&self) -> Result<(), String> {
        let payload = Payload {
            schema: SCHEMA,
            accounts: self.accounts.clone(),
        };
        let json = serde_json::to_vec(&payload).map_err(|err| err.to_string())?;
        let encrypted = crypto::encrypt(&self.key, &self.salt, &json)?;
        let blob = if self.dpapi {
            crypto::dpapi_wrap(&encrypted)?
        } else {
            encrypted
        };

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("Не могу создать папку: {err}"))?;
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &blob).map_err(|err| format!("Не могу записать файл: {err}"))?;
        fs::rename(&tmp, &self.path).map_err(|err| format!("Не могу заменить файл: {err}"))?;
        Ok(())
    }

    /// Turn the DPAPI binding on or off and rewrite the file.
    pub fn set_dpapi(&mut self, enabled: bool) -> Result<(), String> {
        let previous = self.dpapi;
        self.dpapi = enabled;
        if let Err(err) = self.save() {
            self.dpapi = previous;
            return Err(err);
        }
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
        let id = new_id();
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
            created_at: now_unix(),
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

    /// Add accounts from an imported file, skipping duplicates by cookie.
    /// Returns how many were actually added.
    pub fn merge(&mut self, accounts: Vec<Account>) -> usize {
        let mut added = 0;
        for mut account in accounts {
            let duplicate = !account.cookie.is_empty()
                && self
                    .accounts
                    .iter()
                    .any(|existing| existing.cookie == account.cookie);
            if duplicate {
                continue;
            }
            // Ids are local, so a fresh one avoids clashes with the current file.
            account.id = new_id();
            if account.created_at == 0 {
                account.created_at = now_unix();
            }
            self.accounts.push(account);
            added += 1;
        }
        added
    }
}

/// Read accounts from another store file (used by import).
pub fn read_accounts(path: &Path, password: &str) -> Result<Vec<Account>, String> {
    Store::read(path, password).map(|(accounts, _, _, _)| accounts)
}

impl Drop for Store {
    fn drop(&mut self) {
        // Wipe key material and cookies when the store is locked.
        self.key.zeroize();
        self.salt.zeroize();
        for account in &mut self.accounts {
            account.cookie.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rbm-test-{:016x}", rand::random::<u64>()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir.join(name)
    }

    #[test]
    fn create_and_reopen_keeps_accounts() {
        let path = temp_path("accounts.dat");
        let mut store = Store::create(path.clone(), "secret123", false).expect("create");
        store.add("main", "cookie-value", "note");
        store.save().expect("save");
        drop(store);

        let reopened = Store::unlock(path.clone(), "secret123").expect("unlock");
        assert_eq!(reopened.accounts.len(), 1);
        assert_eq!(reopened.accounts[0].cookie, "cookie-value");
        assert!(!reopened.dpapi);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn wrong_password_cannot_open() {
        let path = temp_path("accounts.dat");
        let store = Store::create(path.clone(), "secret123", false).expect("create");
        drop(store);

        assert!(Store::unlock(path.clone(), "secret124").is_err());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn short_password_is_refused() {
        let path = temp_path("accounts.dat");
        assert!(Store::create(path, "123", false).is_err());
    }

    #[test]
    fn merge_skips_duplicate_cookies() {
        let path = temp_path("accounts.dat");
        let mut store = Store::create(path.clone(), "secret123", false).expect("create");
        store.add("first", "same-cookie", "");
        store.save().expect("save");

        let mut imported = store.accounts.clone();
        imported.push(Account {
            id: "other".into(),
            label: "second".into(),
            username: String::new(),
            user_id: 0,
            note: String::new(),
            cookie: "different-cookie".into(),
            created_at: 0,
        });

        let added = store.merge(imported);
        assert_eq!(added, 1);
        assert_eq!(store.accounts.len(), 2);

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn imported_file_is_readable() {
        let path = temp_path("export.dat");
        let mut store = Store::create(path.clone(), "secret123", false).expect("create");
        store.add("main", "cookie-value", "");
        store.save().expect("save");

        let accounts = read_accounts(&path, "secret123").expect("read");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label, "main");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
