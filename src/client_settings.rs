//! Roblox client overrides: FPS cap and graphics quality.
//!
//! The client reads `ClientSettings/ClientAppSettings.json` from its own version
//! folder at startup. Values there are FFlag **strings**, not numbers — that is
//! how every tool writes them, including Bloxstrap.
//! Roblox updates replace that folder, so the file is rewritten before each launch.
//!
//! Since 2026 the client applies only flags listed in
//! `clientsettingscdn.roblox.com/v2/settings/application/PCDesktopClient`, and
//! neither key below is on that list, so current clients ignore them. The file
//! is still written: older clients and builds with the allowlist disabled read
//! it, and the real load limiter is the job-object CPU cap in [`crate::throttle`].

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const FPS_KEY: &str = "DFIntTaskSchedulerTargetFps";
pub const QUALITY_KEY: &str = "DFIntDebugFRMQualityLevelOverride";

/// Merge our overrides into the existing settings, keeping foreign keys intact.
/// A zero value removes the key, so "off" really means "game default".
pub fn merge(existing: Option<&Value>, fps: u32, quality: u32) -> Map<String, Value> {
    let mut map = existing
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if fps > 0 {
        map.insert(FPS_KEY.to_string(), Value::String(fps.to_string()));
    } else {
        map.remove(FPS_KEY);
    }

    if quality > 0 {
        map.insert(QUALITY_KEY.to_string(), Value::String(quality.to_string()));
    } else {
        map.remove(QUALITY_KEY);
    }

    map
}

/// Write the overrides next to the client executable.
/// Returns the file path when the file was actually touched.
pub fn apply(client_exe: &Path, fps: u32, quality: u32) -> Result<Option<PathBuf>, String> {
    let folder = client_exe
        .parent()
        .ok_or_else(|| "Не понял, где лежит клиент Roblox".to_string())?
        .join("ClientSettings");
    let path = folder.join("ClientAppSettings.json");

    let current = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());

    // Without limits and without an existing file there is nothing to do.
    if fps == 0 && quality == 0 && current.is_none() {
        return Ok(None);
    }

    let merged = merge(current.as_ref(), fps, quality);
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
        let merged = merge(None, 60, 3);
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("60"));
        assert_eq!(text(&merged, QUALITY_KEY).as_deref(), Some("3"));
    }

    #[test]
    fn keeps_foreign_keys_from_other_tools() {
        let existing = serde_json::json!({ "FFlagSomething": "True", FPS_KEY: "144" });
        let merged = merge(Some(&existing), 30, 0);

        assert_eq!(text(&merged, "FFlagSomething").as_deref(), Some("True"));
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("30"));
        assert!(!merged.contains_key(QUALITY_KEY));
    }

    #[test]
    fn zero_removes_previous_limits() {
        let existing = serde_json::json!({ FPS_KEY: "30", QUALITY_KEY: "1" });
        let merged = merge(Some(&existing), 0, 0);
        assert!(merged.is_empty());
    }

    #[test]
    fn garbage_previous_file_is_replaced() {
        let merged = merge(None, 45, 0);
        assert_eq!(text(&merged, FPS_KEY).as_deref(), Some("45"));
    }
}
