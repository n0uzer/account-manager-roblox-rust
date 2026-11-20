//! Everything that asks Roblox a read-only question: the check right after
//! unlocking, online status, account details, game search and "go to player".

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, RichText};

use super::{shorten, App, Check, Tone};
use crate::jobs::Job;
use crate::lookup::{self, Details, GameHit, PresenceKind, Target};

/// How often online status is refreshed while it is switched on.
const PRESENCE_EVERY: Duration = Duration::from_secs(60);
/// Wait after a failed refresh, so a rate limit is not hammered.
const PRESENCE_BACKOFF: Duration = Duration::from_secs(300);
/// Names listed in the unlock summary before "and N more".
const NAMES_SHOWN: usize = 5;

// ----------------------------------------------------------------- helpers
/// `1234567` → `1 234 567`.
fn group_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

/// One line for the selection bar: age, balance, subscription.
fn details_text(details: &Details, now: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(age) = details
        .created
        .as_deref()
        .and_then(|created| lookup::age_text(created, now))
    {
        parts.push(format!("возраст {age}"));
    }
    if let Some(robux) = details.robux {
        parts.push(format!("{} R$", group_thousands(robux)));
    }
    match details.premium {
        Some(true) => parts.push("Premium".into()),
        Some(false) => parts.push("без Premium".into()),
        None => {}
    }
    if parts.is_empty() {
        "нет данных".to_string()
    } else {
        parts.join(" · ")
    }
}

/// Tone and text of the summary after the check that runs on unlock.
fn startup_summary(valid: usize, unknown: usize, dead: &[String]) -> (Tone, String) {
    if dead.is_empty() && unknown == 0 {
        return (Tone::Ok, format!("Cookie проверены: рабочих {valid}"));
    }

    let mut text = format!("Cookie проверены: рабочих {valid}");
    if !dead.is_empty() {
        let shown: Vec<&str> = dead.iter().take(NAMES_SHOWN).map(String::as_str).collect();
        text.push_str(&format!(", вышли из аккаунта: {}", shown.join(", ")));
        if dead.len() > NAMES_SHOWN {
            text.push_str(&format!(" и ещё {}", dead.len() - NAMES_SHOWN));
        }
    }
    if unknown > 0 {
        text.push_str(&format!(", не удалось проверить: {unknown}"));
    }
    (Tone::Warn, text)
}

// ------------------------------------------------------------------- state
pub enum DetailsState {
    Loading,
    Ready(Details),
    Failed(String),
}

/// What a finished cookie check said, for the unlock summary.
pub enum Outcome {
    Valid,
    Dead,
    Unknown,
}

/// The check started by unlocking: ids still to come back and the tally.
pub struct StartupCheck {
    waiting: HashSet<String>,
    valid: usize,
    unknown: usize,
    dead: Vec<String>,
}

pub struct GameSearch {
    query: String,
    loading: bool,
    results: Vec<GameHit>,
    error: Option<String>,
    focus: bool,
}

pub struct PlayerSearch {
    input: String,
    loading: bool,
    error: Option<String>,
    focus: bool,
}

impl App {
    // ------------------------------------------------------- unlock check
    /// Check every cookie right after the store is opened.
    pub(super) fn start_unlock_check(&mut self) {
        if !self.config.check_on_unlock {
            return;
        }
        let ids: Vec<String> = self
            .store
            .as_ref()
            .map(|store| {
                store
                    .accounts
                    .iter()
                    .filter(|account| !account.cookie.is_empty())
                    .map(|account| account.id.clone())
                    .collect()
            })
            .unwrap_or_default();

        let started: HashSet<String> = ids
            .into_iter()
            .filter(|id| self.start_validate(id))
            .collect();
        if started.is_empty() {
            return;
        }
        self.info(format!(
            "Проверяю cookie при открытии: {} шт.",
            started.len()
        ));
        self.startup_check = Some(StartupCheck {
            waiting: started,
            valid: 0,
            unknown: 0,
            dead: Vec::new(),
        });
    }

