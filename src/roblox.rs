//! Roblox API client: cookie validation, auth ticket, client launch.
//!
//! Only public endpoints are used:
//!   GET  users.roblox.com/v1/users/authenticated
//!   POST auth.roblox.com/v1/authentication-ticket/
//! The cookie is sent nowhere except Roblox itself.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::client_settings;
use crate::window;

pub const USER_AGENT: &str = "RbxAccountManager/1.0 (local open-source tool)";
const ME_URL: &str = "https://users.roblox.com/v1/users/authenticated";
const TICKET_URL: &str = "https://auth.roblox.com/v1/authentication-ticket/";
const PLACE_LAUNCHER: &str = "https://assetgame.roblox.com/game/PlaceLauncher.ashx";
const CLIENT_EXE: &str = "RobloxPlayerBeta.exe";

/// How the game window was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    /// System `roblox-player:` handler: reuses the running client.
    Deeplink,
    /// Direct `RobloxPlayerBeta.exe --app`: always spawns a new window.
    NewWindow,
}

impl LaunchKind {
    pub fn human(self) -> &'static str {
        match self {
            LaunchKind::Deeplink => "через deeplink",
            LaunchKind::NewWindow => "отдельное окно (--app)",
        }
    }
}

/// Optional per-launch limits that save RAM and CPU.
///
/// A zero value always means "keep the game default".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Target FPS via `DFIntTaskSchedulerTargetFps`.
    pub fps_cap: u32,
    /// Graphics quality 1..=10 via `DFIntDebugFRMQualityLevelOverride`.
    pub quality: u32,
    /// Client area width, applied only in `--app` mode.
    pub width: u32,
    /// Client area height, applied only in `--app` mode.
    pub height: u32,
}

impl Limits {
    fn has_settings(self) -> bool {
        self.fps_cap > 0 || self.quality > 0
    }

