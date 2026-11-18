//! Roblox's own user settings: the frame rate cap and the window size.
//!
//! Fast flags are filtered by the client, but `GlobalBasicSettings_13.xml` is
//! the file behind the in-game settings menu, and the client reads it at start:
//!
//! ```xml
//! <int name="FramerateCap">240</int>
//! <Vector2 name="StartScreenSize"><X>816</X><Y>638</Y></Vector2>
//! ```
//!
//! Writing those two is the only way to get a real FPS cap and a real start
//! size: the client ignores the matching fast flags (`Denied local
//! configuration` in its log), while these values are its own settings.
//!
//! The file belongs to the Windows user, not to an account or a window, so all
//! windows of this user share the values. It is rewritten before every launch
//! because a client writes it again when it exits.

use std::fs;
use std::path::{Path, PathBuf};

/// Where the client keeps its settings. `None` when `LOCALAPPDATA` is unset.
pub fn path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        PathBuf::from(local)
            .join("Roblox")
            .join("GlobalBasicSettings_13.xml"),
    )
}

/// Apply the limits to the client settings.
///
/// `fps == 0` and `width == 0` both mean "leave it alone". Returns `false` when
/// there was nothing to change — including the case when the client has never
/// run and the file does not exist yet, which is not an error.
pub fn apply(fps: u32, width: u32, height: u32) -> Result<bool, String> {
    if fps == 0 && (width == 0 || height == 0) {
        return Ok(false);
    }
    let Some(path) = path() else {
        return Ok(false);
    };
    apply_at(&path, fps, width, height)
}

fn apply_at(path: &Path, fps: u32, width: u32, height: u32) -> Result<bool, String> {
    let Ok(xml) = fs::read_to_string(path) else {
        // The client has never run on this machine: there is nothing to patch.
        return Ok(false);
    };

    let mut patched = xml.clone();
    if fps > 0 {
        if let Some(next) = replace(&patched, "int", "FramerateCap", &fps.to_string()) {
            patched = next;
        }
    }
    if width > 0 && height > 0 {
        if let Some(next) = replace_screen_size(&patched, width, height) {
            patched = next;
        }
        // A fullscreen or maximized window ignores the requested client area.
        if let Some(next) = replace(&patched, "bool", "Fullscreen", "false") {
            patched = next;
        }
        if let Some(next) = replace(&patched, "bool", "StartMaximized", "false") {
            patched = next;
        }
    }

    if patched == xml {
        return Ok(false);
    }
    fs::write(path, patched).map_err(|err| format!("не могу записать настройки клиента: {err}"))?;
    Ok(true)
}

/// Replace the body of `<tag name="name">…</tag>`, keeping the rest of the file
/// byte for byte.
fn replace(xml: &str, tag: &str, name: &str, value: &str) -> Option<String> {
    let open = format!("<{tag} name=\"{name}\">");
    let body_start = xml.find(&open)? + open.len();
    let body_end = body_start + xml[body_start..].find(&format!("</{tag}>"))?;

    let mut out = String::with_capacity(xml.len());
    out.push_str(&xml[..body_start]);
    out.push_str(value);
    out.push_str(&xml[body_end..]);
    Some(out)
}

/// Replace the start window size. The element holds two child tags, so it is
/// rewritten as a whole instead of by value.
fn replace_screen_size(xml: &str, width: u32, height: u32) -> Option<String> {
    let open = "<Vector2 name=\"StartScreenSize\">";
    let start = xml.find(open)?;
    let body_start = start + open.len();
    let body_end = body_start + xml[body_start..].find("</Vector2>")?;

    let mut out = String::with_capacity(xml.len());
    out.push_str(&xml[..body_start]);
    out.push_str(&format!("<X>{width}</X><Y>{height}</Y>"));
    out.push_str(&xml[body_end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed copy of the real file: same tags, same indentation.
    const SAMPLE: &str = "<roblox version=\"4\">\n\
        \t<Item class=\"UserGameSettings\">\n\
        \t\t<Properties>\n\
        \t\t\t<int name=\"FramerateCap\">240</int>\n\
        \t\t\t<bool name=\"Fullscreen\">true</bool>\n\
        \t\t\t<int name=\"GraphicsQualityLevel\">3</int>\n\
        \t\t\t<bool name=\"StartMaximized\">false</bool>\n\
        \t\t\t<Vector2 name=\"StartScreenPosition\">\n\
        \t\t\t\t<X>567</X>\n\
        \t\t\t\t<Y>307</Y>\n\
        \t\t\t</Vector2>\n\
        \t\t\t<Vector2 name=\"StartScreenSize\">\n\
        \t\t\t\t<X>816</X>\n\
        \t\t\t\t<Y>638</Y>\n\
        \t\t\t</Vector2>\n\
        \t\t</Properties>\n\
        \t</Item>\n\
        </roblox>\n";

    fn patched(fps: u32, width: u32, height: u32) -> String {
        let mut xml = SAMPLE.to_string();
        if fps > 0 {
            xml = replace(&xml, "int", "FramerateCap", &fps.to_string()).expect("fps");
        }
        if width > 0 && height > 0 {
            xml = replace_screen_size(&xml, width, height).expect("size");
            xml = replace(&xml, "bool", "Fullscreen", "false").expect("fullscreen");
            xml = replace(&xml, "bool", "StartMaximized", "false").expect("maximized");
        }
        xml
    }

    #[test]
    fn fps_cap_is_written_in_place() {
        let xml = patched(30, 0, 0);
        assert!(xml.contains("<int name=\"FramerateCap\">30</int>"));
        assert!(!xml.contains(">240<"));
    }

    #[test]
    fn start_size_becomes_the_requested_client_area() {
        let xml = patched(0, 640, 360);
        assert!(xml.contains("<X>640</X><Y>360</Y>"));
        // The position is somebody else's business.
        assert!(xml.contains("<X>567</X>"));
    }

    #[test]
    fn fullscreen_is_turned_off_for_a_sized_window() {
        let xml = patched(0, 800, 450);
        assert!(xml.contains("<bool name=\"Fullscreen\">false</bool>"));
    }

    #[test]
    fn foreign_settings_survive() {
        let xml = patched(20, 800, 450);
        assert!(xml.contains("<int name=\"GraphicsQualityLevel\">3</int>"));
        assert!(xml.contains("<roblox version=\"4\">"));
        assert_eq!(xml.matches("FramerateCap").count(), 1);
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("rbm-settings-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("GlobalBasicSettings_13.xml");

        assert_eq!(apply_at(&path, 30, 640, 360), Ok(false));
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_writes_the_file_once() {
        let dir = std::env::temp_dir().join(format!("rbm-settings-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("GlobalBasicSettings_13.xml");
        std::fs::write(&path, SAMPLE).unwrap();

        assert_eq!(apply_at(&path, 30, 640, 360), Ok(true));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("<int name=\"FramerateCap\">30</int>"));
        assert!(text.contains("<X>640</X><Y>360</Y>"));

        // Nothing changes the second time, so the file is not rewritten.
        assert_eq!(apply_at(&path, 30, 640, 360), Ok(false));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