    /// Count a finished check into the unlock summary and report it when the
    /// last one is in.
    pub(super) fn note_startup_check(&mut self, id: &str, label: &str, outcome: Outcome) {
        let Some(check) = self.startup_check.as_mut() else {
            return;
        };
        if !check.waiting.remove(id) {
            return;
        }
        match outcome {
            Outcome::Valid => check.valid += 1,
            Outcome::Unknown => check.unknown += 1,
            Outcome::Dead => check.dead.push(label.to_string()),
        }
        if !check.waiting.is_empty() {
            return;
        }
        if let Some(check) = self.startup_check.take() {
            let (tone, text) = startup_summary(check.valid, check.unknown, &check.dead);
            self.say(tone, text);
        }
    }

    // ----------------------------------------------------------- presence
    /// Cookie to ask Roblox with: the selected account if it works, otherwise
    /// the first one that has not been refused.
    fn lookup_cookie(&self) -> Option<String> {
        let store = self.store.as_ref()?;
        let usable = |id: &str| !matches!(self.checks.get(id), Some(Check::Dead(_)));

        let selected = self
            .selected_account()
            .filter(|account| !account.cookie.is_empty() && usable(&account.id));
        let any = || {
            store
                .accounts
                .iter()
                .find(|account| !account.cookie.is_empty() && usable(&account.id))
        };
        selected.or_else(any).map(|account| account.cookie.clone())
    }

    /// Refresh online status once a minute while it is switched on.
    pub(super) fn tick_presence(&mut self, ctx: &egui::Context) {
        if !self.config.live_status || self.store.is_none() {
            return;
        }
        let now = Instant::now();
        let at = *self.presence_next.get_or_insert(now);
        if now < at {
            ctx.request_repaint_after(at - now);
            return;
        }
        if self.presence_loading {
            return;
        }

        let user_ids: Vec<u64> = self
            .store
            .as_ref()
            .map(|store| {
                store
                    .accounts
                    .iter()
                    .map(|account| account.user_id)
                    .filter(|id| *id > 0)
                    .collect()
            })
            .unwrap_or_default();
        let Some(cookie) = self.lookup_cookie().filter(|_| !user_ids.is_empty()) else {
            // Nobody to ask about, or nobody to ask with: look again later.
            self.presence_next = Some(now + PRESENCE_EVERY);
            return;
        };

        self.presence_loading = true;
        self.presence_next = Some(now + PRESENCE_EVERY);
        self.worker.send(Job::Presence { cookie, user_ids });
    }

    pub(super) fn handle_presence(
        &mut self,
        result: Result<std::collections::HashMap<u64, lookup::Presence>, String>,
    ) {
        self.presence_loading = false;
        match result {
            Ok(map) => self.presence = map,
            Err(err) => {
                self.presence_next = Some(Instant::now() + PRESENCE_BACKOFF);
                self.warn(format!("Статус игроков: {err}"));
            }
        }
    }

    /// Status text of one account for the list.
    pub(super) fn presence_cell(&self, ui: &mut egui::Ui, user_id: u64) {
        let Some(presence) = self.presence.get(&user_id) else {
            ui.label(RichText::new("—").weak());
            return;
        };
        let text = presence.kind.label();
        let rich = match presence.kind {
            PresenceKind::InGame => RichText::new(text).color(Tone::Ok.color(ui)),
            PresenceKind::Studio => RichText::new(text).color(Tone::Warn.color(ui)),
            PresenceKind::Online => RichText::new(text),
            PresenceKind::Offline => RichText::new(text).weak(),
        };
        let response = ui.add(egui::Label::new(rich).truncate().selectable(false));
        if !presence.location.is_empty() {
            response.on_hover_text(&presence.location);
        }
    }

    // ------------------------------------------------------------ details
    /// Ask for age, balance and subscription of the selected account.
    pub(super) fn request_details(&mut self) {
        let Some(account) = self.selected_account() else {
            self.warn("Сначала выбери аккаунт");
            return;
        };
        if account.cookie.is_empty() {
            self.warn("У аккаунта нет cookie");
            return;
        }
        if account.user_id == 0 {
            self.warn("Сначала проверь cookie: нужен ID аккаунта");
            return;
        }
        let job = Job::Details {
            id: account.id.clone(),
            user_id: account.user_id,
            cookie: account.cookie.clone(),
        };
        self.details
            .insert(account.id.clone(), DetailsState::Loading);
        self.worker.send(job);
    }

