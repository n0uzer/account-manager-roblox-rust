//! Read-only questions to Roblox: who is online, what an account looks like,
//! which game matches a name, where a player is playing right now.
//!
//! Like `roblox.rs`, the cookie goes only to Roblox's own hosts. Nothing here
//! changes anything on the account.
//!
//! The endpoints and response shapes below come from the public web API as it was
//! last documented; Roblox changes it without notice. Parsing is therefore
//! deliberately forgiving (missing fields become `None`, game search walks the
//! whole JSON instead of expecting one layout) and every failure is returned as a
//! message for the status bar rather than a panic.

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::roblox::{clean_cookie, parse_job_id, USER_AGENT};

const PRESENCE_URL: &str = "https://presence.roblox.com/v1/presence/users";
const USERS_URL: &str = "https://users.roblox.com/v1/users";
const USERNAMES_URL: &str = "https://users.roblox.com/v1/usernames/users";
const ECONOMY_URL: &str = "https://economy.roblox.com/v1/users";
const PREMIUM_URL: &str = "https://premiumfeatures.roblox.com/v1/users";
const OMNI_SEARCH_URL: &str = "https://apis.roblox.com/search-api/omni-search";
const GAME_LIST_URL: &str = "https://games.roblox.com/v1/games/list";

/// How many users one presence request asks about.
const PRESENCE_CHUNK: usize = 50;
/// Search results shown at most.
const MAX_GAME_HITS: usize = 30;

// ------------------------------------------------------------------- HTTP
fn cookie_header(raw: &str) -> String {
    format!(".ROBLOSECURITY={}", clean_cookie(raw))
}

/// Human text for a failed request.
fn describe(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(429, _) => "Roblox просит не частить (HTTP 429): подожди минуту".into(),
        ureq::Error::Status(401 | 403, _) => "Roblox отказал в доступе (cookie или права)".into(),
        ureq::Error::Status(code, _) => format!("Roblox ответил HTTP {code}"),
        err => format!("Нет связи с Roblox: {err}"),
    }
}

fn get_json(url: &str, cookie: Option<&str>) -> Result<Value, String> {
    let mut request = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json");
    if let Some(cookie) = cookie {
        request = request.set("Cookie", &cookie_header(cookie));
    }
    request
        .call()
        .map_err(describe)?
        .into_json()
        .map_err(|err| format!("Не разобрал ответ Roblox: {err}"))
}

/// POST JSON. Roblox answers the first write with 403 and a CSRF token in a
/// header; the request is repeated once with that token.
fn post_json(url: &str, cookie: Option<&str>, body: &Value) -> Result<Value, String> {
    let mut token: Option<String> = None;
    for _ in 0..2 {
        let mut request = ureq::post(url)
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/json");
        if let Some(cookie) = cookie {
            request = request.set("Cookie", &cookie_header(cookie));
        }
        if let Some(token) = token.as_deref() {
            request = request.set("X-CSRF-TOKEN", token);
        }

        match request.send_json(body.clone()) {
            Ok(response) => {
                return response
                    .into_json()
                    .map_err(|err| format!("Не разобрал ответ Roblox: {err}"))
            }
            Err(ureq::Error::Status(403, response)) => {
                let next = response.header("x-csrf-token").map(str::to_string);
                match next.filter(|next| token.as_deref() != Some(next.as_str())) {
                    Some(next) => token = Some(next),
                    None => return Err(describe(ureq::Error::Status(403, response))),
                }
            }
            Err(err) => return Err(describe(err)),
        }
    }
    Err("Roblox не принял запрос".into())
}

// --------------------------------------------------------------- presence
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceKind {
    Offline,
    Online,
    InGame,
    Studio,
}

