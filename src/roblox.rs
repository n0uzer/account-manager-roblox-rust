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
use crate::game_settings;
use crate::processes;
use crate::window;

pub const USER_AGENT: &str = "RbxAccountManager/1.0 (local open-source tool)";
const ME_URL: &str = "https://users.roblox.com/v1/users/authenticated";
const TICKET_URL: &str = "https://auth.roblox.com/v1/authentication-ticket/";
const PLACE_LAUNCHER: &str = "https://assetgame.roblox.com/game/PlaceLauncher.ashx";
/// Resolves `/share?code=…&type=Server` links into a place and a link code.
const SHARE_LINKS: &str = "https://apis.roblox.com/sharelinks/v1/resolve-link";
/// Public server list of one place.
const SERVERS_URL: &str = "https://games.roblox.com/v1/games";
const CLIENT_EXE: &str = "RobloxPlayerBeta.exe";

/// How the game window was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    /// System `roblox-player:` handler: reuses the running client.
    Deeplink,
    /// Direct `RobloxPlayerBeta.exe --app`: always spawns a new window.
    NewWindow,
    /// Sober with the account's own `HOME`: a separate instance every time.
    Sober,
}

impl LaunchKind {
    pub fn human(self) -> &'static str {
        match self {
            LaunchKind::Deeplink => "через deeplink",
            LaunchKind::NewWindow => "отдельное окно (--app)",
            LaunchKind::Sober => "профиль Sober",
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
    /// Texture quality 0..=3 via `DFIntTextureQualityOverride`, `None` keeps
    /// the game default.
    pub texture: Option<u32>,
    /// Client area width, applied only in `--app` mode.
    pub width: u32,
    /// Client area height, applied only in `--app` mode.
    pub height: u32,
    /// Extra allowlisted switches, see [`client_settings::Extras`].
    pub extras: client_settings::Extras,
    /// Spread every client window over the screen in a grid instead of a fixed
    /// size. Takes over from `width`/`height` when no explicit size is set.
    pub tile: bool,
}

impl Limits {
    pub(crate) fn has_settings(self) -> bool {
        self.fps_cap > 0 || self.quality > 0 || self.texture.is_some() || self.extras.any()
    }

    pub(crate) fn has_size(self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Human-readable list of what was written into the client settings.
    pub(crate) fn settings_note(self) -> String {
        let mut parts = Vec::new();
        if self.fps_cap > 0 {
            parts.push(format!("FPS {}", self.fps_cap));
        }
        if self.quality > 0 {
            parts.push(format!("графика {}", self.quality));
        }
        if let Some(texture) = self.texture {
            parts.push(format!("текстуры {texture}"));
        }
        let extras = self.extras.note();
        if !extras.is_empty() {
            parts.push(extras);
        }
        parts.join(", ")
    }
}

/// What actually happened, plus notes worth showing in the status bar.
pub struct LaunchOutcome {
    pub kind: LaunchKind,
    pub notes: Vec<String>,
    /// FILETIME ticks taken just before the client was started: only processes
    /// created after this can be the window of this launch.
    pub launched_at: u64,
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

/// Place ID from a bare number or from a game link.
///
/// Accepts `1818`, `https://www.roblox.com/games/1818/Name`, localized links
/// (`/ru/games/1818/...`) and `...?placeId=1818`. Zero is never a valid place.
pub fn parse_place_id(input: &str) -> Option<u64> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().ok().filter(|id| *id > 0);
    }

    // ASCII lowercasing keeps byte offsets, so positions carry over to `text`.
    let lowered = text.to_ascii_lowercase();
    for marker in ["/games/", "placeid="] {
        let Some(position) = lowered.find(marker) else {
            continue;
        };
        let digits: String = text[position + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if let Some(id) = digits.parse::<u64>().ok().filter(|id| *id > 0) {
            return Some(id);
        }
    }
    None
}

/// Where a launch should land.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Destination {
    /// 0 when only a share link is known; the place comes from resolving it.
    pub place_id: u64,
    /// Private server to join, if the user pasted a link for one.
    pub private: Option<PrivateServer>,
    /// Public server (Job ID) to join, if the user pasted one.
    pub job_id: Option<String>,
}

impl Destination {
    /// A destination is usable when it names a place or carries a server link.
    pub fn is_valid(&self) -> bool {
        self.place_id > 0 || self.private.is_some()
    }
}

/// Private server reference taken from a pasted link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateServer {
    /// `...?privateServerLinkCode=ABC` — ready to use as is.
    LinkCode(String),
    /// `.../share?code=UUID&type=Server` — the code has to be resolved first.
    ShareCode(String),
}

