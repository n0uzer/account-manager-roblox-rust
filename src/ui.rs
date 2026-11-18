//! egui/eframe user interface.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui::{self, vec2, Align, Align2, Color32, Layout, RichText, Sense, Stroke};
use zeroize::{Zeroize, Zeroizing};

use crate::config::{Config, SortKey};
use crate::jobs::{self, Done, Job};
use crate::roblox::{self, CheckError};
use crate::singleton;
use crate::store::{self, Account, Store, MIN_PASSWORD_CHARS};

const CLIPBOARD_TTL: Duration = Duration::from_secs(60);
/// Gap between queued launches: Roblox dislikes a burst of ticket requests.
const LAUNCH_GAP: Duration = Duration::from_millis(1500);
/// How many avatar downloads may be in flight at once.
const MAX_AVATAR_JOBS: usize = 3;
/// Journal length: old lines fall off the front.
const LOG_LIMIT: usize = 100;

const AVATAR_SIZE: f32 = 24.0;
const ROW_HEIGHT: f32 = 34.0;
const ROW_PAD: f32 = 8.0;
const COL_CHECK: f32 = 30.0;
const COL_AVATAR: f32 = 34.0;
const COL_DOT: f32 = 22.0;
const COL_NAME: f32 = 170.0;
const COL_USER: f32 = 160.0;
const COL_ID: f32 = 100.0;

const AUTO_LOCK_CHOICES: [u32; 5] = [0, 1, 5, 15, 30];
const QUALITY_CHOICES: [u32; 11] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

/// Kind of message, drives its colour.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Ok,
    Warn,
    Error,
}

impl Tone {
    /// Colours are picked per theme: the old fixed dark tones were close to
    /// unreadable on the dark background.
    fn color(self, ui: &egui::Ui) -> Color32 {
        let dark = ui.visuals().dark_mode;
        match (self, dark) {
            (Tone::Info, _) => ui.visuals().text_color(),
            (Tone::Ok, true) => Color32::from_rgb(120, 210, 130),
            (Tone::Ok, false) => Color32::from_rgb(30, 110, 40),
            (Tone::Warn, true) => Color32::from_rgb(235, 175, 90),
            (Tone::Warn, false) => Color32::from_rgb(140, 80, 0),
            (Tone::Error, true) => Color32::from_rgb(255, 125, 115),
            (Tone::Error, false) => Color32::from_rgb(180, 40, 40),
        }
    }
}

struct Status {
    text: String,
    tone: Tone,
}

struct LogEntry {
    at: Instant,
    text: String,
    tone: Tone,
}

/// Result of the last cookie check, kept for the current session only.
#[derive(Clone)]
enum Check {
    Checking,
    Valid,
    /// Roblox refused the cookie.
    Dead(String),
    /// The check itself failed, nothing is known about the cookie.
    Unknown(String),
}

struct Form {
    editing: Option<String>,
    label: String,
    cookie: String,
    note: String,
    error: String,
    reveal: bool,
    focus_first: bool,
}

impl Form {
    fn new(editing: Option<String>, label: String, note: String) -> Self {
        Form {
            editing,
            label,
            cookie: String::new(),
            note,
            error: String::new(),
            reveal: false,
            focus_first: true,
        }
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        self.cookie.zeroize();
    }
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
    tone: Tone,
    /// Path the user already was warned about: a second press overwrites it.
    overwrite_armed: Option<String>,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// What the list needs to know about an account. Deliberately no cookie: the
/// list is rebuilt every frame and must not spread copies of secrets around.
struct Row {
    id: String,
    label: String,
    username: String,
    user_id: u64,
    note: String,
}

impl Row {
    fn from(account: &Account) -> Self {
        Row {
            id: account.id.clone(),
            label: account.label.clone(),
            username: account.username.clone(),
            user_id: account.user_id,
            note: account.note.clone(),
        }
    }
}

/// Things you can do with the selected account.
#[derive(Clone, Copy)]
enum Action {
    Launch,
    Edit,
    Validate,
    CopyCookie,
    ShowCookie,
    Delete,
}

/// Bookkeeping for one batch launch, to report a single summary at the end.
struct Batch {
    total: usize,
    finished: usize,
    failed: Vec<String>,
}

pub struct App {
    path: PathBuf,
    needs_create: bool,
    store: Option<Store>,
    config: Config,

    password: String,
    password_repeat: String,
    reveal_password: bool,
    focus_password: bool,
    lock_error: String,

    selected: Option<String>,
    search: String,
    status: Status,
    log: VecDeque<LogEntry>,
    show_log: bool,

    /// Accounts checked for a batch launch.
    launch_set: Vec<String>,
    /// Batch launch queue: ids dispatched one per `LAUNCH_GAP`.
    queue: VecDeque<String>,
    next_launch_at: Option<Instant>,
    batch: Option<Batch>,

    form: Option<Form>,
    show_cookie: Option<String>,
    confirm_delete: Option<String>,
    transfer: Option<Transfer>,

    clipboard_value: Option<Zeroizing<String>>,
    clipboard_clear_at: Option<Instant>,

    worker: jobs::Worker,
    textures: HashMap<u64, egui::TextureHandle>,
    /// Every user id an avatar was asked for (loaded, loading or failed).
    avatars_requested: HashSet<u64>,
    avatars_loading: HashSet<u64>,
    checks: HashMap<String, Check>,
    pending: usize,

    last_activity: Instant,
}

impl App {
    pub fn new() -> Self {
        let path = Store::default_path();
        let needs_create = !Store::exists(&path);
        let config = Config::load();
        let mut app = App {
            path,
            needs_create,
            store: None,
            config,
            password: String::new(),
            password_repeat: String::new(),
            reveal_password: false,
            focus_password: true,
            lock_error: String::new(),
            selected: None,
            search: String::new(),
            status: Status {
                text: "Готово".into(),
                tone: Tone::Info,
            },
            log: VecDeque::new(),
            show_log: false,
            launch_set: Vec::new(),
            queue: VecDeque::new(),
            next_launch_at: None,
            batch: None,
            form: None,
            show_cookie: None,
            confirm_delete: None,
            transfer: None,
            clipboard_value: None,
            clipboard_clear_at: None,
            worker: jobs::Worker::spawn(),
            textures: HashMap::new(),
            avatars_requested: HashSet::new(),
            avatars_loading: HashSet::new(),
            checks: HashMap::new(),
            pending: 0,
            last_activity: Instant::now(),
        };

        // Claim the client's single-instance names before any launch, so that
        // extra windows are not closed by the client itself.
        if app.config.multi_instance {
            if let Err(message) = singleton::enable() {
                app.warn(message);
            }
        }

        app
    }

    // ---------------------------------------------------------------- status
    /// Show a message in the status bar and keep it in the journal.
    fn say(&mut self, tone: Tone, text: impl Into<String>) {
        let text = text.into();
        match self.log.back() {
            Some(last) if last.text == text => {}
            _ => {
                self.log.push_back(LogEntry {
                    at: Instant::now(),
                    text: text.clone(),
                    tone,
                });
                while self.log.len() > LOG_LIMIT {
                    self.log.pop_front();
                }
            }
        }
        self.status = Status { text, tone };
    }

