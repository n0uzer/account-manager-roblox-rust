//! Sober: the Roblox Android client under Flatpak, and how one manager account
//! becomes one Sober profile.
//!
//! Sober keeps its whole session in one Flatpak data directory: cookies, fast
//! flags, the extracted engine and the in-game settings. Two accounts therefore
//! cannot share an installation — the second login would replace the first.
//! The way out is the one the isolation was built for: give every account its
//! own `HOME`. `flatpak run` then reads and writes `$HOME/.var/app/<id>` and
//! never touches the real home, so N accounts are N independent Sober profiles
//! and can run side by side.
//!
//! What goes into a profile:
//!
//! * the APK store, reflinked (copy-on-write) from the real installation when
//!   the filesystem supports it and copied otherwise — never hard-linked: Sober
//!   locks the APK file, and one inode shared by two profiles makes the second
//!   instance die with "An instance of Sober is already running";
//! * `cookies` — the plain semicolon-separated Cookie header Sober hands to the
//!   engine. Writing `.ROBLOSECURITY=` there is what signs the profile in;
//! * `config/sober/config.json` — fast flags for FPS, quality and textures;
//! * `appData/GlobalBasicSettings_13.xml` — the frame cap and the window size,
//!   the same file the in-game menu writes.
//!
//! `.ROBLOSECURITY` is world-readable on disk by design of the client, so the
//! manager only keeps it in a profile while that profile is in use: every
//! startup, lock and deletion wipes it again and the encrypted store stays the
//! only long-lived copy.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::client_settings::{
    CSG_KEYS, FPS_KEY, GRASS_MAX_KEY, GRASS_MIN_KEY, MSAA_KEY, PAUSE_VOXELIZER_KEY, QUALITY_KEY,
    SKY_GRAY_KEY, TEXTURE_ENABLED_KEY, TEXTURE_KEY,
};
use crate::game_settings;
use crate::processes;
use crate::roblox::{self, Destination, LaunchKind, LaunchOutcome, Limits, PrivateServer};

/// Flatpak id of the client.
pub const APP_ID: &str = "org.vinegarhq.Sober";
/// Cap the client itself ships with; written back when the user turns the
/// limit off, because the profile still holds the previous value.
const DEFAULT_FPS_CAP: u32 = 240;
/// Per-home directory Flatpak gives the app.
const SANDBOX: &str = ".var/app/org.vinegarhq.Sober";
/// Where Sober keeps its own files inside the sandbox.
const DATA: &str = "data/sober";
/// Keys this module owns in `config.json`; anything else is left untouched.
const MANAGED_FLAGS: [&str; 12] = [
    FPS_KEY,
    QUALITY_KEY,
    TEXTURE_ENABLED_KEY,
    TEXTURE_KEY,
    GRASS_MIN_KEY,
    GRASS_MAX_KEY,
    SKY_GRAY_KEY,
    PAUSE_VOXELIZER_KEY,
    CSG_KEYS[0],
    CSG_KEYS[1],
    CSG_KEYS[2],
    CSG_KEYS[3],
];

// --------------------------------------------------------------- locations

/// Whether a Sober installation is visible to `flatpak`.
///
/// Only a **system-wide** install works with a per-account `HOME`: a `--user`
/// install lives under the real `~/.local/share/flatpak`, which a profile never
/// sees, so reporting it as usable would only fail later, in `flatpak run`.
pub fn installed() -> bool {
    if !on_path("flatpak") {
        return false;
    }
    Path::new("/var/lib/flatpak/app")
        .join(APP_ID)
        .join("current")
        .exists()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(name);
            candidate.is_file()
        })
    })
}

/// Data directory of the real, unmanaged Sober install.
fn host_data_dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(SANDBOX)
            .join(DATA),
    )
}

/// `$XDG_DATA_HOME/RbxAccountManager/sober` (or `~/.local/share/...`).
pub fn profiles_root() -> PathBuf {
    crate::store::app_dir().join("sober")
}

/// Directory used as `HOME` for one account.
pub fn profile_dir(account_id: &str) -> PathBuf {
    let safe: String = account_id
        .chars()
        .map(|unit| {
            if unit.is_ascii_alphanumeric() || unit == '-' || unit == '_' {
                unit
            } else {
                '_'
            }
        })
        .collect();
    profiles_root().join(safe)
}

