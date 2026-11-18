//! Roblox client overrides: FPS cap, graphics quality, textures and the extra
//! allowlisted switches that cost frames and memory.
//!
//! The client reads `ClientSettings/ClientAppSettings.json` from its own version
//! folder at startup. Values there are FFlag **strings**, not numbers — that is
//! how every tool writes them, including Bloxstrap.
//! Roblox updates replace that folder, so the file is rewritten before each launch.
//!
//! Since 2026 the client filters local overrides through a list of its own.
//! Measured on a live client: `DFIntTaskSchedulerTargetFps`, the Frame Rate
//! Manager frame budget, every DRS and render-scale key and the MSAA override are
//! denied (`Denied local configuration for: …` in the client log), while the
//! quality, texture-quality and texture-quality-enabled keys are accepted. The
//! FPS cap itself lives in the client's own settings file, see
//! [`crate::game_settings`].
//!
//! The keys in [`Extras`] come from the same allowlist, so a denied key would
//! only be ignored, never break the launch.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const FPS_KEY: &str = "DFIntTaskSchedulerTargetFps";
/// Frame Rate Manager: target frame time in milliseconds. Denied by current
/// clients, kept for older ones.
pub const FRAME_TIME_KEY: &str = "DFIntFRMConstantFrameTimeTargetMs";
pub const QUALITY_KEY: &str = "DFIntDebugFRMQualityLevelOverride";
/// Turns the texture override on; the level itself is in [`TEXTURE_KEY`].
pub const TEXTURE_ENABLED_KEY: &str = "DFFlagTextureQualityOverrideEnabled";
/// Texture quality level 0..=3, lower means smaller textures and less memory.
pub const TEXTURE_KEY: &str = "DFIntTextureQualityOverride";

/// Grass distances; zero removes the blades entirely.
pub const GRASS_MIN_KEY: &str = "FIntFRMMinGrassDistance";
pub const GRASS_MAX_KEY: &str = "FIntFRMMaxGrassDistance";
/// Flat grey sky instead of the skybox: fewer textures per window.
pub const SKY_GRAY_KEY: &str = "FFlagDebugSkyGray";
/// Freezes voxelization of the terrain lightmap, the heaviest idle CPU work.
pub const PAUSE_VOXELIZER_KEY: &str = "DFFlagDebugPauseVoxelizer";
/// CSG is the "union/negate" geometry many places are built from. These four
/// distances decide how far away its detailed mesh is drawn.
pub const CSG_KEYS: [&str; 4] = [
    "DFIntCSGLevelOfDetailSwitchingDistance",
    "DFIntCSGLevelOfDetailSwitchingDistanceL12",
    "DFIntCSGLevelOfDetailSwitchingDistanceL23",
    "DFIntCSGLevelOfDetailSwitchingDistanceL34",
];
/// Multisample anti-aliasing; 1 disables it, 2/4/8 raise it.
pub const MSAA_KEY: &str = "FIntDebugForceMSAASamples";

/// Extra switches from the client's own allowlist.
///
/// Every one of them is off by default: they change how the game looks, so the
/// user has to ask for it. The names match the toolbar labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extras {
    /// No grass at all — noticeable on outdoor maps.
    pub no_grass: bool,
    /// Grey sky instead of the skybox.
    pub gray_sky: bool,
    /// Freeze terrain voxelization (no moving shadows from the sun).
    pub pause_voxelizer: bool,
    /// Draw CSG unions in their cheapest level of detail.
    pub low_csg: bool,
    /// 0 keeps the client default, otherwise 1 (off), 2, 4 or 8 samples.
    pub msaa: u32,
}

impl Extras {
    /// True when the user asked for anything, i.e. the file has to be written.
    pub fn any(&self) -> bool {
        self.no_grass || self.gray_sky || self.pause_voxelizer || self.low_csg || self.msaa > 0
    }

    /// Short line for the status bar, empty when nothing is set.
    pub fn note(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.no_grass {
            parts.push("без травы".into());
        }
        if self.gray_sky {
            parts.push("серое небо".into());
        }
        if self.pause_voxelizer {
            parts.push("без вокселизации".into());
        }
        if self.low_csg {
            parts.push("CSG низкий".into());
        }
        if self.msaa > 0 {
            parts.push(if self.msaa == 1 {
                "без сглаживания".to_string()
            } else {
                format!("MSAA x{}", self.msaa)
            });
        }
        parts.join(", ")
    }
}