    fn info(&mut self, text: impl Into<String>) {
        self.say(Tone::Info, text);
    }

    fn ok(&mut self, text: impl Into<String>) {
        self.say(Tone::Ok, text);
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.say(Tone::Warn, text);
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.say(Tone::Error, text);
    }

    // ------------------------------------------------------------ lock screen
    fn lock_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let width = (ui.available_width() - 32.0).clamp(240.0, 380.0);
            ui.add_space(((ui.available_height() - 340.0) / 2.0).max(16.0));
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                ui.vertical(|ui| {
                    ui.set_width(width);
                    self.lock_form(ui, width);
                });
            });
        });
    }

    fn lock_form(&mut self, ui: &mut egui::Ui, width: f32) {
        let creating = self.needs_create;

        ui.heading(if creating {
            "Новое хранилище"
        } else {
            "Разблокировать хранилище"
        });
        ui.add_space(6.0);
        ui.label(if creating {
            "Придумай мастер-пароль. Им шифруется файл с cookie \
             (Argon2id + AES-256-GCM), сам пароль нигде не сохраняется: \
             забыл — данные не восстановить."
        } else {
            "Введи мастер-пароль от файла accounts.dat."
        });
        ui.add_space(12.0);

        let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
        let hidden = !self.reveal_password;
        let mut submit = false;

        let first = ui.add(
            egui::TextEdit::singleline(&mut self.password)
                .password(hidden)
                .hint_text("мастер-пароль")
                .desired_width(width),
        );
        if self.focus_password {
            first.request_focus();
            self.focus_password = false;
        }
        if creating {
            ui.add_space(4.0);
            let second = ui.add(
                egui::TextEdit::singleline(&mut self.password_repeat)
                    .password(hidden)
                    .hint_text("повтори пароль")
                    .desired_width(width),
            );
            if first.lost_focus() && enter {
                second.request_focus();
            }
            if second.lost_focus() && enter {
                submit = true;
            }
        } else if first.lost_focus() && enter {
            submit = true;
        }

        ui.add_space(4.0);
        ui.checkbox(&mut self.reveal_password, "Показать пароль");

        if creating && !self.password.is_empty() {
            let typed = self.password.chars().count();
            if typed < MIN_PASSWORD_CHARS {
                let color = Tone::Warn.color(ui);
                ui.colored_label(
                    color,
                    format!("Минимум {MIN_PASSWORD_CHARS} символов (сейчас {typed})"),
                );
            } else if !self.password_repeat.is_empty() {
                if self.password == self.password_repeat {
                    let color = Tone::Ok.color(ui);
                    ui.colored_label(color, "Пароли совпадают");
                } else {
                    let color = Tone::Warn.color(ui);
                    ui.colored_label(color, "Пароли пока не совпадают");
                }
            }
        }

        if !self.lock_error.is_empty() {
            ui.add_space(4.0);
            let color = Tone::Error.color(ui);
            ui.colored_label(color, &self.lock_error);
        }

        ui.add_space(10.0);
        let label = if creating {
            "Создать хранилище"
        } else {
            "Войти"
        };
        if ui
            .add_sized(
                [width, 32.0],
                egui::Button::new(RichText::new(label).strong()),
            )
            .clicked()
        {
            submit = true;
        }
        if submit {
            self.try_unlock();
        }

        ui.add_space(18.0);
        ui.small(RichText::new(format!("Файл: {}", self.path.display())).weak());
        let color = self.status.tone.color(ui);
        ui.small(RichText::new(&self.status.text).color(color));
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
            self.password.zeroize();
            self.password_repeat.zeroize();
            self.reveal_password = false;
            self.last_activity = Instant::now();
            self.info("Хранилище открыто");
        } else {
            // Let the user retype right away.
            self.focus_password = true;
        }
    }

    /// Drop the store (key and cookies are zeroized on drop).
    fn lock_with(&mut self, reason: &str) {
        self.store = None;
        self.selected = None;
        self.launch_set.clear();
        self.queue.clear();
        self.next_launch_at = None;
        self.batch = None;
        self.form = None;
        self.show_cookie = None;
        self.confirm_delete = None;
        self.transfer = None;
        self.checks.clear();
        self.password.zeroize();
        self.password_repeat.zeroize();
        self.reveal_password = false;
        self.focus_password = true;
        self.lock_error.clear();
        self.needs_create = false;
        // A failed download may be retried after the next unlock.
        let loaded = &self.textures;
        self.avatars_requested.retain(|id| loaded.contains_key(id));
        self.info(reason);
    }

    /// Lock after the configured idle time.
    ///
    /// Compared against the *previous* activity stamp: egui only calls `update`
    /// when something happens, so the first event after a long idle must not
    /// reset the timer before it is checked.
    fn tick_auto_lock(&mut self) {
        if self.store.is_none() {
            return;
        }
        let minutes = self.config.auto_lock_minutes;
        if minutes == 0 {
            return;
        }
        if self.last_activity.elapsed() >= Duration::from_secs(u64::from(minutes) * 60) {
            self.lock_with("Заблокировано из-за бездействия");
        }
    }

    // ----------------------------------------------------------- main screen
    fn main_screen(&mut self, ctx: &egui::Context) {
        self.menu_bar(ctx);
        self.toolbar(ctx);
        self.status_bar(ctx);
        self.selection_bar(ctx);
        self.account_list(ctx);
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Файл", |ui| {
                    if ui.button("Экспорт…").clicked() {
                        self.open_transfer(TransferKind::Export);
                        ui.close_menu();
                    }
                    if ui.button("Импорт…").clicked() {
                        self.open_transfer(TransferKind::Import);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Заблокировать").clicked() {
                        self.lock_with("Заблокировано вручную");
                        ui.close_menu();
                    }
                });

                ui.menu_button("Настройки", |ui| {
                    ui.menu_button("Автоблокировка", |ui| {
                        for option in AUTO_LOCK_CHOICES {
                            let picked = self.config.auto_lock_minutes == option;
                            if ui.radio(picked, auto_lock_text(option)).clicked() {
                                self.config.auto_lock_minutes = option;
                                self.last_activity = Instant::now();
                                self.save_config();
                                ui.close_menu();
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
                });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .button("Заблокировать")
                        .on_hover_text("Закрыть хранилище и стереть ключ из памяти")
                        .clicked()
                    {
                        self.lock_with("Заблокировано вручную");
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(6.0);
            self.launch_row(ui);
            ui.add_space(4.0);
            self.limits_row(ui);
            ui.add_space(4.0);
            self.list_row(ui);
            ui.add_space(6.0);
        });
    }

    fn launch_row(&mut self, ui: &mut egui::Ui) {
        let place_valid = self.place_id().is_some();
        let place_invalid = !self.config.place_id.trim().is_empty() && !place_valid;
        let selected_label = self.selected_account().map(|account| account.label.clone());

        ui.horizontal_wrapped(|ui| {
            ui.label("Place ID:");
            let error_color = Tone::Error.color(ui);
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut self.config.place_id)
                        .hint_text("ID или ссылка на игру")
                        .text_color_opt(place_invalid.then_some(error_color))
                        .desired_width(190.0),
                )
                .on_hover_text(
                    "Число из ссылки на игру: в roblox.com/games/1818/Name это 1818.\n\
                     Можно вставить ссылку целиком.",
                );
            if field.lost_focus() {
                // A pasted link turns into the bare number.
                if let Some(id) = self.place_id() {
                    let bare = id.to_string();
                    if bare != self.config.place_id {
                        self.config.place_id = bare;
                    }
                }
                self.save_config();
            }

            let launch_text = match &selected_label {
                Some(label) => format!("Запустить «{}»", shorten(label, 18)),
                None => "Запустить".to_string(),
            };
            let launch = egui::Button::new(
                RichText::new(launch_text)
                    .strong()
                    .color(ui.visuals().selection.stroke.color),
            )
            .fill(ui.visuals().selection.bg_fill);
            let response = ui.add_enabled(selected_label.is_some() && place_valid, launch);
            let response = if selected_label.is_none() {
                response.on_disabled_hover_text("Выбери аккаунт в списке")
            } else if !place_valid {
                response.on_disabled_hover_text("Укажи Place ID или вставь ссылку на игру")
            } else {
                response
            };
            if response.clicked() {
                self.launch();
            }

            let queued = self.launch_set.len();
            let batch_label = if queued == 0 {
                "Запустить выбранные".to_string()
            } else {
                format!("Запустить выбранные ({queued})")
            };
            let batch_ready = queued > 0 && self.config.multi_instance && place_valid;
            let response = ui.add_enabled(batch_ready, egui::Button::new(batch_label));
            let response = if queued == 0 {
                response.on_disabled_hover_text("Отметь аккаунты галочками в списке")
            } else if !self.config.multi_instance {
                response.on_disabled_hover_text(
                    "Включи «Несколько окон»: через deeplink Roblox запускает только один клиент",
                )
            } else if !place_valid {
                response.on_disabled_hover_text("Укажи Place ID или вставь ссылку на игру")
            } else {
                response
            };
            if response.clicked() {
                self.launch_batch();
            }

            if ui
                .checkbox(&mut self.config.multi_instance, "Несколько окон")
                .on_hover_text(
                    "Каждый аккаунт открывается в отдельном окне клиента (--app).\n\
                     Без галочки запуск идёт через deeplink: окно одно.",
                )
                .changed()
            {
                self.save_config();
            }
        });
    }

    fn limits_row(&mut self, ui: &mut egui::Ui) {
        let multi = self.config.multi_instance;
        let mut save = false;

        ui.horizontal_wrapped(|ui| {
            // Free-form on purpose: the game accepts any number, and the old
            // presets only got in the way.
            ui.label("FPS:");
            let fps = number(ui, &mut self.config.fps_cap, 0..=1000, " FPS");
            save |= persist_now(&fps);
            fps.on_hover_text(
                "Любое число кадров в секунду, 0 — без ограничения.\n\
                 Пишется в настройки клиента — GlobalBasicSettings_13.xml, поле \
                 FramerateCap. Это та же настройка, что в меню игры, поэтому клиент \
                 её применяет (в отличие от fast-флагов вроде \
                 DFIntTaskSchedulerTargetFps, которые он отклоняет).\n\
                 Проверить: Esc → Настройки → Производительность.",
            );
            if self.config.fps_cap == 0 {
                ui.label("(без лимита)");
            }

            ui.label("Графика:");
            let options: Vec<(u32, String)> = QUALITY_CHOICES
                .iter()
                .map(|&level| {
                    let text = if level == 0 {
                        "по умолчанию".to_string()
                    } else {
                        format!("уровень {level}")
                    };
                    (level, text)
                })
                .collect();
            let (picked, response) = pick(
                ui,
                "graphics-quality",
                self.config.graphics_quality,
                &options,
                120.0,
            );
            response.on_hover_text(
                "1 — самый низкий, 10 — максимальный (DFIntDebugFRMQualityLevelOverride). \
                 Меньше качество — меньше нагрузки на видеокарту.",
            );
            if let Some(level) = picked {
                self.config.graphics_quality = level;
                save = true;
            }

            let per_window = ui
                .add_enabled_ui(multi, |ui| {
                    let mut save = false;

                    ui.label("Разрешение окна:");
                    let width = number(ui, &mut self.config.window_width, 0..=7680, " px");
                    save |= persist_now(&width);
                    width.on_hover_text(
                        "Ширина клиентской области окна в пикселях.\n\
                         Пишется в настройки клиента (StartScreenSize) и удерживается \
                         первые 90 секунд после запуска.",
                    );

                    ui.label("×");

                    let height = number(ui, &mut self.config.window_height, 0..=4320, " px");
                    save |= persist_now(&height);
                    height.on_hover_text(
                        "Высота клиентской области окна в пикселях.\n\
                         0 в ширине или высоте — окно обычного размера.",
                    );

                    save
                })
                .inner;
            save |= per_window;

            if multi {
                ui.label("(0 — как обычно)");
            } else {
                ui.label("(только для «Несколько окон»): deeplink не отдаёт нам процесс")
                    .on_hover_text(
                        "Размер окна и лимит CPU задаются только у клиентов, которых \
                         запустил менеджер через --app.",
                    );
            }
        });

        if save {
            self.save_config();
        }
    }

    fn list_row(&mut self, ui: &mut egui::Ui) {
        let has_accounts = self
            .store
            .as_ref()
            .map(|store| !store.accounts.is_empty())
            .unwrap_or(false);

        ui.horizontal_wrapped(|ui| {
            if ui.button("Добавить").clicked() {
                self.open_add();
            }

            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("поиск: название, ник, заметка, ID")
                    .desired_width(230.0),
            );
            if !self.search.is_empty()
                && ui
                    .small_button("×")
                    .on_hover_text("Сбросить поиск")
                    .clicked()
            {
                self.search.clear();
            }

            ui.label("Сортировка:");
            let options: Vec<(SortKey, String)> = SortKey::variants()
                .into_iter()
                .map(|variant| (variant, variant.label().to_string()))
                .collect();
            let (picked, _) = pick(ui, "sort", self.config.sort, &options, 130.0);
            if let Some(variant) = picked {
                self.config.sort = variant;
                self.save_config();
            }

            ui.add_enabled_ui(has_accounts, |ui| {
                if ui
                    .button("Проверить все")
                    .on_hover_text(
                        "Проверить cookie у всех аккаунтов (по одному запросу на каждый)",
                    )
                    .clicked()
                {
                    self.validate_all();
                }
            });
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let journal = format!("Журнал ({})", self.log.len());
                    if ui.selectable_label(self.show_log, journal).clicked() {
                        self.show_log = !self.show_log;
                    }
                    if !self.queue.is_empty()
                        && ui
                            .button(format!("Отменить очередь ({})", self.queue.len()))
                            .clicked()
                    {
                        self.cancel_queue();
                    }
                    if self.pending > 0 {
                        ui.small(format!("в работе: {}", self.pending));
                        ui.spinner();
                    }

                    // Whatever room is left belongs to the message.
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let color = self.status.tone.color(ui);
                        ui.add(
                            egui::Label::new(RichText::new(&self.status.text).color(color))
                                .truncate()
                                .selectable(false),
                        )
                        .on_hover_text(&self.status.text);
                    });
                });
            });
            ui.add_space(4.0);
        });
    }

    /// Details and actions for the selected account.
    fn selection_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("selection").show(ctx, |ui| {
            ui.add_space(6.0);
            let selected = self.selected_account().map(|account| {
                (
                    account.label.clone(),
                    account.username.clone(),
                    account.user_id,
                )
            });

            ui.horizontal_wrapped(|ui| {
                match &selected {
                    Some((label, username, user_id)) => {
                        ui.label(RichText::new(label).strong());
                        if !username.is_empty() {
                            ui.label(RichText::new(format!("· {username}")).weak());
                        }
                        if *user_id != 0 {
                            ui.label(RichText::new(format!("· ID {user_id}")).weak());
                        }
                    }
                    None => {
                        ui.label(
                            RichText::new("Выбери аккаунт в списке — действия появятся здесь.")
                                .weak(),
                        );
                    }
                }
                ui.add_space(8.0);

                ui.add_enabled_ui(selected.is_some(), |ui| {
                    if ui.button("Изменить…").clicked() {
                        self.run_action(Action::Edit);
                    }
                    if ui.button("Проверить cookie").clicked() {
                        self.run_action(Action::Validate);
                    }
                    ui.menu_button("Cookie…", |ui| {
                        if ui
                            .button("Скопировать (буфер очистится через 60 с)")
                            .clicked()
                        {
                            self.run_action(Action::CopyCookie);
                            ui.close_menu();
                        }
                        if ui.button("Показать…").clicked() {
                            self.run_action(Action::ShowCookie);
                            ui.close_menu();
                        }
                    });
                    let color = Tone::Error.color(ui);
                    if ui.button(RichText::new("Удалить…").color(color)).clicked() {
                        self.run_action(Action::Delete);
                    }
                });
            });
            ui.add_space(6.0);
        });
    }

    fn account_list(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let rows = self.visible_rows();
            self.request_avatars(&rows);

            if rows.is_empty() {
                self.empty_state(ui);
                return;
            }

            self.table_header(ui, &rows);

            let place_valid = self.place_id().is_some();
            let mut requested: Option<(String, Action)> = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for row in &rows {
                        self.draw_row(ui, row, place_valid, &mut requested);
                    }
                });

            if let Some((id, action)) = requested {
                self.selected = Some(id);
                self.run_action(action);
            }
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let no_accounts = self
            .store
            .as_ref()
            .map(|store| store.accounts.is_empty())
            .unwrap_or(true);

        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            if no_accounts {
                ui.heading("Пока ни одного аккаунта");
                ui.add_space(4.0);
                ui.label("Добавь первый: понадобится значение cookie .ROBLOSECURITY.");
                ui.add_space(12.0);
                if ui
                    .add_sized([200.0, 32.0], egui::Button::new("Добавить аккаунт"))
                    .clicked()
                {
                    self.open_add();
                }
            } else {
                ui.label("Ничего не нашлось по этому запросу.");
                ui.add_space(8.0);
                if ui.button("Сбросить поиск").clicked() {
                    self.search.clear();
                }
            }
        });
    }

    fn table_header(&mut self, ui: &mut egui::Ui, rows: &[Row]) {
        let all_marked = rows.iter().all(|row| self.launch_set.contains(&row.id));

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(ROW_PAD);

            column(ui, COL_CHECK, 22.0, |ui| {
                let mut marked = all_marked;
                if ui
                    .checkbox(&mut marked, "")
                    .on_hover_text("Отметить все показанные аккаунты / снять отметки")
                    .changed()
                {
                    for row in rows {
                        self.set_queued(&row.id, marked);
                    }
                }
            });
            column(ui, COL_AVATAR, 22.0, |_| {});
            column(ui, COL_DOT, 22.0, |_| {});
            for (width, title) in [
                (COL_NAME, "Название"),
                (COL_USER, "Roblox"),
                (COL_ID, "User ID"),
            ] {
                column(ui, width, 22.0, |ui| {
                    ui.label(RichText::new(title).strong());
                });
            }
            ui.label(RichText::new("Заметка").strong());
        });
        ui.separator();
    }

    fn draw_row(
        &mut self,
        ui: &mut egui::Ui,
        row: &Row,
        place_valid: bool,
        requested: &mut Option<(String, Action)>,
    ) {
        let selected = self.selected.as_deref() == Some(row.id.as_str());
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());

        if ui.is_rect_visible(rect) {
            let visuals = ui.visuals();
            let fill = if selected {
                Some(visuals.selection.bg_fill.gamma_multiply(0.6))
            } else if response.hovered() {
                Some(visuals.widgets.hovered.weak_bg_fill)
            } else {
                None
            };
            if let Some(fill) = fill {
                ui.painter().rect_filled(rect, 4.0, fill);
            }
        }

        let mut marked = self.launch_set.contains(&row.id);
        let was_marked = marked;
        let cells = egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(ROW_PAD, 0.0)))
            .layout(Layout::left_to_right(Align::Center));
        ui.allocate_new_ui(cells, |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;

            column(ui, COL_CHECK, ROW_HEIGHT, |ui| {
                ui.checkbox(&mut marked, "")
                    .on_hover_text("Запустить вместе с остальными отмеченными");
            });
            column(ui, COL_AVATAR, ROW_HEIGHT, |ui| self.draw_avatar(ui, row));
            column(ui, COL_DOT, ROW_HEIGHT, |ui| {
                status_dot(ui, self.checks.get(&row.id));
            });
            column(ui, COL_NAME, ROW_HEIGHT, |ui| {
                text_cell(ui, RichText::new(&row.label), None);
            });
            column(ui, COL_USER, ROW_HEIGHT, |ui| {
                if row.username.is_empty() {
                    text_cell(ui, RichText::new("—").weak(), None);
                } else {
                    text_cell(ui, RichText::new(&row.username), None);
                }
            });
            column(ui, COL_ID, ROW_HEIGHT, |ui| {
                if row.user_id == 0 {
                    text_cell(ui, RichText::new("—").weak(), None);
                } else {
                    text_cell(ui, RichText::new(row.user_id.to_string()), None);
                }
            });
            text_cell(ui, RichText::new(&row.note), Some(&row.note));
        });

        if marked != was_marked {
            self.set_queued(&row.id, marked);
        }
        if response.clicked() || response.secondary_clicked() {
            self.selected = Some(row.id.clone());
        }
        if response.double_clicked() && place_valid {
            *requested = Some((row.id.clone(), Action::Launch));
        }

        response.context_menu(|ui| {
            self.selected = Some(row.id.clone());
            if ui
                .add_enabled(place_valid, egui::Button::new("Запустить"))
                .on_disabled_hover_text("Сначала укажи Place ID")
                .clicked()
            {
                *requested = Some((row.id.clone(), Action::Launch));
                ui.close_menu();
            }
            ui.separator();
            for (title, action) in [
                ("Изменить…", Action::Edit),
                ("Проверить cookie", Action::Validate),
                ("Скопировать cookie", Action::CopyCookie),
                ("Показать cookie…", Action::ShowCookie),
            ] {
                if ui.button(title).clicked() {
                    *requested = Some((row.id.clone(), action));
                    ui.close_menu();
                }
            }
            ui.separator();
            let color = Tone::Error.color(ui);
            if ui.button(RichText::new("Удалить…").color(color)).clicked() {
                *requested = Some((row.id.clone(), Action::Delete));
                ui.close_menu();
            }
        });
    }

    /// Round avatar, or a letter in a circle while there is no picture.
    fn draw_avatar(&self, ui: &mut egui::Ui, row: &Row) {
        if let Some(texture) = self.textures.get(&row.user_id) {
            ui.add(egui::Image::new((
                texture.id(),
                vec2(AVATAR_SIZE, AVATAR_SIZE),
            )));
            return;
        }

        let (rect, _) = ui.allocate_exact_size(vec2(AVATAR_SIZE, AVATAR_SIZE), Sense::hover());
        let painter = ui.painter();
        painter.circle_filled(
            rect.center(),
            AVATAR_SIZE / 2.0,
            ui.visuals().widgets.inactive.bg_fill,
        );
        let initial = row
            .label
            .chars()
            .find(|ch| ch.is_alphanumeric())
            .map(|ch| ch.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string());
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            initial,
            egui::FontId::proportional(13.0),
            ui.visuals().weak_text_color(),
        );
    }

    /// Filter by the search query, then apply the configured order.
    fn visible_rows(&self) -> Vec<Row> {
        let Some(store) = self.store.as_ref() else {
            return Vec::new();
        };

        let query = self.search.trim().to_lowercase();
        let mut accounts: Vec<&Account> = store
            .accounts
            .iter()
            .filter(|account| {
                query.is_empty()
                    || account.label.to_lowercase().contains(&query)
                    || account.username.to_lowercase().contains(&query)
                    || account.note.to_lowercase().contains(&query)
                    || account.user_id.to_string().contains(&query)
            })
            .collect();

        match self.config.sort {
            SortKey::Added => {
                // Same-second timestamps (an import) must still end up newest
                // first: reverse the insertion order, then a stable sort.
                accounts.reverse();
                accounts.sort_by_key(|account| Reverse(account.created_at));
            }
            SortKey::Name => accounts.sort_by_cached_key(|account| account.label.to_lowercase()),
            SortKey::Username => {
                // Accounts without a nick yet go last instead of first.
                accounts.sort_by_cached_key(|account| {
                    (account.username.is_empty(), account.username.to_lowercase())
                });
            }
        }

        accounts.into_iter().map(Row::from).collect()
    }

    fn request_avatars(&mut self, rows: &[Row]) {
        for row in rows {
            if self.avatars_loading.len() >= MAX_AVATAR_JOBS {
                break;
            }
            let user_id = row.user_id;
            if user_id == 0
                || self.textures.contains_key(&user_id)
                || self.avatars_requested.contains(&user_id)
            {
                continue;
            }
            self.avatars_requested.insert(user_id);
            self.avatars_loading.insert(user_id);
            self.worker.send(Job::Avatar { user_id });
        }
    }

    // --------------------------------------------------------------- dialogs
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.form_window(ctx);
        self.cookie_window(ctx);
        self.delete_window(ctx);
        self.transfer_window(ctx);
        self.log_window(ctx);
    }

    /// Escape closes the topmost dialog.
    fn handle_escape(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            return;
        }
        if self.confirm_delete.take().is_some()
            || self.form.take().is_some()
            || self.transfer.take().is_some()
            || self.show_cookie.take().is_some()
        {
            return;
        }
        self.show_log = false;
    }

    fn form_window(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else {
            return;
        };

        let mut open = true;
        let mut submit = false;
        let mut cancel = false;
        let enter = ctx.input(|input| input.key_pressed(egui::Key::Enter));
        let editing = form.editing.is_some();

        egui::Window::new(if editing {
            "Изменить аккаунт"
        } else {
            "Добавить аккаунт"
        })
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.set_min_width(520.0);

            ui.label("Название");
            let label = ui.add(
                egui::TextEdit::singleline(&mut form.label)
                    .hint_text("например, Main")
                    .desired_width(f32::INFINITY),
            );
            if form.focus_first {
                label.request_focus();
                form.focus_first = false;
            }
            ui.add_space(6.0);

            ui.label(if editing {
                ".ROBLOSECURITY — новый (пусто = не менять)"
            } else {
                ".ROBLOSECURITY"
            });
            let hidden = !form.reveal;
            let cookie = ui.add(
                egui::TextEdit::singleline(&mut form.cookie)
                    .password(hidden)
                    .desired_width(f32::INFINITY),
            );
            ui.checkbox(&mut form.reveal, "Показать cookie");
            ui.add_space(6.0);

            ui.label("Заметка");
            let note = ui.add(
                egui::TextEdit::singleline(&mut form.note)
                    .hint_text("необязательно")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(8.0);

            if enter && label.lost_focus() {
                cookie.request_focus();
            } else if enter && cookie.lost_focus() {
                note.request_focus();
            } else if enter && note.lost_focus() {
                submit = true;
            }

            let warn = Tone::Warn.color(ui);
            ui.colored_label(
                warn,
                "Cookie = полный доступ к аккаунту. Хранится только локально, в шифрованном виде.",
            );

            if !form.error.is_empty() {
                let color = Tone::Error.color(ui);
                ui.colored_label(color, &form.error);
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(RichText::new("Сохранить").strong()))
                    .clicked()
                {
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
            let cookie = Zeroizing::new(roblox::clean_cookie(&form.cookie));

            if !editing && cookie.is_empty() {
                form.error = "Вставь .ROBLOSECURITY".into();
                self.form = Some(form);
                return;
            }

            let duplicate = self
                .store
                .as_ref()
                .and_then(|store| store.find_by_cookie(&cookie))
                .filter(|account| Some(account.id.as_str()) != form.editing.as_deref())
                .map(|account| account.label.clone());
            if let Some(label) = duplicate {
                form.error = format!("Этот cookie уже сохранён в аккаунте «{label}»");
                self.form = Some(form);
                return;
            }

            self.submit_form(form, cookie);
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
        let cookie = Zeroizing::new(account.cookie.clone());
        let label = account.label.clone();

        let mut open = true;
        egui::Window::new(format!("Cookie — {label}"))
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);
                let color = Tone::Warn.color(ui);
                ui.colored_label(
                    color,
                    "Никому не показывай и не вставляй никуда, кроме клиента Roblox.",
                );
                ui.add_space(6.0);

                // A shared `&str` is a read-only text buffer: no extra copy.
                let mut shown: &str = cookie.as_str();
                ui.add(
                    egui::TextEdit::multiline(&mut shown)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
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
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!(
                    "Удалить «{label}» из менеджера?\nСам аккаунт Roblox это не затронет."
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let color = Tone::Error.color(ui);
                    if ui.button(RichText::new("Удалить").color(color)).clicked() {
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
            match self.persist() {
                Some(err) => self.fail(format!("Ошибка сохранения: {err}")),
                None => self.info(format!("Удалено: {label}")),
            }
            if self.selected.as_deref() == Some(id.as_str()) {
                self.selected = None;
            }
            self.launch_set.retain(|marked| marked != &id);
            self.queue.retain(|queued| queued != &id);
            self.checks.remove(&id);
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
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(560.0);

                ui.label("Путь к файлу");
                ui.horizontal(|ui| {
                    let browse = ui.button("Обзор…").clicked();
                    ui.add(
                        egui::TextEdit::singleline(&mut dialog.path).desired_width(f32::INFINITY),
                    );
                    if browse {
                        if let Some(path) = browse_file(dialog.kind, &dialog.path) {
                            dialog.path = path.display().to_string();
                            dialog.message.clear();
                            dialog.overwrite_armed = None;
                        }
                    }
                });
                ui.add_space(6.0);

                let warn = Tone::Warn.color(ui);
                match dialog.kind {
                    TransferKind::Export => {
                        ui.colored_label(
                            warn,
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
                            warn,
                            "Аккаунты добавятся к текущим; дубликаты по cookie пропускаются.",
                        );
                    }
                }

                if !dialog.message.is_empty() {
                    ui.add_space(6.0);
                    let color = dialog.tone.color(ui);
                    ui.colored_label(color, &dialog.message);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let armed = dialog.overwrite_armed.as_deref() == Some(dialog.path.trim());
                    let run_label = match dialog.kind {
                        TransferKind::Export if armed => "Заменить",
                        TransferKind::Export => "Экспортировать",
                        TransferKind::Import => "Импортировать",
                    };
                    if ui
                        .add(egui::Button::new(RichText::new(run_label).strong()))
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

    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.show_log {
            return;
        }

        let mut open = true;
        let mut clear = false;
        egui::Window::new("Журнал")
            .open(&mut open)
            .default_size([520.0, 280.0])
            .default_pos(egui::pos2(60.0, 90.0))
            .resizable(true)
            .show(ctx, |ui| {
                if self.log.is_empty() {
                    ui.label(RichText::new("Пока пусто.").weak());
                    return;
                }
                if ui.button("Очистить").clicked() {
                    clear = true;
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for entry in self.log.iter().rev() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new(ago(entry.at.elapsed())).weak());
                                let color = entry.tone.color(ui);
                                ui.colored_label(color, &entry.text);
                            });
                        }
                    });
            });

        if clear {
            self.log.clear();
        }
        if !open {
            self.show_log = false;
        }
    }

    // --------------------------------------------------------------- actions
    fn run_action(&mut self, action: Action) {
        match action {
            Action::Launch => self.launch(),
            Action::Edit => self.open_edit(),
            Action::Validate => self.validate(),
            Action::CopyCookie => self.copy_cookie(),
            Action::ShowCookie => self.show_cookie = self.selected.clone(),
            Action::Delete => self.confirm_delete = self.selected.clone(),
        }
    }

    fn open_add(&mut self) {
        self.form = Some(Form::new(None, String::new(), String::new()));
    }

    fn open_edit(&mut self) {
        let Some(id) = self.selected.clone() else {
            self.warn("Сначала выбери аккаунт");
            return;
        };
        let Some(account) = self.store.as_ref().and_then(|store| store.find(&id)) else {
            return;
        };
        self.form = Some(Form::new(
            Some(id),
            account.label.clone(),
            account.note.clone(),
        ));
    }

    fn submit_form(&mut self, form: Form, cookie: Zeroizing<String>) {
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
            self.fail(format!("Ошибка сохранения: {err}"));
            return;
        }

        self.selected = Some(id.clone());
        if cookie.is_empty() {
            self.info("Сохранено");
        } else if self.start_validate(&id) {
            self.info("Сохранено, проверяю cookie…");
        }
    }

    fn selected_account(&self) -> Option<&Account> {
        let id = self.selected.as_deref()?;
        self.store.as_ref().and_then(|store| store.find(id))
    }

    /// Place ID typed in the toolbar: a number or a game link.
    fn place_id(&self) -> Option<u64> {
        roblox::parse_place_id(&self.config.place_id)
    }

    /// Queue a cookie check. False when there is nothing to check.
    fn start_validate(&mut self, id: &str) -> bool {
        let job = match self.store.as_ref().and_then(|store| store.find(id)) {
            Some(account) if !account.cookie.is_empty() => Job::Validate {
                id: account.id.clone(),
                cookie: account.cookie.clone(),
            },
            _ => return false,
        };
        self.checks.insert(id.to_string(), Check::Checking);
        self.pending += 1;
        self.worker.send(job);
        true
    }

    fn validate(&mut self) {
        let Some(account) = self.selected_account() else {
            self.warn("Сначала выбери аккаунт");
            return;
        };
        let (id, label) = (account.id.clone(), account.label.clone());
        if self.start_validate(&id) {
            self.info(format!("Проверяю {label}…"));
        } else {
            self.warn("У аккаунта нет cookie");
        }
    }

    fn validate_all(&mut self) {
        let ids: Vec<String> = self
            .store
            .as_ref()
            .map(|store| store.accounts.iter().map(|a| a.id.clone()).collect())
            .unwrap_or_default();
        let started = ids.iter().filter(|id| self.start_validate(id)).count();
        if started == 0 {
            self.warn("Нечего проверять: нет аккаунтов с cookie");
        } else {
            self.info(format!("Проверяю cookie: {started} шт."));
        }
    }

    fn launch(&mut self) {
        let Some(account) = self.selected_account().cloned() else {
            self.warn("Сначала выбери аккаунт");
            return;
        };
        let Some(place_id) = self.place_id() else {
            self.warn("Place ID должен быть числом или ссылкой на игру");
            return;
        };
        if account.cookie.is_empty() {
            self.warn("У аккаунта нет cookie");
            return;
        }

        if self.config.multi_instance {
            if let Err(message) = singleton::enable() {
                self.warn(message);
                return;
            }
        }

        self.save_config();
        self.pending += 1;
        self.worker.send(Job::Launch {
            label: account.label.clone(),
            place_id,
            cookie: account.cookie.clone(),
            multi: self.config.multi_instance,
            limits: self.limits(),
            in_batch: false,
        });
        self.info(format!("Запускаю {}…", account.label));
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
            self.warn("Отметь аккаунты галочками в списке");
            return;
        }
        if !self.config.multi_instance {
            self.warn("Для запуска нескольких аккаунтов включи «Несколько окон»");
            return;
        }
        if let Err(message) = singleton::enable() {
            self.warn(message);
            return;
        }
        if self.place_id().is_none() {
            self.warn("Place ID должен быть числом или ссылкой на игру");
            return;
        }

        self.queue = self.launch_set.iter().cloned().collect();
        self.batch = Some(Batch {
            total: self.queue.len(),
            finished: 0,
            failed: Vec::new(),
        });
        // A new batch replaces whatever was still pending.
        self.next_launch_at = Some(Instant::now());
        self.info(format!("В очереди: {} аккаунт(ов)", self.queue.len()));
    }

    fn cancel_queue(&mut self) {
        let dropped = self.queue.len();
        self.queue.clear();
        self.next_launch_at = None;
        if let Some(batch) = self.batch.as_mut() {
            batch.total = batch.total.saturating_sub(dropped);
        }
        self.warn(format!("Очередь отменена ({dropped} не запущено)"));
        self.batch_check_done();
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
        let Some(place_id) = self.place_id() else {
            let dropped = self.queue.len() + 1;
            self.queue.clear();
            self.batch = None;
            self.warn(format!(
                "Place ID стал некорректным, очередь сброшена ({dropped} не запущено)"
            ));
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
                    cookie: account.cookie.clone(),
                    multi: self.config.multi_instance,
                    limits: self.limits(),
                    in_batch: true,
                });
                let left = self.queue.len();
                self.info(format!("Запускаю {}… (в очереди: {left})", account.label));
            }
            Some(account) => {
                self.warn(format!("{}: нет cookie, пропуск", account.label));
                self.batch_record(&account.label, Err("нет cookie".into()));
            }
            None => {
                self.warn("Аккаунт удалён, пропуск");
                self.batch_record("аккаунт", Err("удалён".into()));
            }
        }

        if !self.queue.is_empty() {
            self.next_launch_at = Some(Instant::now() + LAUNCH_GAP);
            ctx.request_repaint_after(LAUNCH_GAP);
        }
    }

    /// Count one finished batch member and report when the batch is complete.
    fn batch_record(&mut self, label: &str, result: Result<(), String>) {
        let Some(batch) = self.batch.as_mut() else {
            return;
        };
        batch.finished += 1;
        if let Err(reason) = result {
            // Full text is in the journal; the summary only needs the gist.
            batch
                .failed
                .push(format!("{label}: {}", shorten(&reason, 70)));
        }
        self.batch_check_done();
    }

    fn batch_check_done(&mut self) {
        let Some(batch) = self.batch.as_ref() else {
            return;
        };
        if batch.finished < batch.total {
            return;
        }
        let started = batch.total.saturating_sub(batch.failed.len());
        let summary = if batch.failed.is_empty() {
            (Tone::Ok, format!("Пакет готов: запущено {started}"))
        } else {
            (
                Tone::Warn,
                format!(
                    "Пакет: запущено {started} из {}. Не удалось — {}",
                    batch.total,
                    batch.failed.join("; ")
                ),
            )
        };
        self.batch = None;
        self.say(summary.0, summary.1);
    }

    fn copy_cookie(&mut self) {
        let Some(cookie) = self
            .selected_account()
            .map(|account| Zeroizing::new(account.cookie.clone()))
        else {
            self.warn("Сначала выбери аккаунт");
            return;
        };
        if cookie.is_empty() {
            self.warn("У аккаунта нет cookie");
            return;
        }
        self.set_clipboard(&cookie);
    }

    fn toggle_dpapi(&mut self, enabled: bool) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        match store.set_dpapi(enabled) {
            Ok(()) if enabled => self.ok("Файл привязан к этому пользователю Windows"),
            Ok(()) => self.info("Привязка к Windows снята"),
            Err(err) => self.fail(format!("DPAPI: {err}")),
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
            tone: Tone::Info,
            overwrite_armed: None,
        });
    }

    fn run_transfer(&mut self, mut dialog: Transfer) {
        match dialog.kind {
            TransferKind::Export => self.run_export(&mut dialog),
            TransferKind::Import => self.run_import(&mut dialog),
        }
        self.transfer = Some(dialog);
    }

    fn run_export(&mut self, dialog: &mut Transfer) {
        let typed = dialog.path.trim().to_string();
        let target = PathBuf::from(&typed);

        // Copying a file onto itself truncates it: that would wipe the store.
        if typed.is_empty() {
            dialog.message = "Укажи, куда сохранить файл".into();
            dialog.tone = Tone::Warn;
            return;
        }
        if same_file(&target, &self.path) {
            dialog.message =
                "Это сам рабочий файл хранилища. Выбери другое место для копии.".into();
            dialog.tone = Tone::Error;
            return;
        }
        if target.exists() && dialog.overwrite_armed.as_deref() != Some(typed.as_str()) {
            dialog.message = "Такой файл уже есть. Нажми «Заменить», чтобы перезаписать.".into();
            dialog.tone = Tone::Warn;
            dialog.overwrite_armed = Some(typed);
            return;
        }

        match std::fs::copy(&self.path, &target) {
            Ok(bytes) => {
                dialog.message = format!("Экспортировано: {bytes} байт, файл {}", target.display());
                dialog.tone = Tone::Ok;
                dialog.overwrite_armed = None;
                self.ok("Экспорт готов");
            }
            Err(err) => {
                dialog.message = format!("Ошибка экспорта: {err}");
                dialog.tone = Tone::Error;
            }
        }
    }

    fn run_import(&mut self, dialog: &mut Transfer) {
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
                        dialog.tone = Tone::Error;
                    }
                    None => {
                        dialog.message =
                            format!("В файле: {total}, добавлено: {added} (дубликаты пропущены)");
                        dialog.tone = Tone::Ok;
                        self.ok(format!("Импортировано: {added}"));
                    }
                }
            }
            Err(err) => {
                dialog.message = format!("Ошибка импорта: {err}");
                dialog.tone = Tone::Error;
            }
        }
    }

    // --------------------------------------------------------------- worker
    fn poll_worker(&mut self, ctx: &egui::Context) {
        while let Some(done) = self.worker.try_recv() {
            match done {
                Done::Validated { id, result } => {
                    self.pending = self.pending.saturating_sub(1);
                    self.apply_validated(id, result);
                }
                Done::Launched {
                    label,
                    in_batch,
                    result,
                } => {
                    self.pending = self.pending.saturating_sub(1);
                    match &result {
                        Ok(outcome) => self.ok(format!("{label}: запущен ({})", outcome.summary())),
                        Err(err) => self.fail(format!("{label}: {err}")),
                    }
                    if in_batch {
                        self.batch_record(&label, result.map(|_| ()));
                    }
                }
                Done::Avatar { user_id, result } => {
                    self.avatars_loading.remove(&user_id);
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
                    // On failure the row keeps its letter placeholder and the
                    // id stays in `avatars_requested`: no retry loop.
                }
            }
        }
    }

    fn apply_validated(&mut self, id: String, result: Result<roblox::Profile, CheckError>) {
        let label = self
            .store
            .as_ref()
            .and_then(|store| store.find(&id))
            .map(|account| account.label.clone())
            .unwrap_or_else(|| "аккаунт".to_string());

        match result {
            Ok(profile) => {
                if let Some(store) = self.store.as_mut() {
                    store.update(&id, None, None, None, Some(&profile.name), Some(profile.id));
                }
                self.checks.insert(id, Check::Valid);
                if let Some(err) = self.persist() {
                    self.fail(format!("Ошибка сохранения: {err}"));
                    return;
                }
                self.ok(format!("OK: {} (id {})", profile.name, profile.id));
            }
            Err(err @ CheckError::Dead(_)) => {
                let message = err.message().to_string();
                self.checks.insert(id, Check::Dead(message.clone()));
                self.fail(format!("{label}: {message}"));
            }
            Err(err) => {
                let message = err.message().to_string();
                self.checks.insert(id, Check::Unknown(message.clone()));
                self.warn(format!("{label}: проверка не удалась — {message}"));
            }
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
                    self.clipboard_value = Some(Zeroizing::new(text.to_string()));
                    self.clipboard_clear_at = Some(Instant::now() + CLIPBOARD_TTL);
                    self.ok("Cookie скопирован. Буфер очистится через 60 секунд.");
                }
                Err(err) => self.fail(format!("Буфер обмена: {err}")),
            },
            Err(err) => self.fail(format!("Буфер обмена: {err}")),
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
                .map(|current| current == *expected)
                .unwrap_or(false);
            if matches {
                let _ = clipboard.set_text(String::new());
                self.info("Буфер обмена очищен.");
            }
        }
    }

    // ------------------------------------------------------------ scheduling
    /// Ask egui for the next frames we need even without user input.
    fn schedule_repaints(&self, ctx: &egui::Context) {
        let busy = self.clipboard_clear_at.is_some()
            || self.pending > 0
            || self.next_launch_at.is_some()
            || !self.avatars_loading.is_empty();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        // Without this nobody would wake the app up to check the idle timeout:
        // an idle egui window does not repaint on its own.
        let minutes = self.config.auto_lock_minutes;
        if self.store.is_some() && minutes > 0 {
            let limit = Duration::from_secs(u64::from(minutes) * 60);
            let left = limit.saturating_sub(self.last_activity.elapsed());
            ctx.request_repaint_after(left + Duration::from_millis(100));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Order matters: check the idle timeout *before* this frame's input
        // refreshes the activity stamp.
        self.tick_auto_lock();
        if ctx.input(|input| !input.events.is_empty()) {
            self.last_activity = Instant::now();
        }

        self.handle_escape(ctx);
        self.tick_clipboard();
        self.poll_worker(ctx);
        self.tick_queue(ctx);

        if self.store.is_none() {
            self.lock_screen(ctx);
        } else {
            self.main_screen(ctx);
        }

        self.dialogs(ctx);
        self.schedule_repaints(ctx);
    }
}