fn sandbox_dir(profile: &Path) -> PathBuf {
    profile.join(SANDBOX)
}

fn data_dir(profile: &Path) -> PathBuf {
    sandbox_dir(profile).join(DATA)
}

pub fn cookies_path(profile: &Path) -> PathBuf {
    data_dir(profile).join("cookies")
}

fn config_path(profile: &Path) -> PathBuf {
    sandbox_dir(profile).join("config/sober/config.json")
}

fn state_path(profile: &Path) -> PathBuf {
    data_dir(profile).join("state")
}

fn settings_path(profile: &Path) -> PathBuf {
    data_dir(profile).join("appData/GlobalBasicSettings_13.xml")
}

// ---------------------------------------------------------------- seeding

/// Create the profile if it is new. Cheap when it exists: only `stat` calls.
pub fn ensure_profile(profile: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let data = data_dir(profile);
    fs::create_dir_all(&data)
        .and_then(|()| fs::create_dir_all(sandbox_dir(profile).join("config/sober")))
        .and_then(|()| fs::create_dir_all(data.join("appData")))
        .map_err(|err| format!("не могу создать профиль {}: {err}", profile.display()))?;
    // The cookies of this profile are plain text while it is in use.
    let _ = fs::set_permissions(profile, fs::Permissions::from_mode(0o700));

    if let Some(host) = host_data_dir() {
        seed(&host, &data)?;
    }

    // A fresh profile must be usable without a first manual launch, so the file
    // the limits are written into has to exist already.
    let settings = settings_path(profile);
    if !settings.exists() {
        if let Some(host) = host_data_dir() {
            let from = host.join("appData/GlobalBasicSettings_13.xml");
            if from.is_file() {
                let _ = fs::copy(&from, &settings);
            }
        }
    }

    force_windowed(profile)?;
    let config = read_config(profile)?;
    write_config(profile, &config)?;
    Ok(())
}

/// Copy the parts of a real installation that would otherwise be downloaded
/// again: the APK store, the app state and the base settings.
fn seed(host: &Path, data: &Path) -> Result<(), String> {
    let packages = data.join("packages");
    if !packages.exists() {
        let from = host.join("packages");
        if from.is_dir() {
            copy_tree(&from, &packages)
                .map_err(|err| format!("не могу скопировать APK Sober: {err}"))?;
        }
    } else {
        // Profiles seeded by an older build share the APK with the host and
        // with each other. Break those links before the next launch.
        break_shared_links(&packages)
            .map_err(|err| format!("не могу отделить APK профиля: {err}"))?;
    }

    let state = data.join("state");
    if !state.exists() {
        let from = host.join("state");
        if from.is_file() {
            let _ = fs::copy(&from, &state);
        }
    }

    let app_data = data.join("appData");
    for name in ["GlobalBasicSettings_13.xml", "ClientSettings"] {
        let to = app_data.join(name);
        let from = host.join("appData").join(name);
        if !to.exists() && from.exists() {
            if from.is_dir() {
                let _ = copy_tree(&from, &to);
            } else {
                let _ = copy_private(&from, &to);
            }
        }
    }
    Ok(())
}

/// Copy a tree into a profile. Reflinks keep the 150 MB APK store almost free on
/// Btrfs/XFS; a plain copy is the fallback. Never a hard link: Sober holds an
/// advisory lock on `base.apk`, so a shared inode makes a second account think
/// Sober is already running.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
        } else if kind.is_file() && !target.exists() {
            copy_private(&source, &target)?;
        }
    }
    Ok(())
}

/// Copy one file through a temporary name, so a failed copy never leaves a
/// half-written APK where Sober expects a complete one.
fn copy_private(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut temp = to.as_os_str().to_owned();
    temp.push(".rbm-copy");
    let temp = PathBuf::from(temp);
    let _ = fs::remove_file(&temp);
    if reflink(from, &temp).is_err() {
        fs::copy(from, &temp)?;
    }
    match fs::rename(&temp, to) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// `cp --reflink=always` asks the filesystem for a copy-on-write clone. A
/// filesystem without reflink support says no, and the caller falls back.
fn reflink(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let status = Command::new("cp")
        .args(["--reflink=always", "--preserve=mode,timestamps", "--"])
        .arg(from)
        .arg(to)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("reflink is not supported here"))
    }
}

