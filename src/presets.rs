//! Launch presets: a named set of accounts plus the launch settings to use with
//! them. One click restores the whole setup; one of the presets may also be run
//! automatically when the store is opened.
//!
//! File: `%APPDATA%\RbxAccountManager\presets.json`. It holds no secrets: accounts
//! are referenced by their local id, never by cookie or name, so the file can be
//! kept in plain text next to `config.json`.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::LaunchSettings;
use crate::store;

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Preset {
    pub name: String,
    /// Local ids of the accounts that take part, in launch order.
    pub accounts: Vec<String>,
    pub settings: LaunchSettings,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
#[serde(default)]
pub struct Presets {
    pub items: Vec<Preset>,
    /// Name of the preset to run right after the store is opened.
    pub autorun: Option<String>,
}

pub fn path() -> PathBuf {
    store::app_dir().join("presets.json")
}

fn same_name(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

impl Presets {
    /// Missing or broken file means no presets rather than an error.
    pub fn load() -> Self {
        Self::load_from(&path())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&path())
    }

    fn load_from(path: &Path) -> Self {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| format!("Не могу создать папку: {err}"))?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|err| err.to_string())?;
        // Same atomic dance as the store: a crash must not leave half a file.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json).map_err(|err| format!("Не могу записать пресеты: {err}"))?;
        fs::rename(&tmp, path).map_err(|err| format!("Не могу заменить файл пресетов: {err}"))
    }

    pub fn find(&self, name: &str) -> Option<&Preset> {
        self.items
            .iter()
            .find(|preset| same_name(&preset.name, name))
    }

    /// Add a preset, or replace the one with the same name (case-insensitive).
    /// An empty name is refused.
    pub fn upsert(&mut self, mut preset: Preset) -> Result<(), String> {
        preset.name = preset.name.trim().to_string();
        if preset.name.is_empty() {
            return Err("Дай пресету название".into());
        }
        match self
            .items
            .iter_mut()
            .find(|existing| same_name(&existing.name, &preset.name))
        {
            Some(existing) => {
                // The autorun pointer follows a rename that only changes case.
                if self
                    .autorun
                    .as_deref()
                    .is_some_and(|name| same_name(name, &preset.name))
                {
                    self.autorun = Some(preset.name.clone());
                }
                *existing = preset;
            }
            None => self.items.push(preset),
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) {
        self.items.retain(|preset| !same_name(&preset.name, name));
        if self
            .autorun
            .as_deref()
            .is_some_and(|autorun| same_name(autorun, name))
        {
            self.autorun = None;
        }
    }

    /// Turn the automatic run on for one preset or off for all.
    pub fn set_autorun(&mut self, name: Option<&str>) {
        self.autorun = name
            .and_then(|name| self.find(name))
            .map(|preset| preset.name.clone());
    }

    pub fn is_autorun(&self, name: &str) -> bool {
        self.autorun
            .as_deref()
            .is_some_and(|autorun| same_name(autorun, name))
    }

    /// Ids of `preset`'s accounts that still exist, in the preset's order.
    pub fn live_accounts<'a>(preset: &'a Preset, known: &[String]) -> Vec<&'a str> {
        preset
            .accounts
            .iter()
            .filter(|id| known.iter().any(|known| known == *id))
            .map(String::as_str)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(name: &str, accounts: &[&str]) -> Preset {
        Preset {
            name: name.into(),
            accounts: accounts.iter().map(|id| id.to_string()).collect(),
            settings: LaunchSettings::default(),
        }
    }

    fn temp_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rbm-presets-{:016x}", rand::random::<u64>()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir.join("presets.json")
    }

    #[test]
    fn upsert_replaces_a_preset_with_the_same_name() {
        let mut presets = Presets::default();
        presets.upsert(preset("Farm", &["a"])).expect("add");
        presets
            .upsert(preset("farm", &["a", "b"]))
            .expect("replace");
        assert_eq!(presets.items.len(), 1);
        assert_eq!(presets.items[0].accounts, vec!["a", "b"]);
    }

    #[test]
    fn an_empty_name_is_refused() {
        let mut presets = Presets::default();
        assert!(presets.upsert(preset("   ", &[])).is_err());
        assert!(presets.items.is_empty());
    }

    #[test]
    fn removing_the_autorun_preset_clears_autorun() {
        let mut presets = Presets::default();
        presets.upsert(preset("Farm", &["a"])).expect("add");
        presets.upsert(preset("Trade", &["b"])).expect("add");
        presets.set_autorun(Some("Farm"));
        assert!(presets.is_autorun("farm"));

        presets.remove("Trade");
        assert!(presets.is_autorun("Farm"));
        presets.remove("FARM");
        assert!(presets.autorun.is_none());
        assert!(presets.items.is_empty());
    }

    #[test]
    fn autorun_needs_an_existing_preset() {
        let mut presets = Presets::default();
        presets.set_autorun(Some("ghost"));
        assert!(presets.autorun.is_none());
    }

    #[test]
    fn deleted_accounts_are_skipped_but_order_is_kept() {
        let item = preset("Farm", &["c", "gone", "a"]);
        let known = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(Presets::live_accounts(&item, &known), vec!["c", "a"]);
    }

    #[test]
    fn file_round_trip_and_tolerance() {
        let path = temp_file();
        let mut presets = Presets::default();
        presets.upsert(preset("Farm", &["a", "b"])).expect("add");
        presets.set_autorun(Some("Farm"));
        presets.save_to(&path).expect("save");
        assert_eq!(Presets::load_from(&path), presets);

        // Garbage and a missing file both mean "no presets".
        fs::write(&path, b"{ not json").expect("write");
        assert_eq!(Presets::load_from(&path), Presets::default());
        assert_eq!(
            Presets::load_from(&path.with_file_name("missing.json")),
            Presets::default()
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_preset_missing_settings_gets_defaults() {
        let json = br#"{"items":[{"name":"Old","accounts":["a"]}]}"#;
        let presets: Presets = serde_json::from_slice(json).expect("parse");
        assert_eq!(presets.items[0].settings, LaunchSettings::default());
    }
}