// ---------------------------------------------------------------- helpers
fn auto_lock_text(minutes: u32) -> String {
    match minutes {
        0 => "выкл".to_string(),
        minutes => format!("{minutes} мин"),
    }
}

/// Numeric field for a limit: any value can be typed, so the presets are gone.
fn number(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    suffix: &str,
) -> egui::Response {
    ui.add(
        egui::DragValue::new(value)
            .range(range)
            .speed(1.0)
            .suffix(suffix),
    )
}

/// Whether the value should be written to disk right away.
///
/// A drag reports `changed` on every frame, and rewriting the config 60 times a
/// second is pointless: the file is written once, when the drag ends.
fn persist_now(response: &egui::Response) -> bool {
    response.changed() && (!response.dragged() || response.drag_stopped())
}

/// Combo box over `(value, caption)` pairs. Returns the value picked this frame.
fn pick<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    current: T,
    options: &[(T, String)],
    width: f32,
) -> (Option<T>, egui::Response) {
    let selected = options
        .iter()
        .find(|(value, _)| *value == current)
        .map(|(_, text)| text.clone())
        .unwrap_or_default();

    let mut picked = None;
    let response = egui::ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            for (value, text) in options {
                if ui.selectable_label(*value == current, text).clicked() {
                    picked = Some(*value);
                }
            }
        })
        .response;
    (picked, response)
}

