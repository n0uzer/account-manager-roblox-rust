//! egui/eframe user interface.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::roblox;
use crate::store::Store;

const CLIPBOARD_TTL: Duration = Duration::from_secs(60);
/// Gap between queued launches: Roblox dislikes a burst of ticket requests.
const LAUNCH_GAP: Duration = Duration::from_millis(1500);
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(180, 40, 40);
const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(140, 80, 0);

struct Form {
    editing: Option<String>,
    label: String,
    cookie: String,
    note: String,
    error: String,
}

pub struct App {
    path: PathBuf,
    needs_create: bool,
    store: Option<Store>,

    password: String,
    password_repeat: String,
    lock_error: String,

    selected: Option<String>,
    place_id: String,
    status: String,

    /// "Open every window separately" — launches through `RobloxPlayerBeta.exe --app`.
    multi_instance: bool,
    /// Accounts checked for a batch launch.
    launch_set: Vec<String>,
    /// Batch launch queue: one account per tick.
    queue: VecDeque<String>,
    next_launch_at: Option<Instant>,

    form: Option<Form>,
    show_cookie: Option<String>,
    confirm_delete: Option<String>,

    clipboard_value: Option<String>,
    clipboard_clear_at: Option<Instant>,
}

impl App {
    pub fn new() -> Self {
        let path = Store::default_path();
        let needs_create = !Store::exists(&path);
        App {
            path,
            needs_create,
            store: None,
            password: String::new(),
            password_repeat: String::new(),
            lock_error: String::new(),
            selected: None,
            place_id: String::new(),
            status: "Готово".into(),
            multi_instance: true,
            launch_set: Vec::new(),
            queue: VecDeque::new(),
            next_launch_at: None,
            form: None,
            show_cookie: None,
            confirm_delete: None,
            clipboard_value: None,
            clipboard_clear_at: None,
        }
    }