/// Value of `name=` in a query string, up to `&`, `#` or whitespace.
fn query_value(input: &str, name: &str) -> Option<String> {
    // ASCII lowercasing keeps byte offsets, so positions carry over.
    let lowered = input.to_ascii_lowercase();
    let marker = format!("{}=", name.to_ascii_lowercase());
    let start = lowered.find(&marker)? + marker.len();
    let value: String = input[start..]
        .chars()
        .take_while(|unit| *unit != '&' && *unit != '#' && !unit.is_whitespace())
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Private server from a pasted link, if the link is one.
///
/// Two shapes are in use: the old `?privateServerLinkCode=…` link, and the
/// newer `/share?code=…&type=Server` link that has to be resolved through the
/// sharelinks API (it needs the account, so that happens at launch).
pub fn parse_private_server(input: &str) -> Option<PrivateServer> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(code) = query_value(text, "privateServerLinkCode") {
        return Some(PrivateServer::LinkCode(code));
    }
    let lowered = text.to_ascii_lowercase();
    if lowered.contains("/share") && lowered.contains("type=server") {
        if let Some(code) = query_value(text, "code") {
            return Some(PrivateServer::ShareCode(code));
        }
    }
    None
}

/// A Roblox server id: the `GameId`/Job ID from the server list, 36 chars with
/// dashes (`d1e7b1e3-129e-404f-8882-d29d26c72dcd`).
fn is_job_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    text.char_indices().all(|(index, unit)| {
        if index == 8 || index == 13 || index == 18 || index == 23 {
            unit == '-'
        } else {
            unit.is_ascii_hexdigit()
        }
    })
}

/// Job ID from a server link (`...?gameId=<uuid>`) or from a bare id.
///
/// A server browser, the "Join" button on a friend and `roblox.com/games/<id>?
/// gameId=<uuid>` links all carry it; joining by Job ID puts several accounts
/// into exactly the same server, which is the point of running them together.
pub fn parse_job_id(input: &str) -> Option<String> {
    let text = input.trim().trim_matches(['"', '\'']);
    if text.is_empty() {
        return None;
    }
    if is_job_id(text) {
        return Some(text.to_ascii_lowercase());
    }
    let value = query_value(text, "jobId").or_else(|| query_value(text, "gameId"))?;
    is_job_id(&value).then(|| value.to_ascii_lowercase())
}

/// What the user asked for: a place and an optional private server link.
///
/// A private server or server link is accepted in either field: people paste it
/// into the Place ID box as often as into its own field.
pub fn destination(place_input: &str, private_input: &str) -> Destination {
    Destination {
        place_id: parse_place_id(place_input)
            .or_else(|| parse_place_id(private_input))
            .unwrap_or(0),
        private: parse_private_server(private_input).or_else(|| parse_private_server(place_input)),
        job_id: parse_job_id(private_input).or_else(|| parse_job_id(place_input)),
    }
}

/// Where one account should launch.
///
/// An account's own game and server win over the toolbar's. An account that
/// names its own game but no server does *not* inherit the toolbar's server:
/// that server belongs to a different game.
pub fn destination_for_account(
    own_place: &str,
    own_server: &str,
    toolbar_place: &str,
    toolbar_server: &str,
) -> Destination {
    let (own_place, own_server) = (own_place.trim(), own_server.trim());
    let place = if own_place.is_empty() {
        toolbar_place
    } else {
        own_place
    };
    let server = if !own_server.is_empty() {
        own_server
    } else if own_place.is_empty() {
        toolbar_server
    } else {
        ""
    };
    destination(place, server)
}

/// Why a cookie check did not produce a profile.
///
/// The distinction matters for the UI: a dead cookie is a fact about the
/// account, a network hiccup says nothing about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// Roblox answered and refused the cookie.
    Dead(String),
    /// The check itself failed (network, rate limit, unexpected answer).
    Failed(String),
}