impl PresenceKind {
    pub fn label(self) -> &'static str {
        match self {
            PresenceKind::Offline => "офлайн",
            PresenceKind::Online => "онлайн",
            PresenceKind::InGame => "в игре",
            PresenceKind::Studio => "в Studio",
        }
    }

    fn from_code(code: u64) -> Self {
        match code {
            1 => PresenceKind::Online,
            2 => PresenceKind::InGame,
            3 => PresenceKind::Studio,
            _ => PresenceKind::Offline,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub kind: PresenceKind,
    /// Place the user is in, 0 when unknown or hidden.
    pub place_id: u64,
    /// Job ID of the server, if Roblox shows it.
    pub game_id: Option<String>,
    /// Name of the game as Roblox reports it.
    pub location: String,
}

/// `{"userPresences":[{"userId":1,"userPresenceType":2,"placeId":…}]}`
pub fn presences_from_json(json: &Value) -> HashMap<u64, Presence> {
    let mut found = HashMap::new();
    let Some(rows) = json["userPresences"].as_array() else {
        return found;
    };
    for row in rows {
        let Some(user_id) = row["userId"].as_u64().filter(|id| *id > 0) else {
            continue;
        };
        // The root place is the one a join link needs; `placeId` can be a
        // sub-place of the same experience.
        let place_id = row["rootPlaceId"]
            .as_u64()
            .filter(|id| *id > 0)
            .or_else(|| row["placeId"].as_u64())
            .unwrap_or(0);
        found.insert(
            user_id,
            Presence {
                kind: PresenceKind::from_code(row["userPresenceType"].as_u64().unwrap_or(0)),
                place_id,
                game_id: row["gameId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                location: row["lastLocation"].as_str().unwrap_or("").to_string(),
            },
        );
    }
    found
}

/// Presence of several users in as few requests as possible. The cookie of any
/// signed-in account will do; presence is not tied to the asker.
pub fn fetch_presence(cookie: &str, user_ids: &[u64]) -> Result<HashMap<u64, Presence>, String> {
    let mut all = HashMap::new();
    for chunk in user_ids.chunks(PRESENCE_CHUNK) {
        let json = post_json(PRESENCE_URL, Some(cookie), &json!({ "userIds": chunk }))?;
        all.extend(presences_from_json(&json));
    }
    Ok(all)
}

// ---------------------------------------------------------------- details
/// What the "details" view shows for one account. Every part is optional: an
/// endpoint that fails leaves its field empty instead of failing the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    /// Account creation date as Roblox gives it (ISO 8601).
    pub created: Option<String>,
    pub robux: Option<u64>,
    pub premium: Option<bool>,
}

pub fn robux_from_json(json: &Value) -> Option<u64> {
    json["robux"].as_u64()
}

/// The membership check answers a bare `true`/`false`; an object with a flag is
/// accepted as well.
pub fn premium_from_json(json: &Value) -> Option<bool> {
    match json {
        Value::Bool(value) => Some(*value),
        Value::Object(map) => map
            .get("isPremium")
            .or_else(|| map.get("isPremiumMember"))
            .and_then(Value::as_bool),
        _ => None,
    }
}

pub fn fetch_details(cookie: &str, user_id: u64) -> Result<Details, String> {
    let created = get_json(&format!("{USERS_URL}/{user_id}"), None);
    let robux = get_json(&format!("{ECONOMY_URL}/{user_id}/currency"), Some(cookie));
    let premium = get_json(
        &format!("{PREMIUM_URL}/{user_id}/validate-membership"),
        Some(cookie),
    );

    let details = Details {
        created: created
            .as_ref()
            .ok()
            .and_then(|json| json["created"].as_str())
            .map(str::to_string),
        robux: robux.as_ref().ok().and_then(robux_from_json),
        premium: premium.as_ref().ok().and_then(premium_from_json),
    };

    if details == Details::default() {
        // Nothing came back at all: report why, preferring the first error.
        let reason = [created, robux, premium]
            .into_iter()
            .find_map(Result::err)
            .unwrap_or_else(|| "Roblox не вернул данные".into());
        return Err(reason);
    }
    Ok(details)
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `2019-05-02T12:34:56.789Z` → (2019, 5, 2)
fn parse_iso_date(text: &str) -> Option<(i64, i64, i64)> {
    let date = text.get(..10)?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day) && year > 1990)
        .then_some((year, month, day))
}

/// Account age in words, from the creation date and the current Unix time.
pub fn age_text(created: &str, now_unix: i64) -> Option<String> {
    let (year, month, day) = parse_iso_date(created)?;
    let days = now_unix.div_euclid(86_400) - days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let years = days / 365;
    let months = (days % 365) / 30;
    Some(if years > 0 {
        format!("{years} г. {months} мес.")
    } else if months > 0 {
        format!("{months} мес.")
    } else {
        format!("{days} дн.")
    })
}

// ------------------------------------------------------------ game search
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameHit {
    pub name: String,
    pub place_id: u64,
    pub players: u64,
}

