//! The presets window and the Presets menu.

use eframe::egui::{self, Align2, RichText};

use super::{App, Tone};
use crate::memory;
use crate::presets::{Preset, Presets};
use crate::singleton;

pub struct PresetsWindow {
    new_name: String,
    error: String,
}

impl PresetsWindow {
    pub fn new() -> Self {
        PresetsWindow {
            new_name: String::new(),
            error: String::new(),
        }
    }
}

enum PresetAction {
    Apply(String),
    Run(String),
    Update(String),
    Delete(String),
    Autorun(String, bool),
}

impl App {
    pub(super) fn open_presets(&mut self) {
        if self.presets_window.is_none() {
            self.presets_window = Some(PresetsWindow::new());
        }
    }

    /// Quick access from the menu bar. Applying is safe to do by accident:
    /// it only ticks accounts and fills the toolbar, nothing is launched.
    pub(super) fn presets_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Управление пресетами…").clicked() {
            self.open_presets();
            ui.close_menu();
        }
        if self.presets.items.is_empty() {
            return;
        }
        ui.separator();
        ui.label(RichText::new("Применить (без запуска):").weak());
        let names: Vec<String> = self
            .presets
            .items
            .iter()
            .map(|preset| preset.name.clone())
            .collect();
        for name in names {
            if ui.button(&name).clicked() {
                self.apply_preset(&name);
                ui.close_menu();
            }
        }
    }

    /// Tick the preset's accounts and restore its launch settings. Nothing is
    /// launched. False when there is no such preset.
    pub(super) fn apply_preset(&mut self, name: &str) -> bool {
        let Some(preset) = self.presets.find(name).cloned() else {
            return false;
        };
        let known: Vec<String> = self
            .store
            .as_ref()
            .map(|store| {
                store
                    .accounts
                    .iter()
                    .map(|account| account.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let live: Vec<String> = Presets::live_accounts(&preset, &known)
            .into_iter()
            .map(str::to_string)
            .collect();
        let missing = preset.accounts.len() - live.len();

        self.config.apply_launch_settings(&preset.settings);
        self.launch_set = live;

        // Whatever the settings imply on its own.
        if self.config.multi_instance {
            if let Err(message) = singleton::enable() {
                self.warn(message);
            }
        }
        if self.config.trim_memory {
            memory::enable();
        }
        self.save_config();

        let text = format!(
            "Пресет «{}» применён: отмечено аккаунтов {}",
            preset.name,
            self.launch_set.len()
        );
        if missing > 0 {
            self.warn(format!("{text} (удалённых аккаунтов пропущено: {missing})"));
        } else {
            self.info(text);
        }
        true
    }

    /// Run the preset marked for automatic start, right after unlocking.
    pub(super) fn run_autorun_preset(&mut self) {
        let Some(name) = self.presets.autorun.clone() else {
            return;
        };
        if !self.apply_preset(&name) {
            return;
        }
        if self.launch_set.is_empty() {
            self.warn(format!(
                "Автозапуск «{name}»: в пресете не осталось аккаунтов"
            ));
            return;
        }
        self.info(format!("Автозапуск пресета «{name}»…"));
        self.launch_batch();
    }

    /// Remember the ticked accounts and the launch settings under `name`.
    fn save_current_preset(&mut self, name: &str) -> Result<(), String> {
        if self.launch_set.is_empty() {
            return Err("Отметь галочками аккаунты, которые войдут в пресет".into());
        }
        self.presets.upsert(Preset {
            name: name.to_string(),
            accounts: self.launch_set.clone(),
            settings: self.config.launch_settings(),
        })?;
        self.presets.save()
    }

    pub(super) fn presets_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut window) = self.presets_window.take() else {
            return;
        };

        let mut open = true;
        let mut action: Option<PresetAction> = None;
        let mut save_new = false;
        let marked = self.launch_set.len();
        let rows: Vec<(String, usize, bool)> = self
            .presets
            .items
            .iter()
            .map(|preset| {
                (
                    preset.name.clone(),
                    preset.accounts.len(),
                    self.presets.is_autorun(&preset.name),
                )
            })
            .collect();

        egui::Window::new("Пресеты")
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Пресет помнит отмеченные галочками аккаунты и всё, что стоит в панелях \
                         запуска: игру, сервер, режим окон, лимиты. Cookie в нём нет.",
                    )
                    .weak(),
                );
                ui.separator();

                if rows.is_empty() {
                    ui.label(RichText::new("Пока нет ни одного пресета.").weak());
                }
                for (name, count, autorun) in &rows {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(name).strong());
                        ui.label(RichText::new(format!("· аккаунтов: {count}")).weak());

                        if ui
                            .button("Применить")
                            .on_hover_text("Отметить аккаунты и восстановить настройки запуска")
                            .clicked()
                        {
                            action = Some(PresetAction::Apply(name.clone()));
                        }
                        if ui
                            .button("Запустить")
                            .on_hover_text("Применить и сразу запустить всех пакетом")
                            .clicked()
                        {
                            action = Some(PresetAction::Run(name.clone()));
                        }
                        if ui
                            .add_enabled(marked > 0, egui::Button::new("Обновить"))
                            .on_hover_text("Перезаписать пресет текущими отметками и настройками")
                            .on_disabled_hover_text("Сначала отметь аккаунты галочками")
                            .clicked()
                        {
                            action = Some(PresetAction::Update(name.clone()));
                        }
                        let mut auto = *autorun;
                        if ui
                            .checkbox(&mut auto, "при открытии")
                            .on_hover_text(
                                "Запускать этот пресет сам сразу после ввода пароля. \
                                 Включён может быть только один.",
                            )
                            .changed()
                        {
                            action = Some(PresetAction::Autorun(name.clone(), auto));
                        }
                        let color = Tone::Error.color(ui);
                        if ui.button(RichText::new("×").color(color)).clicked() {
                            action = Some(PresetAction::Delete(name.clone()));
                        }
                    });
                }

                ui.separator();
                ui.label("Сохранить текущее состояние как пресет");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut window.new_name)
                            .hint_text("название, например «Ферма»")
                            .desired_width(240.0),
                    );
                    let label = format!("Сохранить (отмечено: {marked})");
                    if ui.button(label).clicked() {
                        save_new = true;
                    }
                });
                if !window.error.is_empty() {
                    let color = Tone::Error.color(ui);
                    ui.colored_label(color, &window.error);
                }
            });

        if save_new {
            let name = window.new_name.trim().to_string();
            match self.save_current_preset(&name) {
                Ok(()) => {
                    self.ok(format!("Пресет «{name}» сохранён"));
                    window.new_name.clear();
                    window.error.clear();
                }
                Err(message) => window.error = message,
            }
        }

        match action {
            Some(PresetAction::Apply(name)) => {
                self.apply_preset(&name);
            }
            Some(PresetAction::Run(name)) => {
                if self.apply_preset(&name) {
                    self.launch_batch();
                }
            }
            Some(PresetAction::Update(name)) => match self.save_current_preset(&name) {
                Ok(()) => self.ok(format!("Пресет «{name}» обновлён")),
                Err(message) => window.error = message,
            },
            Some(PresetAction::Delete(name)) => {
                self.presets.remove(&name);
                if let Err(err) = self.presets.save() {
                    self.fail(err);
                } else {
                    self.info(format!("Пресет «{name}» удалён"));
                }
            }
            Some(PresetAction::Autorun(name, enabled)) => {
                self.presets.set_autorun(enabled.then_some(name.as_str()));
                if let Err(err) = self.presets.save() {
                    self.fail(err);
                }
            }
            None => {}
        }

        if open {
            self.presets_window = Some(window);
        }
    }
}