    pub(super) fn handle_details(&mut self, id: String, result: Result<Details, String>) {
        let state = match result {
            Ok(details) => DetailsState::Ready(details),
            Err(err) => DetailsState::Failed(err),
        };
        self.details.insert(id, state);
    }

    /// Text and tone of the details line for an account, if it has any.
    pub(super) fn details_line(&self, id: &str) -> Option<(String, Tone)> {
        match self.details.get(id)? {
            DetailsState::Loading => Some(("загружаю данные…".to_string(), Tone::Info)),
            DetailsState::Ready(details) => Some((details_text(details, now_unix()), Tone::Info)),
            DetailsState::Failed(err) => Some((err.clone(), Tone::Warn)),
        }
    }

    pub(super) fn details_requested(&self, id: &str) -> bool {
        self.details.contains_key(id)
    }

    // -------------------------------------------------------- game search
    pub(super) fn open_game_search(&mut self) {
        if self.game_search.is_none() {
            self.game_search = Some(GameSearch {
                query: String::new(),
                loading: false,
                results: Vec::new(),
                error: None,
                focus: true,
            });
        }
    }

    pub(super) fn handle_games(&mut self, result: Result<Vec<GameHit>, String>) {
        let Some(window) = self.game_search.as_mut() else {
            return;
        };
        window.loading = false;
        match result {
            Ok(hits) => {
                window.results = hits;
                window.error = None;
            }
            Err(err) => window.error = Some(err),
        }
    }

    // ------------------------------------------------------ player search
    pub(super) fn open_player_search(&mut self) {
        if self.player_search.is_none() {
            self.player_search = Some(PlayerSearch {
                input: String::new(),
                loading: false,
                error: None,
                focus: true,
            });
        }
    }

    pub(super) fn handle_player(&mut self, result: Result<Target, String>) {
        let Some(window) = self.player_search.as_mut() else {
            return;
        };
        window.loading = false;
        match result {
            Err(err) => window.error = Some(err),
            Ok(target) => {
                self.player_search = None;
                self.config.place_id = target.place_id.to_string();
                self.config.private_link = target.job_id.clone().unwrap_or_default();
                self.save_config();

                let place = if target.location.is_empty() {
                    format!("игра {}", target.place_id)
                } else {
                    format!("«{}»", target.location)
                };
                if target.job_id.is_some() {
                    self.ok(format!(
                        "{} играет в {place}: игра и сервер подставлены в панель запуска",
                        target.name
                    ));
                } else {
                    self.warn(format!(
                        "{} играет в {place}, но сервер Roblox не показал: \
                         зайдёшь в любой сервер этой игры",
                        target.name
                    ));
                }
            }
        }
    }

    pub(super) fn lookup_windows(&mut self, ctx: &egui::Context) {
        self.game_search_window(ctx);
        self.player_search_window(ctx);
    }