/// Replace every multiply-linked file under `root` with a private copy. Profiles
/// created before reflinks existed still point at the host's APK inode.
fn break_shared_links(root: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            break_shared_links(&path)?;
        } else if kind.is_file() && entry.metadata()?.nlink() > 1 {
            copy_private(&path, &path)?;
        }
    }
    Ok(())
}

/// Sober remembers whether it was fullscreen. A managed profile always starts
/// windowed, otherwise the second account's window hides the first one.
fn force_windowed(profile: &Path) -> Result<(), String> {
    let path = state_path(profile);
    let text = fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let mut state: Value =
        serde_json::from_str(&text).unwrap_or_else(|_| Value::Object(Map::new()));
    if !state.is_object() {
        state = Value::Object(Map::new());
    }
    let root = state.as_object_mut().expect("checked above");
    let v1 = root
        .entry("v1")
        .or_insert_with(|| Value::Object(Map::new()));
    if !v1.is_object() {
        *v1 = Value::Object(Map::new());
    }
    v1.as_object_mut()
        .expect("checked above")
        .insert("fullscreen".to_string(), Value::Bool(false));
    let text = serde_json::to_string_pretty(&state)
        .map_err(|err| format!("не могу записать состояние Sober: {err}"))?;
    write_private(&path, &format!("{text}\n"))
        .map_err(|err| format!("не могу записать {}: {err}", path.display()))
}

// ----------------------------------------------------------------- cookies

/// Write the account's cookie into the profile, keeping cookies Sober itself
/// put there (`RBXEventTrackerV2` and friends).
pub fn write_cookie(profile: &Path, cookie: &str) -> Result<(), String> {
    let cookie = roblox::clean_cookie(cookie);
    if cookie.is_empty() {
        return Err("Пустой cookie".into());
    }
    let path = cookies_path(profile);
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut parts = foreign_cookies(&existing);
    parts.push(format!(".ROBLOSECURITY={cookie}"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("не могу создать {}: {err}", parent.display()))?;
    }
    write_private(&path, &parts.join("; "))
        .map_err(|err| format!("не могу записать cookie профиля: {err}"))
}

/// Drop the session cookie from one profile, keeping the harmless ones.
pub fn purge_cookie(profile: &Path) -> Result<(), String> {
    let path = cookies_path(profile);
    let Ok(existing) = fs::read_to_string(&path) else {
        return Ok(());
    };
    if !existing.to_ascii_lowercase().contains(".roblosecurity=") {
        return Ok(());
    }
    let parts = foreign_cookies(&existing);
    write_private(&path, &parts.join("; "))
        .map_err(|err| format!("не могу очистить cookie профиля: {err}"))
}

fn foreign_cookies(header: &str) -> Vec<String> {
    header
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter(|part| !part.to_ascii_lowercase().starts_with(".roblosecurity="))
        .map(str::to_string)
        .collect()
}

/// Wipe every profile's session cookie. Called at startup, on lock and on exit,
/// so a crash cannot leave a usable cookie lying in a world-readable file.
pub fn purge_all() -> usize {
    let Ok(entries) = fs::read_dir(profiles_root()) else {
        return 0;
    };
    let mut purged = 0;
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && purge_cookie(&entry.path()).is_ok()
        {
            purged += 1;
        }
    }
    purged
}

/// Delete one account's profile, cookies and all.
pub fn remove_profile(account_id: &str) -> Result<(), String> {
    let profile = profile_dir(account_id);
    if !profile.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&profile)
        .map_err(|err| format!("не могу удалить профиль {}: {err}", profile.display()))
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

// ------------------------------------------------------------------ limits