/// Collect every object that looks like a game: a `name` plus a place id.
///
/// Both the search page API and the older game list wrap their rows differently,
/// so the whole document is walked instead of one fixed path.
pub fn games_from_json(json: &Value) -> Vec<GameHit> {
    fn walk(value: &Value, out: &mut Vec<GameHit>) {
        match value {
            Value::Object(map) => {
                let place_id = map
                    .get("rootPlaceId")
                    .or_else(|| map.get("placeId"))
                    .and_then(Value::as_u64)
                    .filter(|id| *id > 0);
                if let (Some(name), Some(place_id)) =
                    (map.get("name").and_then(Value::as_str), place_id)
                {
                    let players = map
                        .get("playerCount")
                        .or_else(|| map.get("playing"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    out.push(GameHit {
                        name: name.to_string(),
                        place_id,
                        players,
                    });
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            _ => {}
        }
    }

    let mut hits = Vec::new();
    walk(json, &mut hits);

    let mut seen = std::collections::HashSet::new();
    hits.retain(|hit| seen.insert(hit.place_id));
    hits.truncate(MAX_GAME_HITS);
    hits
}

/// Random id for the `sessionId` the search endpoint wants.
fn session_id() -> String {
    let hex = format!("{:032x}", rand::random::<u128>());
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Games matching a name. The newer search endpoint is tried first, the older
/// list as a fallback.
pub fn search_games(query: &str) -> Result<Vec<GameHit>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Введи название игры".into());
    }
    let encoded = urlencoding::encode(query);

    let omni = get_json(
        &format!(
            "{OMNI_SEARCH_URL}?searchQuery={encoded}&sessionId={}&pageType=all",
            session_id()
        ),
        None,
    );
    if let Ok(json) = &omni {
        let hits = games_from_json(json);
        if !hits.is_empty() {
            return Ok(hits);
        }
    }

    let list = get_json(&format!("{GAME_LIST_URL}?model.keyword={encoded}"), None);
    match list {
        Ok(json) => {
            let hits = games_from_json(&json);
            if hits.is_empty() {
                Err("Ничего не нашлось по этому названию".into())
            } else {
                Ok(hits)
            }
        }
        // Report the failure of the first attempt if both failed: it is the
        // one that matters when Roblox is simply unreachable.
        Err(second) => Err(omni.err().unwrap_or(second)),
    }
}

// ---------------------------------------------------------------- follow
/// Who to look for: a numeric id or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerRef {
    Id(u64),
    Name(String),
}

/// Accepts a user id, a name, or a profile link (`roblox.com/users/123/profile`).
pub fn parse_player_input(input: &str) -> Option<PlayerRef> {
    let text = input.trim().trim_matches(['"', '\'']);
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text.parse().ok().filter(|id| *id > 0).map(PlayerRef::Id);
    }
    let lowered = text.to_ascii_lowercase();
    if let Some(position) = lowered.find("/users/") {
        let digits: String = text[position + "/users/".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        return digits.parse().ok().filter(|id| *id > 0).map(PlayerRef::Id);
    }
    // Names are letters, digits and underscores; anything else is not one.
    text.chars()
        .all(|unit| unit.is_ascii_alphanumeric() || unit == '_')
        .then(|| PlayerRef::Name(text.to_string()))
}

/// Where a player is playing right now, ready to be put into the toolbar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub user_id: u64,
    pub name: String,
    pub place_id: u64,
    pub job_id: Option<String>,
    pub location: String,
}

fn resolve_player(who: &PlayerRef) -> Result<(u64, String), String> {
    match who {
        PlayerRef::Id(id) => {
            let json = get_json(&format!("{USERS_URL}/{id}"), None)?;
            let name = json["name"].as_str().unwrap_or("").to_string();
            Ok((
                *id,
                if name.is_empty() {
                    id.to_string()
                } else {
                    name
                },
            ))
        }
        PlayerRef::Name(name) => {
            let json = post_json(
                USERNAMES_URL,
                None,
                &json!({ "usernames": [name], "excludeBannedUsers": true }),
            )?;
            let row = &json["data"][0];
            match row["id"].as_u64().filter(|id| *id > 0) {
                Some(id) => Ok((id, row["name"].as_str().unwrap_or(name).to_string())),
                None => Err(format!("Игрок «{name}» не найден")),
            }
        }
    }
}

/// Find a player and the server he is in. `cookie` is any signed-in account:
/// presence is only shown to signed-in users, and only what that user may see.
pub fn find_player(cookie: &str, input: &str) -> Result<Target, String> {
    let who = parse_player_input(input)
        .ok_or_else(|| "Введи ник, ID или ссылку на профиль игрока".to_string())?;
    let (user_id, name) = resolve_player(&who)?;

    let presences = fetch_presence(cookie, &[user_id])?;
    let presence = presences
        .get(&user_id)
        .ok_or_else(|| format!("Roblox не вернул статус игрока {name}"))?;

    if presence.kind != PresenceKind::InGame || presence.place_id == 0 {
        return Err(format!(
            "{name} сейчас не в игре ({}) или скрывает, где играет",
            presence.kind.label()
        ));
    }
    Ok(Target {
        user_id,
        name,
        place_id: presence.place_id,
        // Only a well-formed server id is passed on; anything else would make
        // the join request fail.
        job_id: presence.game_id.as_deref().and_then(parse_job_id),
        location: presence.location.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_rows_are_parsed() {
        let json = json!({ "userPresences": [
            { "userId": 11, "userPresenceType": 2, "placeId": 5, "rootPlaceId": 1818,
              "gameId": "d1e7b1e3-129e-404f-8882-d29d26c72dcd", "lastLocation": "Crossroads" },
            { "userId": 22, "userPresenceType": 1, "placeId": null, "rootPlaceId": null,
              "gameId": null, "lastLocation": "Website" },
            { "userId": 33, "userPresenceType": 0 },
            { "userPresenceType": 2 }
        ]});
        let found = presences_from_json(&json);

        assert_eq!(found.len(), 3);
        let playing = &found[&11];
        assert_eq!(playing.kind, PresenceKind::InGame);
        // The root place wins over the sub-place.
        assert_eq!(playing.place_id, 1818);
        assert_eq!(
            playing.game_id.as_deref(),
            Some("d1e7b1e3-129e-404f-8882-d29d26c72dcd")
        );
        assert_eq!(found[&22].kind, PresenceKind::Online);
        assert_eq!(found[&22].place_id, 0);
        assert!(found[&22].game_id.is_none());
        assert_eq!(found[&33].kind, PresenceKind::Offline);
    }

    #[test]
    fn presence_of_garbage_is_empty() {
        assert!(presences_from_json(&json!({})).is_empty());
        assert!(presences_from_json(&json!({ "userPresences": "x" })).is_empty());
    }

    #[test]
    fn robux_and_premium_shapes() {
        assert_eq!(robux_from_json(&json!({ "robux": 1234 })), Some(1234));
        assert_eq!(robux_from_json(&json!({ "errors": [] })), None);
        assert_eq!(premium_from_json(&json!(true)), Some(true));
        assert_eq!(premium_from_json(&json!(false)), Some(false));
        assert_eq!(premium_from_json(&json!({ "isPremium": true })), Some(true));
        assert_eq!(premium_from_json(&json!("yes")), None);
    }

    #[test]
    fn civil_days_match_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(2026, 11, 19), 20_776);
    }

    #[test]
    fn age_is_written_in_words() {
        // 2026-11-19 00:00 UTC
        let now = 20_776 * 86_400;
        assert_eq!(
            age_text("2023-11-19T10:00:00.123Z", now).as_deref(),
            Some("3 г. 0 мес.")
        );
        assert_eq!(
            age_text("2026-08-19T00:00:00Z", now).as_deref(),
            Some("3 мес.")
        );
        assert_eq!(
            age_text("2026-11-14T00:00:00Z", now).as_deref(),
            Some("5 дн.")
        );
        // From the future, or not a date at all.
        assert_eq!(age_text("2027-01-01T00:00:00Z", now), None);
        assert_eq!(age_text("soon", now), None);
        assert_eq!(age_text("", now), None);
    }

    #[test]
    fn games_are_found_in_either_layout() {
        let omni = json!({ "searchResults": [
            { "contentGroupType": "Game", "contents": [
                { "contentType": "Game", "universeId": 1, "name": "Adopt Me",
                  "rootPlaceId": 920587237u64, "playerCount": 100 },
                { "name": "Adopt Me Clone", "rootPlaceId": 42, "playerCount": 3 }
            ]}
        ]});
        let hits = games_from_json(&omni);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "Adopt Me");
        assert_eq!(hits[0].place_id, 920_587_237);
        assert_eq!(hits[0].players, 100);

        let list = json!({ "games": [
            { "name": "Old Layout", "placeId": 7, "playerCount": 9 }
        ]});
        assert_eq!(games_from_json(&list)[0].place_id, 7);
    }

    #[test]
    fn game_hits_are_deduplicated_and_limited() {
        let rows: Vec<Value> = (1..=60u64)
            .map(|id| json!({ "name": format!("g{id}"), "placeId": id }))
            .chain(std::iter::once(json!({ "name": "again", "placeId": 1 })))
            .collect();
        let hits = games_from_json(&json!({ "games": rows }));
        assert_eq!(hits.len(), MAX_GAME_HITS);
        assert_eq!(hits.iter().filter(|hit| hit.place_id == 1).count(), 1);
    }

    #[test]
    fn objects_without_a_place_are_not_games() {
        let json = json!({ "creator": { "name": "someone", "id": 5 } });
        assert!(games_from_json(&json).is_empty());
    }

    #[test]
    fn player_input_shapes() {
        assert_eq!(parse_player_input("12345"), Some(PlayerRef::Id(12345)));
        assert_eq!(
            parse_player_input("https://www.roblox.com/users/987/profile"),
            Some(PlayerRef::Id(987))
        );
        assert_eq!(
            parse_player_input("  Builder_Man  "),
            Some(PlayerRef::Name("Builder_Man".into()))
        );
        assert_eq!(parse_player_input("0"), None);
        assert_eq!(parse_player_input(""), None);
        assert_eq!(parse_player_input("two words"), None);
    }

    #[test]
    fn session_id_has_the_uuid_shape() {
        let id = session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert!(id
            .chars()
            .all(|unit| unit == '-' || unit.is_ascii_hexdigit()));
    }
}