impl CheckError {
    pub fn message(&self) -> &str {
        match self {
            CheckError::Dead(message) | CheckError::Failed(message) => message,
        }
    }
}

const RATE_LIMITED: &str = "Roblox просит не частить (HTTP 429): подожди минуту и повтори";

/// Validate the cookie and return the account profile.
pub fn fetch_identity(raw_cookie: &str) -> Result<Profile, CheckError> {
    let cookie = clean_cookie(raw_cookie);
    if cookie.is_empty() {
        return Err(CheckError::Dead("Пустой cookie".into()));
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
                .map_err(|err| CheckError::Failed(format!("Не разобрал ответ Roblox: {err}")))?;
            if identity.id == 0 {
                return Err(CheckError::Dead(
                    "Roblox не вернул профиль по cookie".into(),
                ));
            }
            Ok(Profile {
                id: identity.id,
                name: identity.name,
                display: identity.display_name,
            })
        }
        Err(ureq::Error::Status(401 | 403, _)) => {
            Err(CheckError::Dead("Cookie недействителен или истёк".into()))
        }
        Err(ureq::Error::Status(429, _)) => Err(CheckError::Failed(RATE_LIMITED.into())),
        Err(ureq::Error::Status(code, _)) => {
            Err(CheckError::Failed(format!("Roblox API: HTTP {code}")))
        }
        Err(err) => Err(CheckError::Failed(format!("Нет связи с Roblox: {err}"))),
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
            Err(ureq::Error::Status(429, _)) => {
                return Err(TicketFailure::Fatal(RATE_LIMITED.into()))
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
pub fn build_launch_uri(
    place_id: u64,
    private: Option<&str>,
    job_id: Option<&str>,
    ticket: &str,
) -> String {
    let tracker = random_tracker();
    let launcher = join_url_for(place_id, private, job_id, tracker);
    let launched_at = now_millis();

    format!(
        "roblox-player:1+launchmode:play+gameinfo:{ticket}+launchtime:{launched_at}\
         +placelauncherurl:{}+browsertrackerid:{tracker}\
         +robloxLocale:en_us+gameLocale:en_us+channel:+LaunchExp:InApp",
        urlencoding::encode(&launcher)
    )
}

/// The PlaceLauncher URL for one destination.
///
/// A private server wins over a Job ID: it is the narrower request, and a link
/// that carries both was made for that server anyway.
pub fn join_url_for(
    place_id: u64,
    private: Option<&str>,
    job_id: Option<&str>,
    tracker: u64,
) -> String {
    match (private, job_id) {
        (Some(code), _) => build_private_join_url(place_id, code, tracker),
        (None, Some(job_id)) => build_job_join_url(place_id, job_id, tracker),
        (None, None) => build_join_url(place_id, tracker),
    }
}

/// URL the client asks PlaceLauncher for when joining a place.
pub fn build_join_url(place_id: u64, tracker: u64) -> String {
    format!(
        "{PLACE_LAUNCHER}?request=RequestGame&placeId={place_id}\
         &browserTrackerId={tracker}&isPlayTogetherGame=false"
    )
}

/// Same, but for a private server: the link code is what puts the client into
/// the owner's server instead of a random public one.
pub fn build_private_join_url(place_id: u64, code: &str, tracker: u64) -> String {
    format!(
        "{PLACE_LAUNCHER}?request=RequestPrivateGame&placeId={place_id}\
         &privateServerLinkCode={}&browserTrackerId={tracker}",
        urlencoding::encode(code)
    )
}

/// Same, but for one specific public server (Job ID): this is how several
/// accounts end up in the same place instead of being spread by matchmaking.
pub fn build_job_join_url(place_id: u64, job_id: &str, tracker: u64) -> String {
    format!(
        "{PLACE_LAUNCHER}?request=RequestGameJob&placeId={place_id}\
         &gameId={}&browserTrackerId={tracker}&isPlayTogetherGame=false",
        urlencoding::encode(job_id)
    )
}

/// Turn a `/share?code=…&type=Server` link into a place id and a link code.
///
/// The sharelinks endpoint needs an authenticated session, so the launching
/// account's cookie is used — it is the same account that joins a second later.
pub(crate) fn resolve_share_link(code: &str, cookie: &str) -> Result<(u64, String), String> {
    let body = serde_json::json!({ "linkId": code, "linkType": "Server" });
    let mut token: Option<String> = None;

    for _ in 0..2 {
        let mut request = ureq::post(SHARE_LINKS)
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/json")
            .set("Cookie", &format!(".ROBLOSECURITY={cookie}"));
        if let Some(token) = token.as_deref() {
            request = request.set("X-CSRF-TOKEN", token);
        }

        match request.send_json(body.clone()) {
            Ok(response) => {
                let json: serde_json::Value = response
                    .into_json()
                    .map_err(|err| format!("не понял ответ Roblox про приватный сервер: {err}"))?;
                return share_from_json(&json);
            }
            Err(ureq::Error::Status(_, response)) => {
                // Roblox answers the first POST with 403 and a fresh token.
                let next = response.header("x-csrf-token").map(str::to_string);
                if let Some(next) = next.filter(|next| token.as_deref() != Some(next.as_str())) {
                    token = Some(next);
                    continue;
                }
                let status = response.status();
                let body = response.into_string().unwrap_or_default();
                return Err(format!(
                    "Roblox не принял ссылку на приватный сервер (HTTP {status}): {}",
                    summarize(&body)
                ));
            }
            Err(err) => {
                return Err(format!(
                    "не удалось разобрать ссылку на приватный сервер: {err}"
                ))
            }
        }
    }

    Err("Roblox не принял ссылку на приватный сервер".into())
}

/// Pull the place and the link code out of a resolved share link.
fn share_from_json(json: &serde_json::Value) -> Result<(u64, String), String> {
    let data = &json["privateServerInviteData"];
    let place_id = data["placeId"].as_u64().filter(|id| *id > 0);
    let code = data["linkCode"].as_str().filter(|code| !code.is_empty());

    match (place_id, code) {
        (Some(place_id), Some(code)) => Ok((place_id, code.to_string())),
        _ => Err("ссылка не ведёт на приватный сервер: возможно, он удалён".into()),
    }
}

fn random_tracker() -> u64 {
    rand::random::<u64>() % 9_000_000_000 + 1_000_000_000
}

/// One public server of a place, as the server browser shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    /// Job ID: what `RequestGameJob` needs.
    pub id: String,
    pub playing: u32,
    pub max_players: u32,
    /// Round-trip time the server reports, milliseconds.
    pub ping: u32,
    pub fps: f64,
}

/// Public servers of a place, emptiest first.
///
/// This is how several accounts can be gathered into one server: pick the
/// server here, then launch them all with that Job ID. Roblox rate-limits the
/// list (HTTP 429), so a failure is reported as is instead of being retried.
pub fn fetch_servers(place_id: u64) -> Result<Vec<ServerInfo>, String> {
    let url = format!(
        "{SERVERS_URL}/{place_id}/servers/Public?sortOrder=Asc&excludeFullGames=false&limit=100"
    );
    let response = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .call();

    let json: serde_json::Value = match response {
        Ok(response) => response
            .into_json()
            .map_err(|err| format!("не понял список серверов: {err}"))?,
        Err(ureq::Error::Status(429, _)) => {
            return Err("Roblox просит не частить со списком серверов: подожди минуту".into())
        }
        Err(ureq::Error::Status(404, _)) => return Err("У этой игры нет публичных серверов".into()),
        Err(ureq::Error::Status(code, _)) => return Err(format!("Roblox ответил HTTP {code}")),
        Err(err) => return Err(format!("Нет связи с Roblox: {err}")),
    };

    let mut servers: Vec<ServerInfo> = json["data"]
        .as_array()
        .map(|rows| rows.iter().filter_map(server_from_json).collect())
        .unwrap_or_default();

    // Emptiest first, then the lowest ping: the order in which a request to join
    // is most likely to succeed straight away.
    servers.sort_by(|left, right| {
        left.playing
            .cmp(&right.playing)
            .then(left.ping.cmp(&right.ping))
    });
    Ok(servers)
}

fn server_from_json(row: &serde_json::Value) -> Option<ServerInfo> {
    let id = row["id"].as_str().filter(|id| !id.is_empty())?;
    Some(ServerInfo {
        id: id.to_string(),
        playing: row["playing"].as_u64().unwrap_or(0) as u32,
        max_players: row["maxPlayers"].as_u64().unwrap_or(0) as u32,
        ping: row["ping"].as_u64().unwrap_or(0) as u32,
        fps: row["fps"].as_f64().unwrap_or(0.0),
    })
}

/// Folders that keep the real player next to themselves.
///
/// `Roblox\Versions` is the stock location; bootstrappers install into their
/// own folder instead and never populate the stock one.
const VERSION_OWNERS: [&str; 3] = ["Roblox", "Bloxstrap", "Voidstrap"];

/// Locate `RobloxPlayerBeta.exe`.
///
/// The `roblox-player` protocol handler is trusted only when it points at the
/// player itself. With Bloxstrap and its forks installed it points at
/// `Bloxstrap.exe`, which does not understand player arguments and just opens
/// its own window instead of the game. Those installs keep the player under
/// their own `Versions` folder, so every known location is scanned as well.
/// With several versions installed the most recently updated one wins.
pub fn find_client() -> Option<PathBuf> {
    if let Some(path) = client_from_registry().filter(|path| is_player(path)) {
        return Some(path);
    }

    let mut candidates = Vec::new();
    for root in version_roots() {
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

/// Candidate `Versions` folders for the stock install and known bootstrappers.
fn version_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        for owner in VERSION_OWNERS {
            roots.push(local.join(owner).join("Versions"));
        }
    }
    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(program_files) = std::env::var(variable) {
            roots.push(PathBuf::from(program_files).join("Roblox").join("Versions"));
        }
    }

    roots.retain(|root| root.is_dir());
    roots.sort();
    roots.dedup();
    roots
}

