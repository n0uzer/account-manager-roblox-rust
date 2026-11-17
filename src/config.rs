//! Plain-text settings. No secrets live here, so no encryption is needed.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::store;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    Added,
    Name,
    Username,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Added => "по добавлению",
            SortKey::Name => "по названию",
            SortKey::Username => "по нику",
        }
    }

    pub fn variants() -> [SortKey; 3] {
        [SortKey::Added, SortKey::Name, SortKey::Username]
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub place_id: String,
    pub multi_instance: bool,
    /// 0 disables the auto-lock.
    pub auto_lock_minutes: u32,
    pub sort: SortKey,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            place_id: String::new(),
            multi_instance: true,
            auto_lock_minutes: 15,
            sort: SortKey::Added,
        }
    }
}

pub fn path() -> PathBuf {
    store::app_dir().join("config.json")
}

impl Config {
    /// Missing or broken config falls back to defaults instead of failing.
    pub fn load() -> Self {
        fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        let Some(parent) = path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = fs::write(&path, json);
        }
    }
}
