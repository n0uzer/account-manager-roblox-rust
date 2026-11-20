//! Game windows: which account owns which window, automatic relaunch, the window
//! limit, closing windows, anti-AFK, and the extra settings that go with them.

use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};

use super::{launch_gap, number, persist_now, App, Tone};
use crate::antiafk;
use crate::autostart;
use crate::backup;
use crate::config::Theme;
use crate::jobs::Job;
use crate::processes;
use crate::roblox::Destination;
use crate::store::Account;
use crate::watch::{self, Event};

/// How often the window list is compared with what was launched.
const WATCH_POLL: Duration = Duration::from_secs(3);
/// How long the "close everything" button waits for its confirming second click.
const KILL_CONFIRM: Duration = Duration::from_secs(5);
/// How long a launch waits for a free window slot before looking again.
pub(super) const SLOT_RETRY: Duration = Duration::from_secs(2);
/// The process list is not asked for more often than this.
const COUNT_CACHE: Duration = Duration::from_secs(1);
/// Upper bound of the window-limit field.
const MAX_WINDOWS_FIELD: u32 = 64;

impl App {
    // ------------------------------------------------------- destinations
    /// Where this account launches: its own game and server, or the toolbar's.
    pub(super) fn destination_of(&self, account: &Account) -> Option<Destination> {
        let destination = crate::roblox::destination_for_account(
            &account.place_id,
            &account.server,
            &self.config.place_id,
            &self.config.private_link,
        );
        destination.is_valid().then_some(destination)
    }

    // -------------------------------------------------------- window slots
    /// Client processes running now, asked for at most once a second.
    fn running_windows(&mut self) -> usize {
        let stale = self
            .window_count_at
            .map_or(true, |at| at.elapsed() >= COUNT_CACHE);
        if stale {
            self.window_count = processes::client_pids().len();
            self.window_count_at = Some(Instant::now());
        }
        self.window_count
    }

    /// Whether one more window may be started for `id` under the window limit.
    pub(super) fn slot_free(&mut self, id: &str) -> bool {
        let max = self.config.max_windows;
        if max == 0 {
            return true;
        }
        let mut running = self.running_windows();
        // Launching an account again closes its old window first.
        if self.config.close_previous && self.tracker.window_of(id).is_some() {
            running = running.saturating_sub(1);
        }
        let waiting = self.launching + self.tracker.pending_count();
        watch::has_free_slot(running, waiting, max)
    }

    /// Hand a launch to the worker.
    pub(super) fn dispatch_launch(
        &mut self,
        account: &Account,
        place: Destination,
        in_batch: bool,
    ) {
        if self.config.close_previous {
            self.close_window_of(&account.id);
        }
        self.pending += 1;
        self.launching += 1;
        // The next limit check must see the new window, not a cached count.
        self.window_count_at = None;
        self.worker.send(Job::Launch {
            id: account.id.clone(),
            label: account.label.clone(),
            place,
            cookie: account.cookie.clone(),
            multi: self.config.multi_instance,
            limits: self.limits(),
            in_batch,
        });
    }

    /// Close the window this manager opened for the account, if it knows one.
    /// Returns whether a window was closed.
    pub(super) fn close_window_of(&mut self, id: &str) -> bool {
        let Some(pid) = self.tracker.window_of(id) else {
            return false;
        };
        // Forget first: a deliberate close is not a lost window.
        self.tracker.forget(id);
        self.relaunch.retain(|queued| queued != id);
        processes::kill(&[pid]) > 0
    }

    /// Close every Roblox client, ours or not.
    fn close_all_windows(&mut self) {
        let closed = processes::kill_all();
        self.tracker.clear();
        self.relaunch.clear();
        self.next_relaunch_at = None;
        self.limiter.reset();
        self.window_count_at = None;
        if !self.queue.is_empty() {
            self.cancel_queue();
        }
        self.info(format!("Закрыто окон Roblox: {closed}"));
    }

    fn label_of(&self, id: &str) -> String {
        self.store
            .as_ref()
            .and_then(|store| store.find(id))
            .map(|account| account.label.clone())
            .unwrap_or_else(|| "аккаунт".to_string())
    }

    // ----------------------------------------------------------- watching
    /// Follow the windows of launched accounts and relaunch the lost ones.
    pub(super) fn tick_watch(&mut self, ctx: &egui::Context) {
        if self.store.is_none() {
            return;
        }

        let now = Instant::now();
        if now >= self.next_watch_poll {
            self.next_watch_poll = now + WATCH_POLL;
            if !self.tracker.is_idle() {
                let procs = processes::client_procs();
                self.window_count = procs.len();
                self.window_count_at = Some(now);
                for event in self.tracker.poll(&procs, processes::now_filetime()) {
                    self.on_watch_event(event);
                }
            }
        }

        self.tick_relaunch(ctx);

        // egui does not wake up by itself, and a lost window is only noticed
        // when somebody looks.
        if !self.tracker.is_idle() || !self.relaunch.is_empty() {
            ctx.request_repaint_after(WATCH_POLL);
        }
    }

