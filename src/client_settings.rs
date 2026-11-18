//! Roblox client overrides: FPS cap, graphics quality and texture quality.
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

    map
}

/// Write the overrides next to the client executable.
/// Returns the file path when the file was actually touched.
pub fn apply(
    client_exe: &Path,
    fps: u32,
    quality: u32,
    texture: Option<u32>,
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
    if fps == 0 && quality == 0 && texture.is_none() && current.is_none() {
        return Ok(None);
    }

    let merged = merge(current.as_ref(), fps, quality, texture);
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
        let merged = merge(None, 60, 3, None);
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("60"));
        assert_eq!(text(&merged, FRAME_TIME_KEY).as_deref(), Some("17"));
        assert_eq!(text(&merged, QUALITY_KEY).as_deref(), Some("3"));
    }

    #[test]
    fn texture_quality_needs_both_keys() {
        // The level alone does nothing: the override has to be enabled.
        let merged = merge(None, 0, 0, Some(1));
        assert_eq!(text(&merged, TEXTURE_ENABLED_KEY).as_deref(), Some("True"));
        assert_eq!(text(&merged, TEXTURE_KEY).as_deref(), Some("1"));

        // Level zero is a real level, not "off".
        let lowest = merge(None, 0, 0, Some(0));
        assert_eq!(text(&lowest, TEXTURE_KEY).as_deref(), Some("0"));
    }

    #[test]
    fn no_texture_choice_removes_both_keys() {
        let existing = serde_json::json!({ TEXTURE_ENABLED_KEY: "True", TEXTURE_KEY: "2" });
        let merged = merge(Some(&existing), 0, 0, None);
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
        let merged = merge(Some(&existing), 30, 0, None);

        assert_eq!(text(&merged, "FFlagSomething").as_deref(), Some("True"));
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("30"));
        assert!(!merged.contains_key(QUALITY_KEY));
    }

    #[test]
    fn zero_removes_previous_limits() {
        let existing = serde_json::json!({ FPS_KEY: "30", FRAME_TIME_KEY: "33", QUALITY_KEY: "1" });
        let merged = merge(Some(&existing), 0, 0, None);
        assert!(merged.is_empty());
    }

    #[test]
    fn garbage_previous_file_is_replaced() {
        let merged = merge(None, 45, 0, None);
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("45"));
    }
}