/// Frame budget for a target FPS, rounded to the nearest millisecond. 30 fps
/// becomes 33 ms, 60 fps becomes 17 ms.
fn frame_time_ms(fps: u32) -> u32 {
    ((1000 + fps / 2) / fps).max(1)
}

/// Merge our overrides into the existing settings, keeping foreign keys intact.
/// A zero value removes the key, so "off" really means "game default".
pub fn merge(
    existing: Option<&Value>,
    fps: u32,
    quality: u32,
    texture: Option<u32>,
    extras: Extras,
) -> Map<String, Value> {
    let mut map = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if fps > 0 {
        map.insert(FPS_KEY.to_string(), Value::String(fps.to_string()));
        map.insert(
            FRAME_TIME_KEY.to_string(),
            Value::String(frame_time_ms(fps).to_string()),
        );
    } else {
        map.remove(FPS_KEY);
        map.remove(FRAME_TIME_KEY);
    }

    if quality > 0 {
        map.insert(QUALITY_KEY.to_string(), Value::String(quality.to_string()));
    } else {
        map.remove(QUALITY_KEY);
    }

    match texture {
        Some(level) => {
            map.insert(
                TEXTURE_ENABLED_KEY.to_string(),
                Value::String("True".to_string()),
            );
            map.insert(TEXTURE_KEY.to_string(), Value::String(level.to_string()));
        }
        None => {
            map.remove(TEXTURE_ENABLED_KEY);
            map.remove(TEXTURE_KEY);
        }
    }

    // Extras are plain switches: present means on, absent means the client's own
    // default. Removing them when off keeps a previous run from leaking through.
    // `FFlag`/`DFFlag` take "True", the `FInt`/`DFInt` keys take a number.
    let mut set = |key: &str, value: Option<&str>| match value {
        Some(value) => {
            map.insert(key.to_string(), Value::String(value.to_string()));
        }
        None => {
            map.remove(key);
        }
    };
    // Grass distance zero removes the blades instead of shortening them.
    set(GRASS_MIN_KEY, extras.no_grass.then_some("0"));
    set(GRASS_MAX_KEY, extras.no_grass.then_some("0"));
    set(SKY_GRAY_KEY, extras.gray_sky.then_some("True"));
    set(
        PAUSE_VOXELIZER_KEY,
        extras.pause_voxelizer.then_some("True"),
    );
    // CSG at distance 0 means "always the cheapest mesh".
    for key in CSG_KEYS {
        set(key, extras.low_csg.then_some("0"));
    }

    if extras.msaa > 0 {
        set(MSAA_KEY, Some(&extras.msaa.to_string()));
    } else {
        set(MSAA_KEY, None);
    }

    map
}