    fn on_watch_event(&mut self, event: Event) {
        match event {
            Event::Opened { .. } => {}
            Event::Lost { account } => {
                let label = self.label_of(&account);
                if !self.config.auto_relaunch {
                    self.info(format!("{label}: окно закрыто"));
                    return;
                }
                if !self.limiter.allow(&account, processes::now_filetime()) {
                    self.fail(format!(
                        "{label}: окно закрывается снова и снова, автоперезапуск для него остановлен"
                    ));
                    return;
                }
                self.warn(format!("{label}: окно закрылось, перезапускаю"));
                if !self.relaunch.contains(&account) {
                    self.relaunch.push_back(account);
                }
            }
            Event::NeverOpened { account } => {
                let label = self.label_of(&account);
                self.warn(format!(
                    "{label}: окно клиента так и не появилось (cookie, игра или сеть?)"
                ));
            }
        }
    }

    /// Start one waiting relaunch per launch gap, under the window limit.
    fn tick_relaunch(&mut self, ctx: &egui::Context) {
        if self.relaunch.is_empty() {
            self.next_relaunch_at = None;
            return;
        }
        let now = Instant::now();
        if let Some(at) = self.next_relaunch_at.filter(|at| now < *at) {
            ctx.request_repaint_after(at - now);
            return;
        }

        let Some(id) = self.relaunch.front().cloned() else {
            return;
        };
        if !self.slot_free(&id) {
            self.next_relaunch_at = Some(now + SLOT_RETRY);
            return;
        }
        self.relaunch.pop_front();

        let account = self
            .store
            .as_ref()
            .and_then(|store| store.find(&id))
            .cloned();
        match account {
            Some(account) if !account.cookie.is_empty() => match self.destination_of(&account) {
                Some(place) => {
                    self.info(format!("Перезапускаю {}…", account.label));
                    self.dispatch_launch(&account, place, false);
                }
                None => self.warn(format!(
                    "{}: не знаю, куда перезапускать — укажи игру",
                    account.label
                )),
            },
            _ => self.warn("Перезапуск пропущен: аккаунт удалён или без cookie"),
        }
        self.next_relaunch_at = Some(Instant::now() + launch_gap(self.config.launch_gap_secs));
    }

    // -------------------------------------------------------------- widgets
    /// "Close all windows" button; the second click within a few seconds does it.
    pub(super) fn close_all_button(&mut self, ui: &mut egui::Ui) {
        let running = self.running_windows();
        if running == 0 && self.tracker.is_idle() {
            self.kill_armed = None;
            return;
        }

        let armed = self
            .kill_armed
            .is_some_and(|at| at.elapsed() < KILL_CONFIRM);
        if !armed {
            self.kill_armed = None;
        }

        let text = if armed {
            format!("Точно закрыть все ({running})?")
        } else {
            format!("Закрыть все окна ({running})")
        };
        let color = Tone::Error.color(ui);
        let response = ui.button(RichText::new(text).color(color)).on_hover_text(
            "Закрывает все окна клиента Roblox, в том числе запущенные не из менеджера.\n\
                 Нажми ещё раз в течение 5 секунд, чтобы подтвердить.",
        );
        if armed {
            // The label changes back by itself once the time is up.
            ui.ctx().request_repaint_after(KILL_CONFIRM);
        }
        if response.clicked() {
            if armed {
                self.kill_armed = None;
                self.close_all_windows();
            } else {
                self.kill_armed = Some(Instant::now());
            }
        }
    }

    /// The "Окна" menu: how windows behave once they are open.
    pub(super) fn windows_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .checkbox(
                &mut self.config.auto_relaunch,
                "Перезапускать закрывшиеся окна",
            )
            .on_hover_text(
                "Если окно аккаунта пропало, а закрывал его не ты через менеджер, \
                 аккаунт запускается снова: не больше 3 раз за 10 минут.\n\
                 Работает, пока хранилище открыто: после блокировки cookie недоступны.\n\
                 Закрыл окно сам и не хочешь, чтобы оно вернулось — выключи или закрой \
                 его из менеджера («Закрыть окно» в меню аккаунта).",
            )
            .changed()
        {
            self.limiter.reset();
            self.save_config();
        }
        if ui
            .checkbox(
                &mut self.config.close_previous,
                "Закрывать прежнее окно аккаунта при повторном запуске",
            )
            .on_hover_text(
                "Один аккаунт не может сидеть в игре в двух окнах: второй вход выкидывает \
                 первый. Менеджер закрывает старое окно сам — только то, которое открыл он.",
            )
            .changed()
        {
            self.save_config();
        }