    fn has_size(self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Human-readable list of what was written into the client settings.
    fn settings_note(self) -> String {
        let mut parts = Vec::new();
        if self.fps_cap > 0 {
            parts.push(format!("FPS ≤ {}", self.fps_cap));
        }
        if self.quality > 0 {
            parts.push(format!("графика {}", self.quality));
        }
        parts.join(", ")
    }
}

/// What actually happened, plus notes worth showing in the status bar.
pub struct LaunchOutcome {
    pub kind: LaunchKind,
    pub notes: Vec<String>,
}

impl LaunchOutcome {
    pub fn summary(&self) -> String {
        if self.notes.is_empty() {
            self.kind.human().to_string()
        } else {
            format!("{}, {}", self.kind.human(), self.notes.join(", "))
        }
    }
}

#[derive(Deserialize)]
struct IdentityResponse {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub id: u64,
    pub name: String,
    /// Display name, kept for future use (not shown in the table yet).
    #[allow(dead_code)]
    pub display: String,
}

/// Accepts a bare value, `name=value`, or a quoted string.
pub fn clean_cookie(raw: &str) -> String {
    let mut value = raw.trim();
    if value.to_ascii_lowercase().starts_with(".roblosecurity=") {
        value = &value[".roblosecurity=".len()..];
    }
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

/// Validate the cookie and return the account profile.
pub fn fetch_identity(raw_cookie: &str) -> Result<Profile, String> {
    let cookie = clean_cookie(raw_cookie);
    if cookie.is_empty() {
        return Err("Пустой cookie".into());
    }

    let result = ureq::get(ME_URL)
        .set("Cookie", &format!(".ROBLOSECURITY={cookie}"))
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .call();

    match result {
        Ok(response) => {
            let identity: IdentityResponse = response
                .into_json()
                .map_err(|err| format!("Не разобрал ответ Roblox: {err}"))?;
            if identity.id == 0 {
                return Err("Roblox не вернул профиль по cookie".into());
            }
            Ok(Profile {
                id: identity.id,
                name: identity.name,
                display: identity.display_name,
            })
        }
        Err(ureq::Error::Status(401 | 403, _)) => Err("Cookie недействителен или истёк".into()),
        Err(ureq::Error::Status(code, _)) => Err(format!("Roblox API: HTTP {code}")),
        Err(err) => Err(format!("Нет связи с Roblox: {err}")),
    }
}

/// Body / content type pairs the ticket endpoint accepts. Roblox is picky
/// about the media type of the CSRF request: a request without one is answered
/// with `415`, so the shapes are tried in order until one is accepted.
const TICKET_SHAPES: [(&str, &str); 2] = [
    ("{}", "application/json"),
    ("", "application/x-www-form-urlencoded"),
];

/// Why a single attempt failed.
enum TicketFailure {
    /// The request shape was rejected, another media type may work.
    TryAnother(String),
    /// Retrying with another shape will not help.
    Fatal(String),
}

/// Exchange the cookie for an authentication ticket.
///
/// Flow: the first POST is answered with `403` plus an `x-csrf-token` header,
/// then the same POST with that token returns `rbx-authentication-ticket`.
pub fn fetch_ticket(raw_cookie: &str) -> Result<String, String> {
    let cookie = clean_cookie(raw_cookie);
    if cookie.is_empty() {
        return Err("Пустой cookie".into());
    }

    let mut rejection = String::new();
    for (body, content_type) in TICKET_SHAPES {
        match ticket_for_shape(&cookie, body, content_type) {
            Ok(ticket) => return Ok(ticket),
            Err(TicketFailure::Fatal(message)) => return Err(message),
            Err(TicketFailure::TryAnother(message)) => rejection = message,
        }
    }

    Err(format!("Не удалось получить тикет: {rejection}"))
}

fn ticket_for_shape(cookie: &str, body: &str, content_type: &str) -> Result<String, TicketFailure> {
    let mut csrf: Option<String> = None;

    for attempt in 0..2 {
        let mut request = ureq::post(TICKET_URL)
            .set("Cookie", &format!(".ROBLOSECURITY={cookie}"))
            .set("Content-Type", content_type)
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/json")
            .set("Referer", "https://www.roblox.com/")
            .set("Origin", "https://www.roblox.com");
        if let Some(token) = csrf.as_deref() {
            request = request.set("X-CSRF-TOKEN", token);
        }

        match request.send_string(body) {
            Ok(response) => {
                let ticket = response
                    .header("rbx-authentication-ticket")
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                return ticket.ok_or_else(|| {
                    TicketFailure::Fatal("Roblox не выдал authentication ticket".into())
                });
            }
            Err(ureq::Error::Status(403, response)) => {
                let token = response.header("x-csrf-token").map(str::to_string);
                match (token, attempt) {
                    (Some(token), 0) => {
                        csrf = Some(token);
                        continue;
                    }
                    // A fresh token that the server rejects means the session
                    // itself is not valid any more.
                    (Some(_), _) => {
                        return Err(TicketFailure::Fatal(
                            "Cookie недействителен: Roblox не принял CSRF-токен".into(),
                        ))
                    }
                    (None, _) => {
                        let body = response.into_string().unwrap_or_default();
                        return Err(TicketFailure::TryAnother(format!(
                            "HTTP 403 без CSRF ({content_type}): {}",
                            summarize(&body)
                        )));
                    }
                }
            }
            Err(ureq::Error::Status(401, _)) => {
                return Err(TicketFailure::Fatal(
                    "Cookie недействителен (тикет не выдан)".into(),
                ))
            }
            // Wrong media type or a malformed body: retry with the next shape.
            Err(ureq::Error::Status(code @ (400 | 415 | 422), response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(TicketFailure::TryAnother(format!(
                    "HTTP {code} ({content_type}): {}",
                    summarize(&body)
                )));
            }
            Err(ureq::Error::Status(code, _)) => {
                return Err(TicketFailure::Fatal(format!("Roblox ответил HTTP {code}")))
            }
            Err(err) => return Err(TicketFailure::Fatal(format!("Нет связи с Roblox: {err}"))),
        }
    }

    Err(TicketFailure::Fatal(
        "Roblox не выдал authentication ticket".into(),
    ))
}

/// Short single-line version of an error body, safe for the status bar.
fn summarize(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().take(160).collect();
    if text.is_empty() {
        "пустой ответ".to_string()
    } else {
        text
    }
}

/// Build the `roblox-player:` deeplink for the system protocol handler.
pub fn build_launch_uri(place_id: u64, ticket: &str) -> String {
    let tracker = random_tracker();
    let launcher = build_join_url(place_id, tracker);
    let launched_at = now_millis();

    format!(
        "roblox-player:1+launchmode:play+gameinfo:{ticket}+launchtime:{launched_at}\
         +placelauncherurl:{}+browsertrackerid:{tracker}\
         +robloxLocale:en_us+gameLocale:en_us+channel:+LaunchExp:InApp",
        urlencoding::encode(&launcher)
    )
}

/// URL the client asks PlaceLauncher for when joining a place.
pub fn build_join_url(place_id: u64, tracker: u64) -> String {
    format!(
        "{PLACE_LAUNCHER}?request=RequestGame&placeId={place_id}\
         &browserTrackerId={tracker}&isPlayTogetherGame=false"
    )
}

fn random_tracker() -> u64 {
    rand::random::<u64>() % 9_000_000_000 + 1_000_000_000
}

/// Locate `RobloxPlayerBeta.exe`: protocol handler from the registry first,
/// then the usual `Roblox\Versions` folders. With several versions installed
/// the most recently updated one wins.
pub fn find_client() -> Option<PathBuf> {
    if let Some(path) = client_from_registry() {
        return Some(path);
    }

    let mut roots = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Roblox").join("Versions"));
    }
    if let Ok(program_files) = std::env::var("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files).join("Roblox").join("Versions"));
    }

    let mut candidates = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let exe = entry.path().join(CLIENT_EXE);
            if exe.is_file() {
                candidates.push(exe);
            }
        }
    }

