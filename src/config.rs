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

/// Colour scheme of the window.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the Windows setting.
    #[default]
    System,
    Dark,
    Light,
}

impl Theme {
    pub fn label(self) -> &'static str {
        match self {
            Theme::System => "как в системе",
            Theme::Dark => "тёмная",
            Theme::Light => "светлая",
        }
    }

    pub fn variants() -> [Theme; 3] {
        [Theme::System, Theme::Dark, Theme::Light]
    }
}

/// Everything a preset remembers about *how* to launch: the destination, the
/// window mode and the per-window limits. Which accounts take part is stored
/// next to it, in the preset itself.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct LaunchSettings {
    pub place_id: String,
    pub private_link: String,
    pub multi_instance: bool,
    pub launch_gap_secs: u32,
    pub max_windows: u32,
    pub fps_cap: u32,
    pub graphics_quality: u32,
    pub texture_quality: Option<u32>,
    pub trim_memory: bool,
    pub window_width: u32,
    pub window_height: u32,
    pub tile_windows: bool,
    pub no_grass: bool,
    pub gray_sky: bool,
    pub pause_voxelizer: bool,
    pub low_csg: bool,
    pub msaa: u32,
}

impl Default for LaunchSettings {
    fn default() -> Self {
        Config::default().launch_settings()
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
    pub theme: Theme,
    /// Check every cookie right after the store is opened, to see which
    /// accounts have been signed out.
    pub check_on_unlock: bool,
    /// Ask Roblox who is online / in a game every minute. Off by default: it is
    /// one more request per minute that the manager would not otherwise make.
    pub live_status: bool,
    /// Largest number of client windows open at once, 0 for no limit. A batch
    /// waits for a free slot instead of starting more.
    pub max_windows: u32,
    /// Start an account again when its window disappears.
    pub auto_relaunch: bool,
    /// Close an account's old window before launching it again.
    pub close_previous: bool,
    /// Send a harmless input to background windows so they are not kicked for
    /// idling. Off by default: it is automation, see `antiafk.rs`.
    pub anti_afk: bool,
    /// Minutes between inputs; the client kicks at 20.
    pub anti_afk_minutes: u32,
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
            theme: Theme::System,
            // On: signed-out accounts are what people want to hear about first.
            check_on_unlock: true,
            live_status: false,
            max_windows: 0,
            auto_relaunch: false,
            // On: Windows would not keep two windows of one account in one game
            // anyway, the second one throws the first out.
            close_previous: true,
            anti_afk: false,
            anti_afk_minutes: crate::antiafk::DEFAULT_MINUTES,
        }
    }
}

pub fn path() -> PathBuf {
    store::app_dir().join("config.json")
}

impl Config {
    /// The launch-related part of the settings, for saving into a preset.
    pub fn launch_settings(&self) -> LaunchSettings {
        LaunchSettings {
            place_id: self.place_id.clone(),
            private_link: self.private_link.clone(),
            multi_instance: self.multi_instance,
            launch_gap_secs: self.launch_gap_secs,
            max_windows: self.max_windows,
            fps_cap: self.fps_cap,
            graphics_quality: self.graphics_quality,
            texture_quality: self.texture_quality,
            trim_memory: self.trim_memory,
            window_width: self.window_width,
            window_height: self.window_height,
            tile_windows: self.tile_windows,
            no_grass: self.no_grass,
            gray_sky: self.gray_sky,
            pause_voxelizer: self.pause_voxelizer,
            low_csg: self.low_csg,
            msaa: self.msaa,
        }
    }

    /// Put a preset's launch settings back. Everything else stays as it is.
    pub fn apply_launch_settings(&mut self, settings: &LaunchSettings) {
        self.place_id = settings.place_id.clone();
        self.private_link = settings.private_link.clone();
        self.multi_instance = settings.multi_instance;
        self.launch_gap_secs = settings.launch_gap_secs;
        self.max_windows = settings.max_windows;
        self.fps_cap = settings.fps_cap;
        self.graphics_quality = settings.graphics_quality;
        self.texture_quality = settings.texture_quality;
        self.trim_memory = settings.trim_memory;
        self.window_width = settings.window_width;
        self.window_height = settings.window_height;
        self.tile_windows = settings.tile_windows;
        self.no_grass = settings.no_grass;
        self.gray_sky = settings.gray_sky;
        self.pause_voxelizer = settings.pause_voxelizer;
        self.low_csg = settings.low_csg;
        self.msaa = settings.msaa;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_settings_survive_a_round_trip() {
        let config = Config {
            place_id: "1818".into(),
            private_link: "d1e7b1e3-129e-404f-8882-d29d26c72dcd".into(),
            multi_instance: false,
            max_windows: 4,
            fps_cap: 30,
            texture_quality: Some(1),
            no_grass: true,
            msaa: 1,
            ..Config::default()
        };

        let saved = config.launch_settings();
        let mut other = Config::default();
        other.apply_launch_settings(&saved);
        assert_eq!(other.launch_settings(), saved);
        assert_eq!(other.place_id, "1818");
        assert_eq!(other.max_windows, 4);
        assert_eq!(other.texture_quality, Some(1));
        assert!(!other.multi_instance);
    }

    #[test]
    fn applying_a_preset_leaves_unrelated_settings_alone() {
        let mut config = Config {
            theme: Theme::Light,
            auto_lock_minutes: 5,
            ..Config::default()
        };
        config.apply_launch_settings(&LaunchSettings {
            fps_cap: 20,
            ..LaunchSettings::default()
        });
        assert_eq!(config.theme, Theme::Light);
        assert_eq!(config.auto_lock_minutes, 5);
        assert_eq!(config.fps_cap, 20);
    }

    #[test]
    fn an_old_config_file_gets_defaults_for_new_fields() {
        let json = br#"{"place_id":"1818","multi_instance":true,"auto_lock_minutes":15}"#;
        let config: Config = serde_json::from_slice(json).expect("old config");
        assert_eq!(config.place_id, "1818");
        assert_eq!(config.theme, Theme::System);
        assert!(config.check_on_unlock);
        assert!(config.close_previous);
        assert!(!config.live_status);
        assert!(!config.auto_relaunch);
        assert!(!config.anti_afk);
        assert_eq!(config.anti_afk_minutes, crate::antiafk::DEFAULT_MINUTES);
        assert_eq!(config.max_windows, 0);
    }
}