    // ------------------------------------------------------------ lock screen
    fn lock_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.heading(if self.needs_create {
                "Новое хранилище"
            } else {
                "Разблокировать хранилище"
            });
            ui.add_space(8.0);
            ui.label(if self.needs_create {
                "Придумай мастер-пароль. Им шифруется файл с cookie \
                 (Argon2id + AES-256-GCM), сам пароль нигде не сохраняется: \
                 забыл — данные не восстановить."
            } else {
                "Введи мастер-пароль от файла accounts.dat."
            });
            ui.add_space(12.0);

            ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .hint_text("мастер-пароль")
                    .desired_width(320.0),
            );
            if self.needs_create {
                ui.add(
                    egui::TextEdit::singleline(&mut self.password_repeat)
                        .password(true)
                        .hint_text("повтори пароль")
                        .desired_width(320.0),
                );
            }

            if !self.lock_error.is_empty() {
                ui.add_space(4.0);
                ui.colored_label(ERROR_COLOR, &self.lock_error);
            }

            ui.add_space(10.0);
            let label = if self.needs_create { "Создать" } else { "Войти" };
            if ui.button(label).clicked() {
                self.try_unlock();
            }

            ui.add_space(20.0);
            ui.small(format!("Файл: {}", self.path.display()));
        });
    }

    fn try_unlock(&mut self) {
        self.lock_error.clear();

        if self.needs_create {
            if self.password != self.password_repeat {
                self.lock_error = "Пароли не совпадают".into();
                return;
            }
            match Store::create(self.path.clone(), &self.password) {
                Ok(store) => self.store = Some(store),
                Err(err) => self.lock_error = err,
            }
        } else {
            match Store::unlock(self.path.clone(), &self.password) {
                Ok(store) => self.store = Some(store),
                Err(err) => self.lock_error = err,
            }
        }

        if self.store.is_some() {
            self.password.clear();
            self.password_repeat.clear();
            self.status = "Хранилище открыто".into();
        }
    }

    // ----------------------------------------------------------- main screen
    fn main_screen(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Place ID:");
                ui.add(egui::TextEdit::singleline(&mut self.place_id).desired_width(110.0));
                if ui.button("Запустить игру").clicked() {
                    self.launch();
                }
                let queued = self.launch_set.len();
                let batch_label = if queued == 0 {
                    "Запустить выбранные".to_string()
                } else {
                    format!("Запустить выбранные ({queued})")
                };
                if ui
                    .add_enabled(queued > 0, egui::Button::new(batch_label))
                    .on_hover_text("Отметь аккаунты галочками в списке")
                    .clicked()
                {
                    self.launch_batch();
                }
                ui.checkbox(&mut self.multi_instance, "Несколько окон (--app)")
                    .on_hover_text(
                        "Каждый запуск — отдельное окно Roblox. \
                         Требуется установленный клиент Roblox.",
                    );
                ui.separator();
                if ui.button("Добавить").clicked() {
                    self.open_add();
                }
                if ui.button("Изменить").clicked() {
                    self.open_edit();
                }
                if ui.button("Проверить cookie").clicked() {
                    self.validate();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Cookie → буфер").clicked() {
                    self.copy_cookie();
                }
                if ui.button("Показать cookie").clicked() {
                    self.show_cookie = self.selected.clone();
                }
                if ui.button("Удалить").clicked() {
                    self.confirm_delete = self.selected.clone();
                }
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(&self.status);
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let accounts = self
                .store
                .as_ref()
                .map(|store| store.accounts.clone())
                .unwrap_or_default();

            if accounts.is_empty() {
                ui.add_space(20.0);
                ui.label("Пока пусто. Нажми «Добавить» и вставь .ROBLOSECURITY.");
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("accounts")
                        .num_columns(5)
                        .striped(true)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("Окно");
                            ui.strong("Название");
                            ui.strong("Roblox");
                            ui.strong("User ID");
                            ui.strong("Заметка");
                            ui.end_row();

                            for account in &accounts {
                                let mut queued = self
                                    .launch_set
                                    .iter()
                                    .any(|marked| marked == &account.id);
                                if ui
                                    .checkbox(&mut queued, "")
                                    .on_hover_text("Запустить вместе с остальными отмеченными")
                                    .changed()
                                {
                                    self.set_queued(&account.id, queued);
                                }

                                let is_selected =
                                    self.selected.as_deref() == Some(account.id.as_str());
                                if ui
                                    .selectable_label(is_selected, &account.label)
                                    .clicked()
                                {
                                    self.selected = Some(account.id.clone());
                                }
                                ui.label(if account.username.is_empty() {
                                    "—".to_string()
                                } else {
                                    account.username.clone()
                                });
                                ui.label(if account.user_id == 0 {
                                    "—".to_string()
                                } else {
                                    account.user_id.to_string()
                                });
                                ui.label(&account.note);
                                ui.end_row();
                            }
                        });
                });
        });
    }

    // --------------------------------------------------------------- dialogs
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.form_window(ctx);
        self.cookie_window(ctx);
        self.delete_window(ctx);
    }

    fn form_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else {
            return;
        };

        let mut open = true;
        let mut submit = false;
        let mut cancel = false;

        egui::Window::new(if form.editing.is_some() {
            "Изменить аккаунт"
        } else {
            "Добавить аккаунт"
        })
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(520.0);

            ui.label("Название");
            ui.add(egui::TextEdit::singleline(&mut form.label).desired_width(f32::INFINITY));
            ui.add_space(6.0);

            ui.label(if form.editing.is_some() {
                ".ROBLOSECURITY — новый (пусто = не менять)"
            } else {
                ".ROBLOSECURITY"
            });
            ui.add(
                egui::TextEdit::singleline(&mut form.cookie)
                    .password(true)
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);

            ui.label("Заметка");
            ui.add(egui::TextEdit::singleline(&mut form.note).desired_width(f32::INFINITY));
            ui.add_space(8.0);

            ui.colored_label(
                WARN_COLOR,
                "Cookie = полный доступ к аккаунту. Хранится только локально, в шифрованном виде.",
            );

            if !form.error.is_empty() {
                ui.colored_label(ERROR_COLOR, &form.error);
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Сохранить").clicked() {
                    submit = true;
                }
                if ui.button("Отмена").clicked() {
                    cancel = true;
                }
            });
        });

        if cancel || !open {
            return;
        }

        if submit {
            if form.editing.is_none() && form.cookie.trim().is_empty() {
                form.error = "Вставь .ROBLOSECURITY".into();
                self.form = Some(form);
                return;
            }
            self.submit_form(form);
        } else {
            self.form = Some(form);
        }
    }

    fn cookie_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.show_cookie.clone() else {
            return;
        };
        let Some(account) = self.store.as_ref().and_then(|store| store.find(&id)) else {
            self.show_cookie = None;
            return;
        };
        let cookie = account.cookie.clone();
        let label = account.label.clone();

        let mut open = true;
        egui::Window::new(format!("Cookie — {label}"))
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);
                ui.colored_label(
                    WARN_COLOR,
                    "Никому не показывай и не вставляй никуда, кроме клиента Roblox.",
                );
                ui.add_space(6.0);

                let mut readonly = cookie.clone();
                ui.add(
                    egui::TextEdit::multiline(&mut readonly)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .interactive(false),
                );

                ui.add_space(6.0);
                if ui.button("Скопировать").clicked() {
                    self.set_clipboard(&cookie);
                }
            });

        if !open {
            self.show_cookie = None;
        }
    }

    fn delete_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete.clone() else {
            return;
        };
        let Some(account) = self.store.as_ref().and_then(|store| store.find(&id)) else {
            self.confirm_delete = None;
            return;
        };
        let label = account.label.clone();

        let mut open = true;
        let mut remove = false;
        let mut cancel = false;

        egui::Window::new("Удалить аккаунт")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Удалить «{label}» из менеджера?\nСам аккаунт Roblox это не затронет."
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Удалить").clicked() {
                        remove = true;
                    }
                    if ui.button("Отмена").clicked() {
                        cancel = true;
                    }
                });
            });

        if remove {
            self.confirm_delete = None;
            if let Some(store) = self.store.as_mut() {
                store.remove(&id);
                if let Err(err) = store.save() {
                    self.status = format!("Ошибка сохранения: {err}");
                } else {
                    self.status = "Удалено".into();
                }
            }
            if self.selected.as_deref() == Some(id.as_str()) {
                self.selected = None;
            }
            self.launch_set.retain(|marked| marked != &id);
            self.queue.retain(|queued| queued != &id);
        } else if cancel || !open {
            self.confirm_delete = None;
        }
    }

    // --------------------------------------------------------------- actions
    fn open_add(&mut self) {
        self.form = Some(Form {
            editing: None,
            label: String::new(),
            cookie: String::new(),
            note: String::new(),
            error: String::new(),
        });
    }

    fn open_edit(&mut self) {
        let Some(id) = self.selected.clone() else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        let Some(account) = self.store.as_ref().and_then(|store| store.find(&id)) else {
            return;
        };
        self.form = Some(Form {
            editing: Some(id),
            label: account.label.clone(),
            cookie: String::new(),
            note: account.note.clone(),
            error: String::new(),
        });
    }

    fn submit_form(&mut self, form: Form) {
        let cookie = form.cookie.trim().to_string();
        let mut identity = None;
        let mut check_error = None;

        if !cookie.is_empty() {
            self.status = "Проверяю cookie…".into();
            match roblox::fetch_identity(&cookie) {
                Ok(profile) => identity = Some(profile),
                Err(err) => check_error = Some(err),
            }
        }

        let Some(store) = self.store.as_mut() else {
            return;
        };

        let id = match &form.editing {
            Some(existing) => {
                store.update(
                    existing,
                    Some(&form.label),
                    Some(&form.note),
                    if cookie.is_empty() { None } else { Some(&cookie) },
                    identity.as_ref().map(|profile| profile.name.as_str()),
                    identity.as_ref().map(|profile| profile.id),
                );
                existing.clone()
            }
            None => {
                let new_id = store.add(&form.label, &cookie, &form.note);
                if let Some(profile) = &identity {
                    store.update(
                        &new_id,
                        None,
                        None,
                        None,
                        Some(&profile.name),
                        Some(profile.id),
                    );
                }
                new_id
            }
        };

        if let Err(err) = store.save() {
            self.status = format!("Ошибка сохранения: {err}");
            return;
        }

        self.selected = Some(id);
        self.status = match check_error {
            Some(err) => format!("Сохранено без проверки: {err}"),
            None => match identity {
                Some(profile) => format!("OK: {} (id {})", profile.name, profile.id),
                None => "Сохранено".to_string(),
            },
        };
    }

    fn selected_account(&self) -> Option<&crate::store::Account> {
        let id = self.selected.as_deref()?;
        self.store.as_ref().and_then(|store| store.find(id))
    }

    fn validate(&mut self) {
        let Some(account) = self.selected_account() else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        let id = account.id.clone();
        let cookie = account.cookie.clone();

        self.status = "Проверяю cookie…".into();
        match roblox::fetch_identity(&cookie) {
            Ok(profile) => {
                if let Some(store) = self.store.as_mut() {
                    store.update(&id, None, None, None, Some(&profile.name), Some(profile.id));
                    if let Err(err) = store.save() {
                        self.status = format!("Ошибка сохранения: {err}");
                        return;
                    }
                }
                self.status = format!("OK: {} (id {})", profile.name, profile.id);
            }
            Err(err) => self.status = format!("Cookie недействителен: {err}"),
        }
    }

    fn launch(&mut self) {
        let Some(account) = self.selected_account() else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        let cookie = account.cookie.clone();
        let label = account.label.clone();
        let place_id = self.place_id.trim().parse::<u64>();

        let Ok(place_id) = place_id else {
            self.status = "Place ID должен быть числом".into();
            return;
        };

        self.status = match roblox::launch_game(place_id, &cookie, self.multi_instance) {
            Ok(kind) => format!("{label}: place {place_id} ({})", kind.human()),
            Err(err) => format!("{label}: {err}"),
        };
    }

    /// Toggle an account in the batch launch list.
    fn set_queued(&mut self, id: &str, queued: bool) {
        if queued {
            if !self.launch_set.iter().any(|existing| existing == id) {
                self.launch_set.push(id.to_string());
            }
        } else {
            self.launch_set.retain(|existing| existing != id);
        }
    }

    /// Queue every checked account; `tick_queue` launches them one by one.
    fn launch_batch(&mut self) {
        if self.launch_set.is_empty() {
            self.status = "Отметь аккаунты галочками в списке".into();
            return;
        }
        if self.place_id.trim().parse::<u64>().is_err() {
            self.status = "Place ID должен быть числом".into();
            return;
        }

        self.queue = self.launch_set.iter().cloned().collect();
        // A new batch restarts the countdown; pending accounts from the previous
        // batch are replaced on purpose.
        self.next_launch_at = Some(Instant::now());
        self.status = format!("В очереди: {} аккаунт(ов)", self.queue.len());
    }

    /// Launch at most one queued account per tick, spaced by `LAUNCH_GAP`.
    fn tick_queue(&mut self, ctx: &egui::Context) {
        let Some(at) = self.next_launch_at else {
            return;
        };
        if Instant::now() < at {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        self.next_launch_at = None;

        let Some(id) = self.queue.pop_front() else {
            return;
        };
        let Ok(place_id) = self.place_id.trim().parse::<u64>() else {
            self.queue.clear();
            self.status = "Place ID должен быть числом".into();
            return;
        };

        let account = self
            .store
            .as_ref()
            .and_then(|store| store.find(&id))
            .cloned();

        let line = match account {
            None => "аккаунт удалён, пропуск".to_string(),
            Some(account) if account.cookie.is_empty() => {
                format!("{}: нет cookie, пропуск", account.label)
            }
            Some(account) => {
                self.status = format!("Запускаю {}…", account.label);
                match roblox::launch_game(place_id, &account.cookie, self.multi_instance) {
                    Ok(_) => format!("{}: запущен", account.label),
                    Err(err) => format!("{}: {err}", account.label),
                }
            }
        };

        let left = self.queue.len();
        self.status = if left == 0 {
            format!("{line}. Готово")
        } else {
            format!("{line}. Осталось: {left}")
        };

        if left > 0 {
            self.next_launch_at = Some(Instant::now() + LAUNCH_GAP);
            ctx.request_repaint_after(LAUNCH_GAP);
        }
    }

    fn copy_cookie(&mut self) {
        let Some(cookie) = self.selected_account().map(|account| account.cookie.clone()) else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        if cookie.is_empty() {
            self.status = "У аккаунта нет cookie".into();
            return;
        }
        self.set_clipboard(&cookie);
    }

    // ------------------------------------------------------------- clipboard
    fn set_clipboard(&mut self, text: &str) {
        match arboard::Clipboard::new() {
            Ok(mut clipboard) => match clipboard.set_text(text.to_string()) {
                Ok(()) => {
                    self.clipboard_value = Some(text.to_string());
                    self.clipboard_clear_at = Some(Instant::now() + CLIPBOARD_TTL);
                    self.status = "Cookie скопирован. Буфер очистится через 60 секунд.".into();
                }
                Err(err) => self.status = format!("Буфер обмена: {err}"),
            },
            Err(err) => self.status = format!("Буфер обмена: {err}"),
        }
    }

    fn tick_clipboard(&mut self) {
        let Some(clear_at) = self.clipboard_clear_at else {
            return;
        };
        if Instant::now() < clear_at {
            return;
        }
        self.clipboard_clear_at = None;

        let Some(expected) = self.clipboard_value.take() else {
            return;
        };
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let matches = clipboard
                .get_text()
                .map(|current| current == expected)
                .unwrap_or(false);
            if matches {
                let _ = clipboard.set_text(String::new());
                self.status = "Буфер обмена очищен.".into();
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick_clipboard();
        if self.clipboard_clear_at.is_some() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.tick_queue(ctx);

        if self.store.is_none() {
            self.lock_screen(ctx);
        } else {
            self.main_screen(ctx);
        }

        self.dialogs(ctx);
    }
}