/// Write FPS, quality and texture limits as typed FastFlags into the profile's
/// Sober config. Values that are off remove their key, so "off" really means the
/// game default instead of whatever was written last time.
pub fn apply_limits(profile: &Path, limits: Limits) -> Result<(), String> {
    let mut config = read_config(profile)?;

    let mut flags = match config.remove("fflags") {
        Some(Value::Object(flags)) => flags,
        _ => Map::new(),
    };
    for key in MANAGED_FLAGS {
        flags.remove(key);
    }

    if limits.fps_cap > 0 {
        flags.insert(FPS_KEY.to_string(), Value::from(limits.fps_cap));
    }
    if limits.quality > 0 {
        flags.insert(QUALITY_KEY.to_string(), Value::from(limits.quality));
    }
    if let Some(level) = limits.texture {
        flags.insert(TEXTURE_ENABLED_KEY.to_string(), Value::Bool(true));
        flags.insert(TEXTURE_KEY.to_string(), Value::from(level));
    }
    if limits.extras.no_grass {
        flags.insert(GRASS_MIN_KEY.to_string(), Value::from(0));
        flags.insert(GRASS_MAX_KEY.to_string(), Value::from(0));
    }
    if limits.extras.gray_sky {
        flags.insert(SKY_GRAY_KEY.to_string(), Value::Bool(true));
    }
    if limits.extras.pause_voxelizer {
        flags.insert(PAUSE_VOXELIZER_KEY.to_string(), Value::Bool(true));
    }
    if limits.extras.low_csg {
        for key in CSG_KEYS {
            flags.insert(key.to_string(), Value::from(0));
        }
    }
    if limits.extras.msaa > 0 {
        flags.insert(MSAA_KEY.to_string(), Value::from(limits.extras.msaa));
    }

    config.insert("fflags".to_string(), Value::Object(flags));
    write_config(profile, &config)
}

/// The frame cap and the start size live in the client's own settings, and are
/// read once at startup — the same treatment as on Windows.
///
/// A zero cap means "the game default", which is 240 in the client's file; the
/// profile keeps whatever the previous launch wrote, so the default has to be
/// written back instead of skipped.
pub fn apply_game_settings(profile: &Path, limits: Limits) -> Result<bool, String> {
    let fps = if limits.fps_cap == 0 {
        DEFAULT_FPS_CAP
    } else {
        limits.fps_cap
    };
    game_settings::apply_at(&settings_path(profile), fps, limits.width, limits.height)
}

fn read_config(profile: &Path) -> Result<Map<String, Value>, String> {
    let text = match fs::read_to_string(config_path(profile)) {
        Ok(text) => text,
        Err(_) => {
            // First run: start from the real installation's settings, without
            // its cookies and without its session.
            let mut seed = None;
            if let Some(home) = std::env::var_os("HOME") {
                let from = PathBuf::from(home)
                    .join(SANDBOX)
                    .join("config/sober/config.json");
                seed = fs::read_to_string(from).ok();
            }
            seed.unwrap_or_else(|| "{}".to_string())
        }
    };
    let stripped = strip_comments(&text);
    match serde_json::from_str::<Value>(&stripped) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Ok(Map::new()),
    }
}