/// Fixed-width table cell. Without `set_width` the parent would only advance
/// by the used part of the cell and the columns would drift.
fn column(ui: &mut egui::Ui, width: f32, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        vec2(width, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_width(width);
            ui.set_min_height(height);
            add(ui);
        },
    );
}

/// One line of text that is cut with an ellipsis instead of overflowing.
fn text_cell(ui: &mut egui::Ui, text: RichText, hover: Option<&str>) {
    let response = ui.add(egui::Label::new(text).truncate().selectable(false));
    if let Some(full) = hover.filter(|full| !full.is_empty()) {
        response.on_hover_text(full);
    }
}

/// Small coloured dot that shows the result of the last cookie check.
fn status_dot(ui: &mut egui::Ui, check: Option<&Check>) {
    let (rect, response) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());

    let (tip, color, filled) = match check {
        None => (
            "Cookie ещё не проверялся в этом сеансе".to_string(),
            ui.visuals().weak_text_color(),
            false,
        ),
        Some(Check::Checking) => ("Проверяю…".to_string(), Tone::Warn.color(ui), true),
        Some(Check::Valid) => ("Cookie рабочий".to_string(), Tone::Ok.color(ui), true),
        Some(Check::Dead(message)) => (message.clone(), Tone::Error.color(ui), true),
        Some(Check::Unknown(message)) => (
            format!("Проверка не удалась: {message}"),
            Tone::Warn.color(ui),
            false,
        ),
    };

    let painter = ui.painter();
    if filled {
        painter.circle_filled(rect.center(), 5.0, color);
    } else {
        painter.circle_stroke(rect.center(), 4.5, Stroke::new(1.5_f32, color));
    }
    response.on_hover_text(tip);
}

