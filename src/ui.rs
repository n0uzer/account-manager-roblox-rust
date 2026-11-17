//! egui/eframe user interface.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::config::{Config, SortKey};
use crate::jobs::{self, Done, Job};
use crate::roblox;
use crate::store::{self, Account, Store};

const CLIPBOARD_TTL: Duration = Duration::from_secs(60);
/// Gap between queued launches: Roblox dislikes a burst of ticket requests.
const LAUNCH_GAP: Duration = Duration::from_millis(1500);
const AVATAR_SIZE: f32 = 24.0;
/// How many avatar downloads may be in flight at once.
const MAX_AVATAR_JOBS: usize = 3;
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(180, 40, 40);
const OK_COLOR: egui::Color32 = egui::Color32::from_rgb(30, 110, 40);
const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(140, 80, 0);
const AUTO_LOCK_CHOICES: [u32; 5] = [0, 1, 5, 15, 30];
const FPS_CHOICES: [u32; 5] = [0, 30, 60, 120, 144];
const QUALITY_CHOICES: [u32; 11] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
const SIZE_CHOICES: [(u32, u32); 6] = [
    (0, 0),
    (1280, 720),
    (1024, 576),
    (960, 540),
    (800, 450),
    (640, 360),
];

struct Form {
    editing: Option<String>,
    label: String,
    cookie: String,
    note: String,
    error: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferKind {
    Export,
    Import,
}

struct Transfer {
    kind: TransferKind,
    path: String,
    password: String,
    message: String,
    failed: bool,
}

pub struct App {
    path: PathBuf,
    needs_create: bool,
    store: Option<Store>,
    config: Config,

    password: String,
    password_repeat: String,
    lock_error: String,

    selected: Option<String>,
    search: String,
    status: String,

    /// Accounts checked for a batch launch.
    launch_set: Vec<String>,
    /// Batch launch queue: ids dispatched one per `LAUNCH_GAP`.
    queue: VecDeque<String>,
    next_launch_at: Option<Instant>,

    form: Option<Form>,
    show_cookie: Option<String>,
    confirm_delete: Option<String>,
    transfer: Option<Transfer>,

    clipboard_value: Option<String>,
    clipboard_clear_at: Option<Instant>,

    worker: jobs::Worker,
    textures: HashMap<u64, egui::TextureHandle>,
    avatars_requested: HashSet<u64>,
    avatar_busy: usize,
    pending: usize,

    last_activity: Instant,
}

impl App {
    pub fn new() -> Self {
        let path = Store::default_path();
        let needs_create = !Store::exists(&path);
        App {
            path,
            needs_create,
            store: None,
            config: Config::load(),
            password: String::new(),
            password_repeat: String::new(),
            lock_error: String::new(),
            selected: None,
            search: String::new(),
            status: "Готово".into(),
            launch_set: Vec::new(),
            queue: VecDeque::new(),
            next_launch_at: None,
            form: None,
            show_cookie: None,
            confirm_delete: None,
            transfer: None,
            clipboard_value: None,
            clipboard_clear_at: None,
            worker: jobs::Worker::spawn(),
            textures: HashMap::new(),
            avatars_requested: HashSet::new(),
            avatar_busy: 0,
            pending: 0,
            last_activity: Instant::now(),
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
            let label = if self.needs_create {
                "Создать"
            } else {
                "Войти"
            };
            if ui.button(label).clicked() {
                self.try_unlock();
            }

            ui.add_space(16.0);
            ui.colored_label(
                WARN_COLOR,
                "Хранилище само заблокируется по бездействию (см. настройки после входа).",
            );
            ui.add_space(12.0);
            ui.small(format!("Файл: {}", self.path.display()));
            ui.small(&self.status);
        });
    }