    fn game_search_window(&mut self, ctx: &egui::Context) {
        let Some(mut window) = self.game_search.take() else {
            return;
        };

        let mut open = true;
        let mut search = false;
        let mut picked: Option<GameHit> = None;

        egui::Window::new("Поиск игры")
            .collapsible(false)
            .open(&mut open)
            .resizable(true)
            .default_width(520.0)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut window.query)
                            .hint_text("название игры")
                            .desired_width(300.0),
                    );
                    if window.focus {
                        edit.request_focus();
                        window.focus = false;
                    }
                    let enter =
                        edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let clicked = ui
                        .add_enabled(!window.loading, egui::Button::new("Искать"))
                        .clicked();
                    if (clicked || enter) && !window.loading {
                        search = true;
                    }
                    if window.loading {
                        ui.spinner();
                    }
                });
                ui.add_space(4.0);

                if let Some(error) = &window.error {
                    let color = Tone::Error.color(ui);
                    ui.colored_label(color, error);
                }
                if window.results.is_empty() {
                    return;
                }
                ui.label(
                    RichText::new("Клик по игре подставит её Place ID в панель запуска.").weak(),
                );
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for hit in &window.results {
                            let label = format!(
                                "{}   ·   игроков {}   ·   {}",
                                shorten(&hit.name, 44),
                                hit.players,
                                hit.place_id
                            );
                            if ui
                                .selectable_label(false, label)
                                .on_hover_text(&hit.name)
                                .clicked()
                            {
                                picked = Some(hit.clone());
                            }
                        }
                    });
            });

        if let Some(hit) = picked {
            self.config.place_id = hit.place_id.to_string();
            // A server belongs to one game: the old one would not fit.
            self.config.private_link.clear();
            self.save_config();
            self.ok(format!("Игра выбрана: {} ({})", hit.name, hit.place_id));
            return;
        }

        if search {
            let query = window.query.trim().to_string();
            if query.is_empty() {
                window.error = Some("Введи название игры".into());
            } else {
                window.loading = true;
                window.error = None;
                self.worker.send(Job::SearchGames { query });
            }
        }
        if open {
            self.game_search = Some(window);
        }
    }

    fn player_search_window(&mut self, ctx: &egui::Context) {
        let Some(mut window) = self.player_search.take() else {
            return;
        };

        let mut open = true;
        let mut search = false;

        egui::Window::new("Зайти к игроку")
            .collapsible(false)
            .open(&mut open)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.label(
                    "Найдёт, где сейчас играет игрок, и подставит игру и сервер в панель запуска. \
                     Дальше запускай аккаунты как обычно: они зайдут к нему.",
                );
                ui.label(
                    RichText::new(
                        "Roblox показывает это только тем, кому игрок разрешил видеть, где он \
                         играет. Запрос идёт от имени выбранного (или первого рабочего) аккаунта.",
                    )
                    .weak(),
                );
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut window.input)
                            .hint_text("ник, ID или ссылка на профиль")
                            .desired_width(300.0),
                    );
                    if window.focus {
                        edit.request_focus();
                        window.focus = false;
                    }
                    let enter =
                        edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let clicked = ui
                        .add_enabled(!window.loading, egui::Button::new("Найти"))
                        .clicked();
                    if (clicked || enter) && !window.loading {
                        search = true;
                    }
                    if window.loading {
                        ui.spinner();
                    }
                });
                if let Some(error) = &window.error {
                    ui.add_space(4.0);
                    let color = Tone::Error.color(ui);
                    ui.colored_label(color, error);
                }
            });

        if search {
            match self.lookup_cookie() {
                None => window.error = Some("Нет ни одного рабочего аккаунта для запроса".into()),
                Some(cookie) => {
                    window.loading = true;
                    window.error = None;
                    self.worker.send(Job::FindPlayer {
                        cookie,
                        who: window.input.clone(),
                    });
                }
            }
        }
        if open {
            self.player_search = Some(window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1 000");
        assert_eq!(group_thousands(1_234_567), "1 234 567");
        assert_eq!(group_thousands(12_345), "12 345");
    }

    #[test]
    fn details_line_skips_what_is_unknown() {
        let now = 20_776 * 86_400;
        let full = Details {
            created: Some("2023-11-19T10:00:00Z".into()),
            robux: Some(1_500),
            premium: Some(true),
        };
        assert_eq!(
            details_text(&full, now),
            "возраст 3 г. 0 мес. · 1 500 R$ · Premium"
        );

        let partial = Details {
            created: None,
            robux: Some(0),
            premium: Some(false),
        };
        assert_eq!(details_text(&partial, now), "0 R$ · без Premium");
        assert_eq!(details_text(&Details::default(), now), "нет данных");
    }

    #[test]
    fn the_unlock_summary_is_quiet_when_all_is_well() {
        let (tone, text) = startup_summary(4, 0, &[]);
        assert!(tone == Tone::Ok);
        assert_eq!(text, "Cookie проверены: рабочих 4");
    }

    #[test]
    fn the_unlock_summary_names_signed_out_accounts() {
        let dead: Vec<String> = ["a", "b", "c", "d", "e", "f", "g"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        let (tone, text) = startup_summary(2, 1, &dead);
        assert!(tone == Tone::Warn);
        assert!(text.contains("вышли из аккаунта: a, b, c, d, e и ещё 2"));
        assert!(text.contains("не удалось проверить: 1"));
    }
}