/// Write the overrides next to the client executable.
/// Returns the file path when the file was actually touched.
pub fn apply(
    client_exe: &Path,
    fps: u32,
    quality: u32,
    texture: Option<u32>,
    extras: Extras,
) -> Result<Option<PathBuf>, String> {
    let folder = client_exe
        .parent()
        .ok_or_else(|| "Не понял, где лежит клиент Roblox".to_string())?
        .join("ClientSettings");
    let path = folder.join("ClientAppSettings.json");

    let current = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());

    // Without limits and without an existing file there is nothing to do.
    if fps == 0 && quality == 0 && texture.is_none() && !extras.any() && current.is_none() {
        return Ok(None);
    }

    let merged = merge(current.as_ref(), fps, quality, texture, extras);
    fs::create_dir_all(&folder).map_err(|err| format!("не могу создать ClientSettings: {err}"))?;
    let json = serde_json::to_vec_pretty(&Value::Object(merged))
        .map_err(|err| format!("не могу собрать настройки клиента: {err}"))?;
    fs::write(&path, json).map_err(|err| format!("не могу записать настройки клиента: {err}"))?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
        map.get(key).and_then(Value::as_str).map(str::to_string)
    }

    #[test]
    fn writes_limits_as_strings() {
        let merged = merge(None, 60, 3, None, Extras::default());
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("60"));
        assert_eq!(text(&merged, FRAME_TIME_KEY).as_deref(), Some("17"));
        assert_eq!(text(&merged, QUALITY_KEY).as_deref(), Some("3"));
    }

    #[test]
    fn extras_are_off_unless_asked_for() {
        let merged = merge(None, 0, 0, None, Extras::default());
        assert!(merged.is_empty());
        assert!(!Extras::default().any());
    }

    #[test]
    fn grass_is_removed_with_two_zero_distances() {
        let extras = Extras {
            no_grass: true,
            ..Extras::default()
        };
        let merged = merge(None, 0, 0, None, extras);
        assert_eq!(text(&merged, GRASS_MIN_KEY).as_deref(), Some("0"));
        assert_eq!(text(&merged, GRASS_MAX_KEY).as_deref(), Some("0"));
    }

    #[test]
    fn switches_are_the_string_true() {
        let extras = Extras {
            gray_sky: true,
            pause_voxelizer: true,
            low_csg: true,
            ..Extras::default()
        };
        let merged = merge(None, 0, 0, None, extras);
        assert_eq!(text(&merged, SKY_GRAY_KEY).as_deref(), Some("True"));
        assert_eq!(text(&merged, PAUSE_VOXELIZER_KEY).as_deref(), Some("True"));
        for key in CSG_KEYS {
            assert_eq!(text(&merged, key).as_deref(), Some("0"), "{key}");
        }
    }

    #[test]
    fn msaa_one_means_off_not_default() {
        let off = merge(
            None,
            0,
            0,
            None,
            Extras {
                msaa: 1,
                ..Extras::default()
            },
        );
        assert_eq!(text(&off, MSAA_KEY).as_deref(), Some("1"));

        let default = merge(None, 0, 0, None, Extras::default());
        assert!(!default.contains_key(MSAA_KEY));
    }

    #[test]
    fn extras_can_be_switched_off_again() {
        let existing = serde_json::json!({
            GRASS_MIN_KEY: "0", GRASS_MAX_KEY: "0", SKY_GRAY_KEY: "True", MSAA_KEY: "4"
        });
        let merged = merge(Some(&existing), 0, 0, None, Extras::default());
        assert!(!merged.contains_key(GRASS_MIN_KEY));
        assert!(!merged.contains_key(SKY_GRAY_KEY));
        assert!(!merged.contains_key(MSAA_KEY));
    }

    #[test]
    fn texture_quality_needs_both_keys() {
        // The level alone does nothing: the override has to be enabled.
        let merged = merge(None, 0, 0, Some(1), Extras::default());
        assert_eq!(text(&merged, TEXTURE_ENABLED_KEY).as_deref(), Some("True"));
        assert_eq!(text(&merged, TEXTURE_KEY).as_deref(), Some("1"));

        // Level zero is a real level, not "off".
        let lowest = merge(None, 0, 0, Some(0), Extras::default());
        assert_eq!(text(&lowest, TEXTURE_KEY).as_deref(), Some("0"));
    }

    #[test]
    fn no_texture_choice_removes_both_keys() {
        let existing = serde_json::json!({ TEXTURE_ENABLED_KEY: "True", TEXTURE_KEY: "2" });
        let merged = merge(Some(&existing), 0, 0, None, Extras::default());
        assert!(!merged.contains_key(TEXTURE_ENABLED_KEY));
        assert!(!merged.contains_key(TEXTURE_KEY));
    }

    #[test]
    fn fps_becomes_a_frame_budget() {
        // The FRM key takes a frame time, not a frame rate.
        assert_eq!(frame_time_ms(30), 33);
        assert_eq!(frame_time_ms(60), 17);
        assert_eq!(frame_time_ms(20), 50);
        assert_eq!(frame_time_ms(144), 7);
        assert_eq!(frame_time_ms(1), 1000);
        assert_eq!(frame_time_ms(1000), 1);
    }

    #[test]
    fn keeps_foreign_keys_from_other_tools() {
        let existing = serde_json::json!({ "FFlagSomething": "True", FPS_KEY: "144" });
        let merged = merge(Some(&existing), 30, 0, None, Extras::default());

        assert_eq!(text(&merged, "FFlagSomething").as_deref(), Some("True"));
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("30"));
        assert!(!merged.contains_key(QUALITY_KEY));
    }

    #[test]
    fn zero_removes_previous_limits() {
        let existing = serde_json::json!({ FPS_KEY: "30", FRAME_TIME_KEY: "33", QUALITY_KEY: "1" });
        let merged = merge(Some(&existing), 0, 0, None, Extras::default());
        assert!(merged.is_empty());
    }

    #[test]
    fn garbage_previous_file_is_replaced() {
        let merged = merge(None, 45, 0, None, Extras::default());
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("45"));
    }
}