/// Native file dialog for export (where to save) and import (what to open).
fn browse_file(kind: TransferKind, current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(folder) = Path::new(current.trim())
        .parent()
        .filter(|folder| folder.is_dir())
    {
        dialog = dialog.set_directory(folder);
    }
    match kind {
        TransferKind::Export => dialog
            .set_title("Куда сохранить копию хранилища")
            .set_file_name("rbx-accounts.dat")
            .add_filter("Хранилище (.dat)", &["dat"])
            .save_file(),
        TransferKind::Import => dialog.set_title("Файл хранилища для импорта").pick_file(),
    }
}

/// Whether two paths point at the same file on disk.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Cut a name to `max` characters, adding an ellipsis when something was lost.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Age of a journal line for display.
fn ago(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=4 => "только что".to_string(),
        5..=59 => format!("{secs} с назад"),
        60..=3599 => format!("{} мин назад", secs / 60),
        _ => format!("{} ч назад", secs / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_names_are_shortened_with_an_ellipsis() {
        assert_eq!(shorten("Main", 18), "Main");
        assert_eq!(shorten("abcdefghij", 5), "abcd…");
        // Characters, not bytes: Cyrillic must not be cut in the middle.
        assert_eq!(shorten("Тестовый аккаунт", 8), "Тестовы…");
    }

    #[test]
    fn journal_age_is_human() {
        assert_eq!(ago(Duration::from_secs(1)), "только что");
        assert_eq!(ago(Duration::from_secs(30)), "30 с назад");
        assert_eq!(ago(Duration::from_secs(150)), "2 мин назад");
        assert_eq!(ago(Duration::from_secs(7300)), "2 ч назад");
    }

    #[test]
    fn same_file_sees_through_different_spellings() {
        let dir = std::env::temp_dir().join(format!("rbm-ui-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("accounts.dat");
        std::fs::write(&file, b"x").unwrap();

        let roundabout = dir.join(".").join("accounts.dat");
        assert!(same_file(&file, &roundabout));
        assert!(!same_file(&file, &dir.join("copy.dat")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_lock_captions() {
        assert_eq!(auto_lock_text(0), "выкл");
        assert_eq!(auto_lock_text(15), "15 мин");
    }
}