/// Only an executable that is the player can be started with player arguments.
///
/// Both kinds of separators are split on by hand: `Path::file_name` does not
/// treat `\\` as a separator off Windows, which made this check (and its test)
/// depend on the platform.
fn is_player(path: &Path) -> bool {
    path.to_str()
        .and_then(|text| text.rsplit(['\\', '/']).next())
        .is_some_and(|name| name.eq_ignore_ascii_case(CLIENT_EXE))
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
pub(crate) fn run_hidden(command: &mut Command) -> Option<Vec<u8>> {
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
/// Windows (`multi`):
/// * `false` — system `roblox-player:` handler, single window.
/// * `true`  — direct client start with `--app`, new window every call.
///
/// Linux: `account_id` names the account's own Sober profile, so every launch is
/// a separate instance and `multi` has nothing to decide; the details live in
/// [`crate::sober::launch_game`].
///
/// The FPS cap and the start window size go into the client's own settings file
/// (`GlobalBasicSettings_13.xml`), which is where the in-game menu writes them.
/// The quality override goes into `ClientSettings/ClientAppSettings.json` in the
/// client version folder. Both files are read only at startup, so both are
/// rewritten before every launch. The window size is also enforced afterwards,
/// because the client may restore its own size while loading.
pub fn launch_game(
    account_id: &str,
    place: Destination,
    cookie: &str,
    multi: bool,
    limits: Limits,
) -> Result<LaunchOutcome, String> {
    #[cfg(windows)]
    {
        let _ = account_id;
        launch_game_windows(place, cookie, multi, limits)
    }
    #[cfg(not(windows))]
    {
        let _ = multi;
        crate::sober::launch_game(account_id, place, cookie, limits)
    }
}

/// Windows: ticket plus the installed player, either through the protocol
/// handler or as a direct `--app` window.
///
/// Kept compiled on Linux too: the branch is dead there, but it still has to
/// type-check, and one attribute is cheaper than gating every helper.
#[cfg_attr(not(windows), allow(dead_code))]
fn launch_game_windows(
    place: Destination,
    cookie: &str,
    multi: bool,
    limits: Limits,
) -> Result<LaunchOutcome, String> {
    let ticket = fetch_ticket(cookie)?;
    let mut notes = Vec::new();

    // A share link only knows its code; the place and the link code come from
    // Roblox itself, and that call needs the account that is about to join.
    let mut place_id = place.place_id;
    let private = match place.private {
        Some(PrivateServer::LinkCode(code)) => Some(code),
        Some(PrivateServer::ShareCode(code)) => {
            let (resolved_place, link) = resolve_share_link(&code, cookie)?;
            if place_id == 0 {
                place_id = resolved_place;
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
        notes.push(format!("сервер {}", short_job(job_id)));
    }

    let client = find_client();

    if limits.has_settings() {
        match client.as_deref() {
            Some(exe) => {
                match client_settings::apply(
                    exe,
                    limits.fps_cap,
                    limits.quality,
                    limits.texture,
                    limits.extras,
                ) {
                    Ok(Some(_)) => notes.push(limits.settings_note()),
                    Ok(None) => {}
                    Err(err) => notes.push(format!("настройки клиента не применены: {err}")),
                }
            }
            None => notes.push("клиент не найден: FPS/графику не применил".into()),
        }
    }

    // The client's own settings file is what actually caps frames and sets the
    // start size: the matching fast flags are denied by the client.
    if limits.fps_cap > 0 || limits.has_size() {
        if let Err(err) = game_settings::apply(limits.fps_cap, limits.width, limits.height) {
            notes.push(format!("настройки игры не применены: {err}"));
        }
    }

    if multi {
        let exe =
            client.ok_or_else(|| format!("Не нашёл {CLIENT_EXE}. Клиент Roblox установлен?"))?;
        let tracker = random_tracker();
        let join_url = join_url_for(
            place_id,
            private.as_deref(),
            place.job_id.as_deref(),
            tracker,
        );
        let launched_at = processes::now_filetime();
        let child = spawn_new_window(&exe, &ticket, &join_url)?;

        if limits.tile {
            // The grid decides both the position and the size: it recomputes
            // itself as more windows appear.
            window::arrange();
            notes.push("раскладка сеткой".to_string());
        } else if limits.has_size() {
            window::resize_when_ready(child.id(), limits.width, limits.height);
            notes.push(format!("окно {}×{}", limits.width, limits.height));
        }

        return Ok(LaunchOutcome {
            kind: LaunchKind::NewWindow,
            notes,
            launched_at,
        });
    }

    if limits.has_size() {
        notes.push("размер окна задаётся только в режиме нескольких окон".into());
    }

    let uri = build_launch_uri(
        place_id,
        private.as_deref(),
        place.job_id.as_deref(),
        &ticket,
    );
    let launched_at = processes::now_filetime();
    open::that(uri).map_err(|err| format!("Не удалось открыть клиент Roblox: {err}"))?;
    Ok(LaunchOutcome {
        kind: LaunchKind::Deeplink,
        notes,
        launched_at,
    })
}

/// First eight characters of a Job ID, enough to tell two servers apart.
pub(crate) fn short_job(job_id: &str) -> String {
    job_id.chars().take(8).collect()
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
            texture: Some(1),
            width: 960,
            height: 540,
            extras: crate::client_settings::Extras {
                no_grass: true,
                ..crate::client_settings::Extras::default()
            },
            tile: false,
        };
        assert!(limits.has_settings());
        assert!(limits.has_size());
        assert_eq!(
            limits.settings_note(),
            "FPS 60, графика 1, текстуры 1, без травы"
        );
    }

    #[test]
    fn private_link_code_is_recognized() {
        let link = "https://www.roblox.com/games/1818/Crossroads?privateServerLinkCode=AbC123xyz";
        assert_eq!(
            parse_private_server(link),
            Some(PrivateServer::LinkCode("AbC123xyz".into()))
        );

        // The same link also names the place.
        let place = destination(link, "");
        assert_eq!(place.place_id, 1818);
        assert!(place.is_valid());
        assert!(place.private.is_some());
    }

    #[test]
    fn share_link_needs_resolving() {
        let link = "https://www.roblox.com/share?code=0f1e2d3c-4b5a&type=Server";
        assert_eq!(
            parse_private_server(link),
            Some(PrivateServer::ShareCode("0f1e2d3c-4b5a".into()))
        );

        // A share link carries no place id, but it is still launchable.
        let place = destination("", link);
        assert_eq!(place.place_id, 0);
        assert!(place.is_valid());
    }

    #[test]
    fn ordinary_links_are_not_private() {
        assert_eq!(parse_private_server("1818"), None);
        assert_eq!(
            parse_private_server("https://www.roblox.com/games/1818/Name"),
            None
        );
        assert_eq!(parse_private_server(""), None);
        // A share link for an experience is not a server link.
        assert_eq!(
            parse_private_server("https://www.roblox.com/share?code=abc"),
            None
        );
    }

    #[test]
    fn private_join_url_asks_for_the_private_game() {
        let url = build_private_join_url(1818, "AbC123", 42);
        assert!(url.starts_with(PLACE_LAUNCHER));
        assert!(url.contains("request=RequestPrivateGame"));
        assert!(url.contains("placeId=1818"));
        assert!(url.contains("privateServerLinkCode=AbC123"));
        assert!(url.contains("browserTrackerId=42"));
    }

    #[test]
    fn deeplink_carries_the_private_launcher() {
        let uri = build_launch_uri(1818, Some("CODE123"), None, "TICKET");
        assert!(uri.contains("gameinfo:TICKET"));
        assert!(uri.contains("RequestPrivateGame"));
        assert!(uri.contains("CODE123"));
    }

    #[test]
    fn deeplink_carries_the_job_id() {
        let job = "d1e7b1e3-129e-404f-8882-d29d26c72dcd";
        let uri = build_launch_uri(1818, None, Some(job), "TICKET");
        assert!(uri.contains("RequestGameJob"));
        assert!(uri.contains(job));

        // A private server wins over a Job ID when both are somehow present.
        let both = join_url_for(1818, Some("CODE123"), Some(job), 42);
        assert!(both.contains("RequestPrivateGame"));
        assert!(!both.contains("RequestGameJob"));
    }

    #[test]
    fn job_id_is_recognized_in_both_shapes() {
        let job = "d1e7b1e3-129e-404f-8882-d29d26c72dcd";
        assert_eq!(parse_job_id(job), Some(job.to_string()));

        // Upper case is fine; the launcher expects lower case.
        assert_eq!(
            parse_job_id("D1E7B1E3-129E-404F-8882-D29D26C72DCD"),
            Some(job.to_string())
        );

        let link = format!("https://www.roblox.com/games/1818/Name?gameId={job}");
        assert_eq!(parse_job_id(&link), Some(job.to_string()));
        assert_eq!(destination(&link, "").job_id, Some(job.to_string()));
        assert_eq!(destination("1818", job).job_id, Some(job.to_string()));

        // Ordinary links and ids are not servers.
        assert_eq!(parse_job_id("https://www.roblox.com/games/1818/Name"), None);
        assert_eq!(parse_job_id("1818"), None);
        assert_eq!(parse_job_id(""), None);
        // A private server link is not a Job ID either.
        assert_eq!(
            parse_job_id("https://www.roblox.com/games/1/N?privateServerLinkCode=abc"),
            None
        );
    }

    #[test]
    fn server_row_is_read_from_the_api_answer() {
        let row = serde_json::json!({
            "id": "d1e7b1e3-129e-404f-8882-d29d26c72dcd",
            "playing": 7,
            "maxPlayers": 15,
            "ping": 131,
            "fps": 59.97
        });
        let server = server_from_json(&row).expect("server");
        assert_eq!(server.id, "d1e7b1e3-129e-404f-8882-d29d26c72dcd");
        assert_eq!(server.playing, 7);
        assert_eq!(server.max_players, 15);
        assert_eq!(server.ping, 131);
        assert!((server.fps - 59.97).abs() < 0.01);

        // A row without an id is not a server.
        assert!(server_from_json(&serde_json::json!({ "playing": 1 })).is_none());
    }

    #[test]
    fn job_join_url_asks_for_that_server() {
        let job = "d1e7b1e3-129e-404f-8882-d29d26c72dcd";
        let url = build_job_join_url(1818, job, 42);
        assert!(url.starts_with(PLACE_LAUNCHER));
        assert!(url.contains("request=RequestGameJob"));
        assert!(url.contains("placeId=1818"));
        assert!(url.contains(&format!("gameId={job}")));
        assert!(url.contains("browserTrackerId=42"));
    }

    #[test]
    fn share_response_gives_place_and_code() {
        let json = serde_json::json!({
            "privateServerInviteData": { "placeId": 1818, "linkCode": "abc" }
        });
        assert_eq!(share_from_json(&json), Ok((1818, "abc".to_string())));

        let empty = serde_json::json!({ "privateServerInviteData": null });
        assert!(share_from_json(&empty).is_err());
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
    fn only_the_player_executable_is_accepted() {
        // Bloxstrap registers itself as the protocol handler; launching it with
        // player arguments only opens its own window.
        assert!(is_player(Path::new(
            r"C:\Users\me\AppData\Local\Bloxstrap\Versions\version-abc\RobloxPlayerBeta.exe"
        )));
        assert!(is_player(Path::new(r"C:\Roblox\robloxplayerbeta.EXE")));
        assert!(!is_player(Path::new(
            r"C:\Users\me\AppData\Local\Bloxstrap\Bloxstrap.exe"
        )));
        assert!(!is_player(Path::new(r"C:\Windows\System32\reg.exe")));
    }

    #[test]
    fn place_id_from_number_or_link() {
        assert_eq!(parse_place_id("1818"), Some(1818));
        assert_eq!(parse_place_id("  1818 \n"), Some(1818));
        assert_eq!(
            parse_place_id("https://www.roblox.com/games/920587237/Adopt-Me"),
            Some(920587237)
        );
        assert_eq!(
            parse_place_id("https://www.roblox.com/ru/games/1818/Crossroads?x=1"),
            Some(1818)
        );
        assert_eq!(
            parse_place_id("https://www.roblox.com/Game?PlaceId=1818&foo=2"),
            Some(1818)
        );
    }

    #[test]
    fn place_id_rejects_garbage_and_zero() {
        assert_eq!(parse_place_id(""), None);
        assert_eq!(parse_place_id("abc"), None);
        assert_eq!(parse_place_id("0"), None);
        assert_eq!(parse_place_id("https://www.roblox.com/games/0/x"), None);
        assert_eq!(parse_place_id("https://example.com/"), None);
        assert_eq!(parse_place_id("99999999999999999999999"), None);
    }

    #[test]
    fn check_error_exposes_its_message() {
        assert_eq!(CheckError::Dead("x".into()).message(), "x");
        assert_eq!(CheckError::Failed("y".into()).message(), "y");
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

#[cfg(test)]
mod account_destination_tests {
    use super::*;

    const JOB: &str = "d1e7b1e3-129e-404f-8882-d29d26c72dcd";

    #[test]
    fn the_toolbar_is_used_when_the_account_has_nothing_of_its_own() {
        let dest = destination_for_account("", "", "1818", JOB);
        assert_eq!(dest.place_id, 1818);
        assert_eq!(dest.job_id.as_deref(), Some(JOB));
    }

    #[test]
    fn an_own_game_replaces_the_toolbar_game_and_its_server() {
        let dest = destination_for_account("920587237", "", "1818", JOB);
        assert_eq!(dest.place_id, 920_587_237);
        // The toolbar's server is for another game: not inherited.
        assert!(dest.job_id.is_none());
        assert!(dest.private.is_none());
    }

    #[test]
    fn an_own_server_alone_joins_the_toolbar_game() {
        let dest = destination_for_account("", JOB, "1818", "");
        assert_eq!(dest.place_id, 1818);
        assert_eq!(dest.job_id.as_deref(), Some(JOB));
    }

    #[test]
    fn own_game_and_server_together() {
        let dest = destination_for_account(
            "https://www.roblox.com/games/42/Name",
            "https://www.roblox.com/games/42/Name?privateServerLinkCode=ABC",
            "1818",
            JOB,
        );
        assert_eq!(dest.place_id, 42);
        assert_eq!(dest.private, Some(PrivateServer::LinkCode("ABC".into())));
    }

    #[test]
    fn nothing_anywhere_is_not_a_destination() {
        assert!(!destination_for_account("", "", "", "").is_valid());
    }
}