/// Sober ships its config with `//` header lines. Every byte outside a comment
/// is kept, so a rewrite only changes the keys the manager owns.
fn strip_comments(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_config(profile: &Path, config: &Map<String, Value>) -> Result<(), String> {
    let mut config = config.clone();
    // The cookies file is the only session source the manager can write, and
    // the keyring is per user rather than per profile: it would mix accounts
    // up instead of isolating them.
    config.insert("use_libsecret".to_string(), Value::Bool(false));
    let config = &config;

    let path = config_path(profile);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("не могу создать {}: {err}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(config)
        .map_err(|err| format!("не могу собрать конфиг Sober: {err}"))?;
    // Sober refuses to start on malformed JSON, so the new file is written next
    // to the old one and moved into place in one step.
    let temporary = path.with_extension("json.new");
    fs::write(&temporary, format!("{text}\n"))
        .map_err(|err| format!("не могу записать конфиг Sober: {err}"))?;
    fs::rename(&temporary, &path).map_err(|err| format!("не могу заменить конфиг Sober: {err}"))
}

// ------------------------------------------------------------------ launch

/// Deep link Sober understands. The engine signs in with the profile's cookie,
/// so no authentication ticket has to be fetched first.
pub fn launch_uri(place_id: u64, private: Option<&str>, job_id: Option<&str>) -> String {
    match (private, job_id) {
        (Some(code), _) => format!(
            "roblox://placeId={place_id}&linkCode={}",
            urlencoding::encode(code)
        ),
        (None, Some(job_id)) => format!(
            "roblox://placeId={place_id}&gameInstanceId={}",
            urlencoding::encode(job_id)
        ),
        (None, None) => format!("roblox://placeId={place_id}"),
    }
}

/// Launch one account in its own Sober profile.
pub fn launch_game(
    account_id: &str,
    place: Destination,
    cookie: &str,
    limits: Limits,
) -> Result<LaunchOutcome, String> {
    if !installed() {
        return Err(format!(
            "Sober не установлен (flatpak {APP_ID}) или установлен только для пользователя"
        ));
    }

    let profile = profile_dir(account_id);
    ensure_profile(&profile)?;

    let mut notes = Vec::new();
    let mut place_id = place.place_id;
    let private = match place.private {
        Some(PrivateServer::LinkCode(code)) => Some(code),
        Some(PrivateServer::ShareCode(code)) => {
            let (resolved, link) = roblox::resolve_share_link(&code, cookie)?;
            if place_id == 0 {
                place_id = resolved;
            }
            Some(link)
        }
        None => None,
    };
    if place_id == 0 {
        return Err("Не понял Place ID: вставь ссылку на игру или на приватный сервер".into());
    }
    if private.is_some() {
        notes.push("приватный сервер".to_string());
    } else if let Some(job_id) = place.job_id.as_deref() {
        notes.push(format!("сервер {}", roblox::short_job(job_id)));
    }

    write_cookie(&profile, cookie)?;
    // With the grid on, the compositor decides the size of each window. A start
    // size written into the client settings would only fight it.
    let limits = if limits.tile {
        Limits {
            width: 0,
            height: 0,
            ..limits
        }
    } else {
        limits
    };
    if limits.tile {
        notes.push("раскладка сеткой".to_string());
    }
    // Written even when every limit is off: that is what removes the flags a
    // previous launch left in the profile.
    match apply_limits(&profile, limits) {
        Ok(()) => {
            let note = limits.settings_note();
            if !note.is_empty() {
                notes.push(note);
            }
        }
        Err(err) => notes.push(format!("настройки клиента не применены: {err}")),
    }
    if let Err(err) = apply_game_settings(&profile, limits) {
        notes.push(format!("настройки игры не применены: {err}"));
    }

    let uri = launch_uri(place_id, private.as_deref(), place.job_id.as_deref());
    let launched_at = processes::now_filetime();
    spawn(&profile, &uri)?;
    #[cfg(target_os = "linux")]
    if limits.tile {
        // The window appears a moment later; the layout worker keeps applying
        // the grid for a while, so one call here is enough.
        crate::window::arrange();
    }
    Ok(LaunchOutcome {
        kind: LaunchKind::Sober,
        notes,
        launched_at,
    })
}

fn spawn(profile: &Path, uri: &str) -> Result<(), String> {
    let mut command = Command::new("flatpak");
    command
        .arg("run")
        .arg(APP_ID)
        .arg(uri)
        .env("HOME", profile)
        // Flatpak derives the per-app directory from HOME; leaving the XDG
        // overrides in place would send the profile somewhere else entirely.
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Не удалось запустить Sober: {err}"))
}

/// One line for the status bar / diagnostics.
pub fn status() -> String {
    if !installed() {
        return "Sober не найден: flatpak не установил org.vinegarhq.Sober".to_string();
    }
    let root = profiles_root();
    let count = fs::read_dir(&root)
        .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    format!("Sober готов · профилей: {count}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rbm-sober-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn launch_uri_shapes() {
        assert_eq!(launch_uri(1818, None, None), "roblox://placeId=1818");
        assert_eq!(
            launch_uri(1818, Some("abc"), None),
            "roblox://placeId=1818&linkCode=abc"
        );
        assert_eq!(
            launch_uri(1818, None, Some("d1e7b1e3-129e-404f-8882-d29d26c72dcd")),
            "roblox://placeId=1818&gameInstanceId=d1e7b1e3-129e-404f-8882-d29d26c72dcd"
        );
    }

    #[test]
    fn a_shared_apk_link_is_broken_into_a_private_copy() {
        use std::os::unix::fs::MetadataExt;

        let dir = scratch("links");
        let root = dir.join("packages");
        fs::create_dir_all(&root).unwrap();
        let host = dir.join("host.apk");
        fs::write(&host, b"apk bytes").unwrap();
        let shared = root.join("base.apk");
        fs::hard_link(&host, &shared).unwrap();
        assert_eq!(fs::metadata(&shared).unwrap().nlink(), 2);

        break_shared_links(&root).unwrap();

        let copied = fs::metadata(&shared).unwrap();
        assert_eq!(copied.nlink(), 1, "the APK is still shared");
        assert_ne!(copied.ino(), fs::metadata(&host).unwrap().ino());
        assert_eq!(fs::read(&shared).unwrap(), b"apk bytes");
        assert_eq!(
            fs::read(&host).unwrap(),
            b"apk bytes",
            "the host copy changed"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cookie_round_trip_keeps_foreign_cookies() {
        let dir = scratch("cookie");
        let profile = dir.join("profile");
        fs::create_dir_all(data_dir(&profile)).unwrap();

        fs::write(
            cookies_path(&profile),
            "RBXEventTrackerV2=CreateDate=x&rbxid=1&browserid=2",
        )
        .unwrap();

        write_cookie(&profile, ".ROBLOSECURITY=secret-value").unwrap();
        let text = fs::read_to_string(cookies_path(&profile)).unwrap();
        assert!(text.contains(".ROBLOSECURITY=secret-value"));
        assert!(text.contains("RBXEventTrackerV2="));

        purge_cookie(&profile).unwrap();
        let text = fs::read_to_string(cookies_path(&profile)).unwrap();
        assert!(!text.to_ascii_lowercase().contains(".roblosecurity"));
        assert!(text.contains("RBXEventTrackerV2="));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn config_comments_are_dropped_and_flags_are_typed() {
        let dir = scratch("config");
        let profile = dir.join("profile");
        fs::create_dir_all(sandbox_dir(&profile).join("config/sober")).unwrap();
        fs::write(
            config_path(&profile),
            "// header\n{\n  \"enable_hidpi\": false,\n  \"fflags\": {\"KeepMe\": 3}\n}\n",
        )
        .unwrap();

        apply_limits(
            &profile,
            Limits {
                fps_cap: 60,
                quality: 3,
                texture: Some(1),
                ..Limits::default()
            },
        )
        .unwrap();

        let text = fs::read_to_string(config_path(&profile)).unwrap();
        let parsed: Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(parsed["enable_hidpi"], Value::Bool(false));
        assert_eq!(parsed["use_libsecret"], Value::Bool(false));
        assert_eq!(parsed["fflags"]["KeepMe"], Value::from(3));
        assert_eq!(parsed["fflags"][FPS_KEY], Value::from(60));
        assert_eq!(parsed["fflags"][TEXTURE_ENABLED_KEY], Value::Bool(true));

        // Turning a limit off removes its key instead of leaving the old value.
        apply_limits(&profile, Limits::default()).unwrap();
        let text = fs::read_to_string(config_path(&profile)).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert!(parsed["fflags"].get(FPS_KEY).is_none());
        assert!(parsed["fflags"].get(TEXTURE_KEY).is_none());
        assert_eq!(parsed["fflags"]["KeepMe"], Value::from(3));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_profile_is_windowed() {
        let dir = scratch("state");
        let profile = dir.join("profile");
        fs::create_dir_all(data_dir(&profile)).unwrap();
        force_windowed(&profile).unwrap();
        let text = fs::read_to_string(state_path(&profile)).unwrap();
        let state: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(state["v1"]["fullscreen"], Value::Bool(false));
        let _ = fs::remove_dir_all(dir);
    }

    /// Live check. Starts a real Sober window with a throw-away profile and a
    /// fake cookie, then proves the process scan and the window tracker see it:
    ///
    /// `cargo test launch_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn launch_smoke() {
        assert!(installed(), "Sober is not installed system-wide");
        let id = format!("smoke-{}", std::process::id());
        let place = Destination {
            place_id: 1818,
            ..Destination::default()
        };
        let limits = Limits {
            fps_cap: 30,
            quality: 2,
            width: 960,
            height: 540,
            ..Limits::default()
        };
        let outcome = launch_game(
            &id,
            place,
            "_|WARNING:-DO-NOT-SHARE-THIS.|_smoke-test",
            limits,
        )
        .expect("launch");
        println!("profile: {}", profile_dir(&id).display());
        println!("notes: {:?}", outcome.notes);

        let mut started = Vec::new();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            started = crate::processes::client_procs()
                .into_iter()
                .filter(|proc| proc.created >= outcome.launched_at)
                .collect();
            if !started.is_empty() {
                break;
            }
        }
        println!("started: {started:?}");
        assert!(!started.is_empty(), "no Sober instance appeared");

        // The engine starts, rewrites its own cookies and must keep ours.
        std::thread::sleep(std::time::Duration::from_secs(8));
        let profile = profile_dir(&id);
        let cookies = fs::read_to_string(cookies_path(&profile)).unwrap_or_default();
        println!("cookies: {cookies}");
        assert!(
            cookies.contains(".ROBLOSECURITY=_|WARNING:-DO-NOT-SHARE-THIS.|_smoke-test"),
            "the engine dropped the injected cookie"
        );

        // The APK must be a private inode: Sober locks it, and a hard link
        // shared with the host would make every second account fail to start.
        if let Some(host) = host_data_dir() {
            use std::os::unix::fs::MetadataExt;
            let shared = "packages/x86_64/com.roblox.client/base.apk";
            let from = fs::metadata(host.join(shared));
            let to = fs::metadata(data_dir(&profile).join(shared));
            if let (Ok(from), Ok(to)) = (from, to) {
                println!("apk inodes: {} {}", from.ino(), to.ino());
                assert_ne!(from.ino(), to.ino(), "the APK is still a hard link");
            }
        }

        let mut tracker = crate::watch::Tracker::default();
        tracker.launched("smoke", outcome.launched_at);
        let events = tracker.poll(&started, crate::processes::now_filetime());
        println!("events: {events:?}");
        assert!(matches!(
            events.first(),
            Some(crate::watch::Event::Opened { .. })
        ));

        println!("killed: {}", crate::processes::kill(&[started[0].pid]));
        std::thread::sleep(std::time::Duration::from_secs(3));
        let left = crate::processes::client_procs()
            .into_iter()
            .filter(|proc| proc.created >= outcome.launched_at)
            .count();
        println!("left after kill: {left}");
        let _ = remove_profile(&id);
    }

    #[test]
    #[ignore = "launches two real Sober profiles side by side"]
    fn smoke_two_profiles_fit_together() {
        use std::os::unix::fs::MetadataExt;
        use std::time::Duration;

        // Profiles an older build left behind: their APKs were hard links to
        // the host's inode, which is exactly what made the second account crash
        // with "An instance of Sober is already running".
        let ids = ["a2794b39b38b7516", "dbca626b5d57936e"];
        let mut profiles = Vec::new();
        for id in ids {
            let profile = profile_dir(id);
            ensure_profile(&profile).unwrap();
            let apk = data_dir(&profile).join("packages/x86_64/com.roblox.client/base.apk");
            let meta = fs::metadata(&apk).expect("seeded APK");
            println!("{id}: apk nlink={} ino={}", meta.nlink(), meta.ino());
            assert_eq!(meta.nlink(), 1, "{id} still shares its APK");
            profiles.push(profile);
        }

        let launched_at = crate::processes::now_filetime();
        for profile in &profiles {
            // The singleton check happens before a place loads; a real place
            // keeps Sober from rejecting the URI and closing again.
            spawn(profile, "roblox://placeId=1818").expect("spawn");
        }

        let mut started = Vec::new();
        for _ in 0..30 {
            std::thread::sleep(Duration::from_secs(1));
            started = crate::processes::client_procs()
                .into_iter()
                .filter(|proc| proc.created >= launched_at)
                .collect();
            if started.len() >= 2 {
                break;
            }
        }
        println!("started: {started:?}");

        // The singleton check runs during the first seconds of startup; give
        // both instances time to get past it and open their windows.
        std::thread::sleep(Duration::from_secs(8));
        let alive: Vec<_> = crate::processes::client_procs()
            .into_iter()
            .filter(|proc| proc.created >= launched_at)
            .collect();
        println!("alive after 8s: {alive:?}");

        for profile in &profiles {
            let log = fs::read_to_string(data_dir(profile).join("sober_logs/latest.log"))
                .unwrap_or_default();
            assert!(
                !log.contains("An instance of Sober is already running"),
                "{} hit the singleton lock",
                profile.display()
            );
            assert!(
                log.contains("lifecycle:"),
                "{} did not reach startup",
                profile.display()
            );
        }

        let pids: Vec<u32> = alive.iter().map(|proc| proc.pid).collect();
        println!("killed: {}", crate::processes::kill(&pids));
        assert!(alive.len() >= 2, "two accounts did not stay up together");
    }

    #[test]
    fn profile_names_cannot_escape_the_data_dir() {
        let path = profile_dir("../../etc");
        assert!(path.starts_with(profiles_root()));
        assert!(!path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)));
        assert_eq!(
            path.components().count(),
            profiles_root().components().count() + 1
        );
    }
}