        ui.horizontal(|ui| {
            ui.label("Лимит окон:");
            let response = number(ui, &mut self.config.max_windows, 0..=MAX_WINDOWS_FIELD, "");
            if persist_now(&response) {
                self.save_config();
            }
            response.on_hover_text(
                "Сколько окон клиента может быть открыто сразу. Пакетный запуск ждёт, \
                 пока освободится место, а не открывает лишние. 0 — без лимита.",
            );
        });

        ui.separator();
        self.anti_afk_controls(ui);
    }

    fn anti_afk_controls(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        changed |= ui
            .checkbox(&mut self.config.anti_afk, "Анти-AFK")
            .on_hover_text(
                "Раз в несколько минут отправляет в окна клиента лёгкий ввод — сдвиг мыши \
                 на пиксель и нажатие F15 — чтобы за простой не выкидывало (Roblox кикает \
                 примерно через 20 минут).\n\
                 Сообщения кладутся прямо в окно: фокус не отбирается, настоящий курсор \
                 не двигается.\n\
                 Это автоматизация ввода: правила Roblox и отдельных игр могут её не \
                 одобрять. Включено — на свой риск. Работает не с клиентом, запущенным \
                 от администратора.",
            )
            .changed();

        ui.add_enabled_ui(self.config.anti_afk, |ui| {
            ui.horizontal(|ui| {
                ui.label("Каждые:");
                let response = number(
                    ui,
                    &mut self.config.anti_afk_minutes,
                    antiafk::MIN_MINUTES..=antiafk::MAX_MINUTES,
                    " мин",
                );
                if persist_now(&response) {
                    changed = true;
                }
                response.on_hover_text("От 1 до 19 минут: клиент кикает на двадцатой.");

                if ui
                    .small_button("сейчас")
                    .on_hover_text("Отправить один раз прямо сейчас, чтобы проверить")
                    .clicked()
                {
                    let reached = antiafk::pulse();
                    self.info(format!("Анти-AFK: ввод отправлен в окон: {reached}"));
                }
            });
        });

        if changed {
            self.config.anti_afk_minutes = antiafk::clamp_minutes(self.config.anti_afk_minutes);
            antiafk::configure(self.config.anti_afk, self.config.anti_afk_minutes);
            self.save_config();
        }
    }

    /// Extra entries of the "Настройки" menu: theme, startup, checks, backups.
    pub(super) fn settings_extras(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Тема", |ui| {
            for theme in Theme::variants() {
                if ui
                    .radio(self.config.theme == theme, theme.label())
                    .clicked()
                {
                    self.config.theme = theme;
                    self.apply_theme(ui.ctx());
                    self.save_config();
                    ui.close_menu();
                }
            }
        });

        let mut start = self.autostart_enabled;
        if ui
            .checkbox(&mut start, "Запускать вместе с Windows")
            .on_hover_text("Запись в HKCU\\...\\Run: права администратора не нужны.")
            .changed()
        {
            match autostart::set_enabled(start) {
                Ok(()) => self.autostart_enabled = start,
                Err(message) => {
                    self.autostart_enabled = autostart::is_enabled();
                    self.fail(message);
                }
            }
        }

        if ui
            .checkbox(
                &mut self.config.check_on_unlock,
                "Проверять cookie при открытии",
            )
            .on_hover_text(
                "После ввода пароля все cookie проверяются сами, и в журнале сразу видно, \
                 какие аккаунты вышли. По одному запросу на аккаунт.",
            )
            .changed()
        {
            self.save_config();
        }

        if ui
            .checkbox(
                &mut self.config.live_status,
                "Показывать, кто в игре (раз в минуту)",
            )
            .on_hover_text(
                "В списке появится колонка «Статус»: онлайн, в игре, офлайн. Один запрос \
                 в минуту ко всем аккаунтам сразу. Выключено по умолчанию.",
            )
            .changed()
        {
            self.presence.clear();
            self.presence_next = self.config.live_status.then(Instant::now);
            self.save_config();
        }

        ui.separator();
        if ui
            .button("Папка резервных копий")
            .on_hover_text(
                "Менеджер хранит несколько последних копий accounts.dat. Они зашифрованы \
                 тем же паролем. Чтобы восстановить, замени accounts.dat нужной копией.",
            )
            .clicked()
        {
            let dir = backup::dir_for(&self.path);
            let _ = std::fs::create_dir_all(&dir);
            if let Err(err) = open::that(&dir) {
                self.fail(format!("Не открыть папку: {err}"));
            }
            ui.close_menu();
        }
    }
}