    candidates.into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    })
}

/// Read the path from `HKCU\Software\Classes\roblox-player\shell\open\command`.
fn client_from_registry() -> Option<PathBuf> {
    let stdout = run_hidden(Command::new("reg").args([
        "query",
        r"HKCU\Software\Classes\roblox-player\shell\open\command",
        "/ve",
    ]))?;

    // Output line looks like:
    //     (Default)    REG_SZ    "C:\...\RobloxPlayerBeta.exe" "%1"
    let text = String::from_utf8_lossy(&stdout);
    let text: &str = text.as_ref();
    let start = text.find('"')? + 1;
    let end = text[start..].find('"')? + start;
    let path = PathBuf::from(text[start..end].trim());
    path.is_file().then_some(path)
}

/// Run a console command without flashing a console window.
fn run_hidden(command: &mut Command) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    output.status.success().then_some(output.stdout)
}

/// Endpoint the client redeems the authentication ticket at.
const NEGOTIATE_URL: &str = "https://www.roblox.com/Login/Negotiate.ashx";

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// Arguments for one standalone client window.
///
/// `-a` is what makes `-t` work: without it the client never learns where to
/// redeem the ticket, ignores it and opens the login screen instead.
fn window_args(ticket: &str, join_url: &str, launchtime: i64) -> Vec<String> {
    [
        "--app".to_string(),
        "-a".to_string(),
        NEGOTIATE_URL.to_string(),
        "-t".to_string(),
        ticket.to_string(),
        format!("--launchtime={launchtime}"),
        "-j".to_string(),
        join_url.to_string(),
    ]
    .to_vec()
}

/// Start a separate client window:
/// `RobloxPlayerBeta.exe --app -a <negotiate> -t <ticket> --launchtime=<ms> -j <url>`.
///
/// `--app` skips the singleton check, so every call opens another window.
fn spawn_new_window(
    exe: &Path,
    ticket: &str,
    join_url: &str,
) -> Result<std::process::Child, String> {
    Command::new(exe)
        .args(window_args(ticket, join_url, now_millis()))
        .spawn()
        .map_err(|err| format!("Не удалось запустить {}: {err}", exe.display()))
}