    fn try_unlock(&mut self) {
        self.lock_error.clear();

        if self.needs_create {
            if self.password != self.password_repeat {
                self.lock_error = "Пароли не совпадают".into();
                return;
            }
            match Store::create(self.path.clone(), &self.password, false) {
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
            self.needs_create = false;
            self.password.clear();
            self.password_repeat.clear();
            self.last_activity = Instant::now();
            self.status = "Хранилище открыто".into();
        }
    }

    /// Drop the store (key and cookies are zeroized on drop).
    fn lock_with(&mut self, reason: &str) {
        self.store = None;
        self.selected = None;
        self.launch_set.clear();
        self.queue.clear();
        self.next_launch_at = None;
        self.form = None;
        self.show_cookie = None;
        self.confirm_delete = None;
        self.transfer = None;
        self.password.clear();
        self.password_repeat.clear();
        self.lock_error.clear();
        self.needs_create = false;
        self.status = reason.to_string();
    }

    fn tick_auto_lock(&mut self) {
        if self.store.is_none() {
            return;
        }
        let minutes = self.config.auto_lock_minutes;
        if minutes == 0 {
            return;
        }
        if self.last_activity.elapsed() >= Duration::from_secs(minutes as u64 * 60) {
            self.lock_with("Заблокировано из-за бездействия");
        }
    }

    // ----------------------------------------------------------- main screen
    fn main_screen(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Place ID:");
                let place = ui.add(
                    egui::TextEdit::singleline(&mut self.config.place_id).desired_width(100.0),
                );
                if place.lost_focus() {
                    self.save_config();
                }

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

                if ui
                    .checkbox(&mut self.config.multi_instance, "Несколько окон (--app)")
                    .changed()
                {
                    self.save_config();
                }

                ui.separator();
                if ui.button("Заблокировать").clicked() {
                    self.lock_with("Заблокировано вручную");
                }
            });

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("поиск по названию, нику, заметке…")
                        .desired_width(220.0),
                );

                ui.label("Сортировка:");
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(self.config.sort.label())
                    .show_ui(ui, |ui| {
                        for variant in SortKey::variants() {
                            let picked = self.config.sort == variant;
                            if ui.selectable_label(picked, variant.label()).clicked() && !picked {
                                self.config.sort = variant;
                                self.save_config();
                            }
                        }
                    });

                ui.label("Автоблокировка:");
                egui::ComboBox::from_id_salt("auto-lock")
                    .selected_text(match self.config.auto_lock_minutes {
                        0 => "выкл".to_string(),
                        minutes => format!("{minutes} мин"),
                    })
                    .show_ui(ui, |ui| {
                        for option in AUTO_LOCK_CHOICES {
                            let text = if option == 0 {
                                "выкл".to_string()
                            } else {
                                format!("{option} мин")
                            };
                            if ui
                                .selectable_label(self.config.auto_lock_minutes == option, text)
                                .clicked()
                            {
                                self.config.auto_lock_minutes = option;
                                self.last_activity = Instant::now();
                                self.save_config();
                            }
                        }
                    });

                let mut dpapi = self
                    .store
                    .as_ref()
                    .map(|store| store.dpapi)
                    .unwrap_or(false);
                if ui
                    .checkbox(&mut dpapi, "Привязка к Windows (DPAPI)")
                    .on_hover_text(
                        "Файл можно будет открыть только под этим пользователем Windows. \
                         Экспорт такого файла на другой ПК не сработает.",
                    )
                    .changed()
                {
                    self.toggle_dpapi(dpapi);
                }

                ui.separator();
                if ui.button("Экспорт…").clicked() {
                    self.open_transfer(TransferKind::Export);
                }
                if ui.button("Импорт…").clicked() {
                    self.open_transfer(TransferKind::Import);
                }
            });

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("FPS:");
                egui::ComboBox::from_id_salt("fps-cap")
                    .selected_text(match self.config.fps_cap {
                        0 => "без лимита".to_string(),
                        fps => format!("{fps}"),
                    })
                    .show_ui(ui, |ui| {
                        for option in FPS_CHOICES {
                            let text = if option == 0 {
                                "без лимита".to_string()
                            } else {
                                format!("{option} FPS")
                            };
                            if ui
                                .selectable_label(self.config.fps_cap == option, text)
                                .clicked()
                            {
                                self.config.fps_cap = option;
                                self.save_config();
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Потолок кадров клиента (DFIntTaskSchedulerTargetFps). \
                         Применяется при следующем запуске окна.",
                    );

                ui.label("Графика:");
                egui::ComboBox::from_id_salt("graphics-quality")
                    .selected_text(match self.config.graphics_quality {
                        0 => "по умолчанию".to_string(),
                        level => format!("уровень {level}"),
                    })
                    .show_ui(ui, |ui| {
                        for option in QUALITY_CHOICES {
                            let text = if option == 0 {
                                "по умолчанию".to_string()
                            } else {
                                format!("уровень {option}")
                            };
                            if ui
                                .selectable_label(self.config.graphics_quality == option, text)
                                .clicked()
                            {
                                self.config.graphics_quality = option;
                                self.save_config();
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "1 — самый низкий, 10 — максимальный (DFIntDebugFRMQualityLevelOverride). \
                         Меньше качество — меньше нагрузки на видеокарту.",
                    );

                ui.label("Разрешение окна:");
                egui::ComboBox::from_id_salt("window-size")
                    .selected_text(if self.config.window_width == 0 {
                        "как обычно".to_string()
                    } else {
                        format!("{}×{}", self.config.window_width, self.config.window_height)
                    })
                    .show_ui(ui, |ui| {
                        for (width, height) in SIZE_CHOICES {
                            let text = if width == 0 {
                                "как обычно".to_string()
                            } else {
                                format!("{width}×{height}")
                            };
                            let picked = self.config.window_width == width
                                && self.config.window_height == height;
                            if ui.selectable_label(picked, text).clicked() {
                                self.config.window_width = width;
                                self.config.window_height = height;
                                self.save_config();
                            }
                        }
                    })
                    .response
                    .on_hover_text(
                        "Меньшее окно = меньше пикселей на кадр. Работает только при \
                         включённом режиме «Несколько окон (--app)»: deeplink размер окна \
                         не задаёт.",
                    );

                ui.separator();
                ui.small("FPS и графика пишутся в ClientAppSettings.json клиента.");
            });

            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("Добавить").clicked() {
                    self.open_add();
                }
                if ui.button("Изменить").clicked() {
                    self.open_edit();
                }
                if ui.button("Проверить cookie").clicked() {
                    self.validate();
                }
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
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if self.pending > 0 {
                    ui.spinner();
                    ui.small(format!("в работе: {}", self.pending));
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let accounts = self.visible_accounts();
            self.request_avatars(&accounts);

            if accounts.is_empty() {
                ui.add_space(20.0);
                ui.label(
                    if self
                        .store
                        .as_ref()
                        .map(|s| s.accounts.is_empty())
                        .unwrap_or(true)
                    {
                        "Пока пусто. Нажми «Добавить» и вставь .ROBLOSECURITY."
                    } else {
                        "Ничего не нашлось по этому запросу."
                    },
                );
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("accounts")
                        .num_columns(6)
                        .striped(true)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("Окно");
                            ui.strong("");
                            ui.strong("Название");
                            ui.strong("Roblox");
                            ui.strong("User ID");
                            ui.strong("Заметка");
                            ui.end_row();

                            for account in &accounts {
                                let mut queued =
                                    self.launch_set.iter().any(|marked| marked == &account.id);
                                if ui
                                    .checkbox(&mut queued, "")
                                    .on_hover_text("Запустить вместе с остальными отмеченными")
                                    .changed()
                                {
                                    self.set_queued(&account.id, queued);
                                }

                                self.avatar_cell(ui, account);

                                let is_selected =
                                    self.selected.as_deref() == Some(account.id.as_str());
                                if ui.selectable_label(is_selected, &account.label).clicked() {
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

    fn avatar_cell(&self, ui: &mut egui::Ui, account: &Account) {
        if let Some(texture) = self.textures.get(&account.user_id) {
            ui.add(egui::Image::new((
                texture.id(),
                egui::vec2(AVATAR_SIZE, AVATAR_SIZE),
            )));
        } else if account.user_id != 0 && self.avatars_requested.contains(&account.user_id) {
            ui.add(egui::Spinner::new().size(14.0));
        } else {
            ui.allocate_space(egui::vec2(AVATAR_SIZE, AVATAR_SIZE));
        }
    }

    /// Filter by the search query, then apply the configured order.
    fn visible_accounts(&self) -> Vec<Account> {
        let mut accounts: Vec<Account> = self
            .store
            .as_ref()
            .map(|store| store.accounts.clone())
            .unwrap_or_default();

        let query = self.search.trim().to_lowercase();
        if !query.is_empty() {
            accounts.retain(|account| {
                account.label.to_lowercase().contains(&query)
                    || account.username.to_lowercase().contains(&query)
                    || account.note.to_lowercase().contains(&query)
                    || account.user_id.to_string().contains(&query)
            });
        }

        match self.config.sort {
            SortKey::Added => accounts.sort_by_key(|account| std::cmp::Reverse(account.created_at)),
            SortKey::Name => {
                accounts.sort_by_key(|account| account.label.to_lowercase());
            }
            SortKey::Username => {
                accounts.sort_by_key(|account| account.username.to_lowercase());
            }
        }

        accounts
    }

    fn request_avatars(&mut self, accounts: &[Account]) {
        for account in accounts {
            if self.avatar_busy >= MAX_AVATAR_JOBS {
                break;
            }
            let user_id = account.user_id;
            if user_id == 0
                || self.textures.contains_key(&user_id)
                || self.avatars_requested.contains(&user_id)
            {
                continue;
            }
            self.avatars_requested.insert(user_id);
            self.avatar_busy += 1;
            self.worker.send(Job::Avatar { user_id });
        }
    }

    // --------------------------------------------------------------- dialogs
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.form_window(ctx);
        self.cookie_window(ctx);
        self.delete_window(ctx);
        self.transfer_window(ctx);
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
            }
            if let Some(err) = self.persist() {
                self.status = format!("Ошибка сохранения: {err}");
            } else {
                self.status = "Удалено".into();
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

    fn transfer_window(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.transfer.take() else {
            return;
        };

        let mut open = true;
        let mut run = false;
        let mut close = false;
        let title = match dialog.kind {
            TransferKind::Export => "Экспорт хранилища",
            TransferKind::Import => "Импорт хранилища",
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(540.0);

                ui.label("Путь к файлу");
                ui.add(egui::TextEdit::singleline(&mut dialog.path).desired_width(f32::INFINITY));
                ui.add_space(6.0);

                match dialog.kind {
                    TransferKind::Export => {
                        ui.colored_label(
                            WARN_COLOR,
                            "Файл копируется как есть: тот же пароль, та же привязка к Windows.",
                        );
                    }
                    TransferKind::Import => {
                        ui.label("Пароль импортируемого файла");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.password)
                                .password(true)
                                .desired_width(f32::INFINITY),
                        );
                        ui.colored_label(
                            WARN_COLOR,
                            "Аккаунты добавятся к текущим; дубликаты по cookie пропускаются.",
                        );
                    }
                }

                if !dialog.message.is_empty() {
                    ui.add_space(6.0);
                    let color = if dialog.failed { ERROR_COLOR } else { OK_COLOR };
                    ui.colored_label(color, &dialog.message);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(match dialog.kind {
                            TransferKind::Export => "Экспортировать",
                            TransferKind::Import => "Импортировать",
                        })
                        .clicked()
                    {
                        run = true;
                    }
                    if ui.button("Закрыть").clicked() {
                        close = true;
                    }
                });
            });

        if run {
            self.run_transfer(dialog);
        } else if open && !close {
            self.transfer = Some(dialog);
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
        let Some(store) = self.store.as_mut() else {
            return;
        };

        let id = match &form.editing {
            Some(existing) => {
                store.update(
                    existing,
                    Some(&form.label),
                    Some(&form.note),
                    if cookie.is_empty() {
                        None
                    } else {
                        Some(&cookie)
                    },
                    None,
                    None,
                );
                existing.clone()
            }
            None => store.add(&form.label, &cookie, &form.note),
        };

        if let Some(err) = self.persist() {
            self.status = format!("Ошибка сохранения: {err}");
            return;
        }

        self.selected = Some(id.clone());
        if cookie.is_empty() {
            self.status = "Сохранено".into();
        } else {
            self.pending += 1;
            self.worker.send(Job::Validate { id, cookie });
            self.status = "Сохранено, проверяю cookie…".into();
        }
    }

    fn selected_account(&self) -> Option<&Account> {
        let id = self.selected.as_deref()?;
        self.store.as_ref().and_then(|store| store.find(id))
    }

    fn validate(&mut self) {
        let Some(account) = self.selected_account().cloned() else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        if account.cookie.is_empty() {
            self.status = "У аккаунта нет cookie".into();
            return;
        }

        self.pending += 1;
        self.worker.send(Job::Validate {
            id: account.id,
            cookie: account.cookie,
        });
        self.status = format!("Проверяю {}…", account.label);
    }

    fn launch(&mut self) {
        let Some(account) = self.selected_account().cloned() else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        let Ok(place_id) = self.config.place_id.trim().parse::<u64>() else {
            self.status = "Place ID должен быть числом".into();
            return;
        };
        if account.cookie.is_empty() {
            self.status = "У аккаунта нет cookie".into();
            return;
        }

        self.save_config();
        self.pending += 1;
        self.worker.send(Job::Launch {
            label: account.label.clone(),
            place_id,
            cookie: account.cookie,
            multi: self.config.multi_instance,
            limits: self.limits(),
        });
        self.status = format!("Запускаю {}…", account.label);
    }

    /// Snapshot of the FPS/quality/window-size limits from the toolbar.
    fn limits(&self) -> roblox::Limits {
        roblox::Limits {
            fps_cap: self.config.fps_cap,
            quality: self.config.graphics_quality,
            width: self.config.window_width,
            height: self.config.window_height,
        }
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

    /// Queue every checked account; `tick_queue` dispatches them one by one.
    fn launch_batch(&mut self) {
        if self.launch_set.is_empty() {
            self.status = "Отметь аккаунты галочками в списке".into();
            return;
        }
        if self.config.place_id.trim().parse::<u64>().is_err() {
            self.status = "Place ID должен быть числом".into();
            return;
        }

        self.queue = self.launch_set.iter().cloned().collect();
        // A new batch replaces whatever was still pending.
        self.next_launch_at = Some(Instant::now());
        self.status = format!("В очереди: {} аккаунт(ов)", self.queue.len());
    }

    /// Dispatch at most one queued account per `LAUNCH_GAP`.
    fn tick_queue(&mut self, ctx: &egui::Context) {
        let Some(at) = self.next_launch_at else {
            return;
        };
        if Instant::now() < at {
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        self.next_launch_at = None;

        let Some(id) = self.queue.pop_front() else {
            return;
        };
        let Ok(place_id) = self.config.place_id.trim().parse::<u64>() else {
            self.queue.clear();
            self.status = "Place ID должен быть числом".into();
            return;
        };

        let account = self
            .store
            .as_ref()
            .and_then(|store| store.find(&id))
            .cloned();

        match account {
            Some(account) if !account.cookie.is_empty() => {
                self.pending += 1;
                self.worker.send(Job::Launch {
                    label: account.label.clone(),
                    place_id,
                    cookie: account.cookie,
                    multi: self.config.multi_instance,
                    limits: self.limits(),
                });
                self.status = format!(
                    "Запускаю {}… (в очереди: {})",
                    account.label,
                    self.queue.len()
                );
            }
            Some(account) => {
                self.status = format!("{}: нет cookie, пропуск", account.label);
            }
            None => self.status = "Аккаунт удалён, пропуск".into(),
        }

        if !self.queue.is_empty() {
            self.next_launch_at = Some(Instant::now() + LAUNCH_GAP);
            ctx.request_repaint_after(LAUNCH_GAP);
        }
    }

    fn copy_cookie(&mut self) {
        let Some(cookie) = self
            .selected_account()
            .map(|account| account.cookie.clone())
        else {
            self.status = "Сначала выбери аккаунт".into();
            return;
        };
        if cookie.is_empty() {
            self.status = "У аккаунта нет cookie".into();
            return;
        }
        self.set_clipboard(&cookie);
    }

    fn toggle_dpapi(&mut self, enabled: bool) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match store.set_dpapi(enabled) {
            Ok(()) => {
                self.status = if enabled {
                    "Файл привязан к этому пользователю Windows".into()
                } else {
                    "Привязка к Windows снята".into()
                };
            }
            Err(err) => self.status = format!("DPAPI: {err}"),
        }
    }

    fn open_transfer(&mut self, kind: TransferKind) {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let path = if home.is_empty() {
            "rbx-accounts.dat".to_string()
        } else {
            format!("{home}\\Desktop\\rbx-accounts.dat")
        };

        self.transfer = Some(Transfer {
            kind,
            path,
            password: String::new(),
            message: String::new(),
            failed: false,
        });
    }

    fn run_transfer(&mut self, mut dialog: Transfer) {
        match dialog.kind {
            TransferKind::Export => {
                let target = PathBuf::from(dialog.path.trim());
                match std::fs::copy(&self.path, &target) {
                    Ok(bytes) => {
                        dialog.message =
                            format!("Экспортировано: {} байт → {}", bytes, target.display());
                        dialog.failed = false;
                        self.status = "Экспорт готов".into();
                    }
                    Err(err) => {
                        dialog.message = format!("Ошибка экспорта: {err}");
                        dialog.failed = true;
                    }
                }
            }
            TransferKind::Import => {
                let source = PathBuf::from(dialog.path.trim());
                match store::read_accounts(&source, &dialog.password) {
                    Ok(accounts) => {
                        let total = accounts.len();
                        let added = match self.store.as_mut() {
                            Some(store) => store.merge(accounts),
                            None => 0,
                        };
                        match self.persist() {
                            Some(err) => {
                                dialog.message = format!("Ошибка сохранения: {err}");
                                dialog.failed = true;
                            }
                            None => {
                                dialog.message = format!(
                                    "В файле: {total}, добавлено: {added} (дубликаты пропущены)"
                                );
                                dialog.failed = false;
                                self.status = format!("Импортировано: {added}");
                            }
                        }
                    }
                    Err(err) => {
                        dialog.message = format!("Ошибка импорта: {err}");
                        dialog.failed = true;
                    }
                }
            }
        }

        self.transfer = Some(dialog);
    }

    // --------------------------------------------------------------- worker
    fn poll_worker(&mut self, ctx: &egui::Context) {
        while let Some(done) = self.worker.try_recv() {
            match done {
                Done::Validated { id, result } => {
                    self.pending = self.pending.saturating_sub(1);
                    self.apply_validated(id, result);
                }
                Done::Launched { label, result } => {
                    self.pending = self.pending.saturating_sub(1);
                    self.status = match result {
                        Ok(outcome) => format!("{label}: запущен ({})", outcome.summary()),
                        Err(err) => format!("{label}: {err}"),
                    };
                }
                Done::Avatar { user_id, result } => {
                    self.avatar_busy = self.avatar_busy.saturating_sub(1);
                    if let Ok(image) = result {
                        let pixels =
                            egui::ColorImage::from_rgba_unmultiplied(image.size, &image.rgba);
                        let texture = ctx.load_texture(
                            format!("avatar-{user_id}"),
                            pixels,
                            egui::TextureOptions::LINEAR,
                        );
                        self.textures.insert(user_id, texture);
                    }
                    // On failure the row just keeps the empty placeholder.
                }
            }
        }
    }

    fn apply_validated(&mut self, id: String, result: Result<roblox::Profile, String>) {
        match result {
            Ok(profile) => {
                if let Some(store) = self.store.as_mut() {
                    store.update(&id, None, None, None, Some(&profile.name), Some(profile.id));
                }
                if let Some(err) = self.persist() {
                    self.status = format!("Ошибка сохранения: {err}");
                    return;
                }
                self.status = format!("OK: {} (id {})", profile.name, profile.id);
            }
            Err(err) => self.status = format!("Cookie не проверен: {err}"),
        }
    }

    fn persist(&self) -> Option<String> {
        match self.store.as_ref().map(Store::save) {
            Some(Err(err)) => Some(err),
            _ => None,
        }
    }

    fn save_config(&self) {
        self.config.save();
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
        if ctx.input(|input| !input.events.is_empty()) {
            self.last_activity = Instant::now();
        }

        self.tick_clipboard();
        self.poll_worker(ctx);
        self.tick_auto_lock();
        self.tick_queue(ctx);

        if self.store.is_none() {
            self.lock_screen(ctx);
        } else {
            self.main_screen(ctx);
        }

        self.dialogs(ctx);

        if self.clipboard_clear_at.is_some()
            || self.pending > 0
            || self.next_launch_at.is_some()
            || self.avatar_busy > 0
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}
