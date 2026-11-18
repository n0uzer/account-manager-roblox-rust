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
    /// Link to a private server, empty for a public one. Either the
    /// `?privateServerLinkCode=…` form or the newer `/share?code=…&type=Server`.
    pub private_link: String,
    pub multi_instance: bool,
    /// 0 disables the auto-lock.
    pub auto_lock_minutes: u32,
    pub sort: SortKey,
    /// Target FPS for the client, 0 keeps the game default.
    ///
    /// Written to `ClientAppSettings.json`, but current clients filter the key
    /// out (fast-flag allowlist), so it is informational rather than effective.
    pub fps_cap: u32,
    /// Graphics quality 1..=10, 0 keeps the game default.
    pub graphics_quality: u32,
    /// Texture quality 0..=3, `None` keeps the game default.
    pub texture_quality: Option<u32>,
    /// Give background clients' memory back to the system on a timer.
    pub trim_memory: bool,
    /// Client area size in `--app` mode, 0x0 keeps the default window.
    pub window_width: u32,
    pub window_height: u32,
    /// Spread the client windows over the screen in a grid after each launch.
    pub tile_windows: bool,
    /// Seconds between two launches of a batch. Booting six clients at once
    /// spikes RAM and disk, so the queue is spaced out.
    pub launch_gap_secs: u32,
    /// Extra graphics switches from the client's allowlist.
    pub no_grass: bool,
    pub gray_sky: bool,
    pub pause_voxelizer: bool,
    pub low_csg: bool,
    /// 0 keeps the client default, 1 disables anti-aliasing.
    pub msaa: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            place_id: String::new(),
            private_link: String::new(),
            multi_instance: true,
            auto_lock_minutes: 15,
            sort: SortKey::Added,
            // Off by default: current clients ignore the FPS flag anyway, so a
            // non-zero default would only produce a misleading status note.
            fps_cap: 0,
            graphics_quality: 0,
            texture_quality: None,
            trim_memory: false,
            window_width: 0,
            window_height: 0,
            // On by default: several windows overlapping each other is the
            // main reason to run them in the first place.
            tile_windows: true,
            // Two seconds is enough for the client to grab its window without
            // six of them loading a place at the same moment.
            launch_gap_secs: 2,
            no_grass: false,
            gray_sky: false,
            pause_voxelizer: false,
            low_csg: false,
            msaa: 0,
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