/// Launch the game for one account.
///
/// * `multi == false` — system `roblox-player:` handler, single window.
/// * `multi == true`  — direct client start with `--app`, new window every call.
///
/// FPS and quality limits are written into `ClientSettings/ClientAppSettings.json`
/// inside the client version folder before the client starts, because the client
/// reads that file only at startup. The window size is applied afterwards, which
/// is possible only for windows we spawned ourselves.
pub fn launch_game(
    place_id: u64,
    cookie: &str,
    multi: bool,
    limits: Limits,
) -> Result<LaunchOutcome, String> {
    let ticket = fetch_ticket(cookie)?;
    let client = find_client();
    let mut notes = Vec::new();

    if limits.has_settings() {
        match client.as_deref() {
            Some(exe) => match client_settings::apply(exe, limits.fps_cap, limits.quality) {
                Ok(Some(_)) => notes.push(limits.settings_note()),
                Ok(None) => {}
                Err(err) => notes.push(format!("настройки клиента не применены: {err}")),
            },
            None => notes.push("клиент не найден: FPS/графику не применил".into()),
        }
    }

    if multi {
        let exe =
            client.ok_or_else(|| format!("Не нашёл {CLIENT_EXE}. Клиент Roblox установлен?"))?;
        let join_url = build_join_url(place_id, random_tracker());
        let child = spawn_new_window(&exe, &ticket, &join_url)?;

        if limits.has_size() {
            window::resize_when_ready(child.id(), limits.width, limits.height);
            notes.push(format!("окно {}×{}", limits.width, limits.height));
        }

        return Ok(LaunchOutcome {
            kind: LaunchKind::NewWindow,
            notes,
        });
    }

    if limits.has_size() {
        notes.push("размер окна задаётся только в режиме нескольких окон".into());
    }

    let uri = build_launch_uri(place_id, &ticket);
    open::that(uri).map_err(|err| format!("Не удалось открыть клиент Roblox: {err}"))?;
    Ok(LaunchOutcome {
        kind: LaunchKind::Deeplink,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_prefixes_and_quotes_are_removed() {
        assert_eq!(clean_cookie("  abc  "), "abc");
        assert_eq!(clean_cookie(".ROBLOSECURITY=abc"), "abc");
        assert_eq!(clean_cookie(".roblosecurity=\"abc\""), "abc");
        assert_eq!(clean_cookie(".ROBLOSECURITY='abc'"), "abc");
    }

    #[test]
    fn cookie_warning_prefix_is_kept() {
        // The warning text is part of the real cookie value.
        let raw = "_|WARNING:-DO-NOT-SHARE-THIS.|_abc";
        assert_eq!(clean_cookie(raw), raw);
    }

    #[test]
    fn error_body_is_flattened_and_cut() {
        assert_eq!(summarize("a\n  b\tc"), "a b c");
        assert_eq!(summarize(""), "пустой ответ");
        assert_eq!(summarize(&"x".repeat(500)).len(), 160);
    }

    #[test]
    fn limits_zero_means_defaults() {
        let none = Limits::default();
        assert!(!none.has_settings());
        assert!(!none.has_size());

        let limits = Limits {
            fps_cap: 60,
            quality: 1,
            width: 960,
            height: 540,
        };
        assert!(limits.has_settings());
        assert!(limits.has_size());
        assert_eq!(limits.settings_note(), "FPS ≤ 60, графика 1");
    }

    #[test]
    fn window_args_include_the_negotiate_endpoint() {
        // Without `-a` the client ignores `-t` and shows the login screen.
        let args = window_args("TICKET", "https://example/join", 1700000000000);
        assert_eq!(
            args,
            [
                "--app",
                "-a",
                "https://www.roblox.com/Login/Negotiate.ashx",
                "-t",
                "TICKET",
                "--launchtime=1700000000000",
                "-j",
                "https://example/join",
            ]
        );
    }

    #[test]
    fn join_url_carries_place_and_tracker() {
        let url = build_join_url(1818, 1234);
        assert!(url.starts_with("https://assetgame.roblox.com/game/PlaceLauncher.ashx?"));
        assert!(url.contains("request=RequestGame"));
        assert!(url.contains("placeId=1818"));
        assert!(url.contains("browserTrackerId=1234"));
        assert!(url.contains("isPlayTogetherGame=false"));
    }
}
