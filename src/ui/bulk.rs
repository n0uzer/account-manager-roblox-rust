//! Bulk cookie import (paste, file, drag and drop) and bulk editing of the
//! accounts ticked in the list.

use std::path::Path;

use eframe::egui::{self, Align2, RichText};
use zeroize::Zeroize;

use super::{App, Tone};
use crate::import::{self, Parsed};
use crate::roblox;
use crate::store::{AccountPatch, DEFAULT_LABEL};

/// Largest file taken for import. A list of thousands of cookies is still far
/// below this; anything bigger is not a cookie list.
const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024;
/// Lines of "skipped" detail shown before the rest is summarised.
const SKIPPED_SHOWN: usize = 5;

/// Text of a file as the import wants it. Notepad's "Unicode" saves UTF-16 with
/// a byte-order mark, so that is read as well as UTF-8; anything else is refused.
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).ok()
}

pub struct BulkImport {
    text: String,
    /// Group given to every imported account; empty for none.
    group: String,
    check_after: bool,
    parsed: Option<Parsed>,
    /// How many parsed cookies the store already has.
    already: usize,
    /// The text changed since it was last parsed.
    dirty: bool,
    message: String,
}

impl BulkImport {
    fn new(text: String) -> Self {
        BulkImport {
            text,
            group: String::new(),
            check_after: true,
            parsed: None,
            already: 0,
            dirty: true,
            message: String::new(),
        }
    }
}

impl Drop for BulkImport {
    fn drop(&mut self) {
        // Cookies are in the text and in the parsed entries.
        self.text.zeroize();
        if let Some(parsed) = self.parsed.as_mut() {
            for entry in &mut parsed.entries {
                entry.cookie.zeroize();
            }
        }
    }
}

/// Fields of the bulk edit that were asked to change.
pub struct BulkEdit {
    ids: Vec<String>,
    set_group: bool,
    group: String,
    set_place: bool,
    place: String,
    set_server: bool,
    server: String,
    error: String,
}

impl BulkEdit {
    fn new(ids: Vec<String>) -> Self {
        BulkEdit {
            ids,
            set_group: false,
            group: String::new(),
            set_place: false,
            place: String::new(),
            set_server: false,
            server: String::new(),
            error: String::new(),
        }
    }
}

/// A bulk edit after checking: `None` leaves a field alone, `Some("")` clears it.
#[derive(Debug, PartialEq, Eq)]
struct Checked {
    group: Option<String>,
    place: Option<String>,
    server: Option<String>,
}

fn check_edit(edit: &BulkEdit) -> Result<Checked, String> {
    if !(edit.set_group || edit.set_place || edit.set_server) {
        return Err("Отметь, что именно менять".into());
    }

    let place = if edit.set_place {
        let text = edit.place.trim();
        if !text.is_empty() && roblox::parse_place_id(text).is_none() {
            return Err("Не разобрал игру: нужен Place ID или ссылка на игру".into());
        }
        Some(text.to_string())
    } else {
        None
    };

    let server = if edit.set_server {
        let text = edit.server.trim();
        if !text.is_empty()
            && roblox::parse_private_server(text).is_none()
            && roblox::parse_job_id(text).is_none()
        {
            return Err("Не разобрал сервер: нужна ссылка на приватный сервер или Job ID".into());
        }
        Some(text.to_string())
    } else {
        None
    };

    Ok(Checked {
        group: edit.set_group.then(|| edit.group.trim().to_string()),
        place,
        server,
    })
}

impl App {
    /// Open the import window, adding `text` to what is already typed there.
    pub(super) fn open_bulk_import(&mut self, text: String) {
        match self.bulk_import.as_mut() {
            Some(window) => {
                if !window.text.is_empty() && !window.text.ends_with('\n') {
                    window.text.push('\n');
                }
                window.text.push_str(&text);
                window.dirty = true;
            }
            None => self.bulk_import = Some(BulkImport::new(text)),
        }
    }

    /// Edit the accounts that are ticked in the list.
    pub(super) fn open_bulk_edit(&mut self) {
        if self.launch_set.is_empty() {
            self.warn("Отметь аккаунты галочками в списке");
            return;
        }
        self.bulk_edit = Some(BulkEdit::new(self.launch_set.clone()));
    }

    /// Files dropped onto the window go to the import.
    pub(super) fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        for file in dropped {
            let text = match (&file.path, &file.bytes) {
                (Some(path), _) => self.read_import_file(path),
                (None, Some(bytes)) => decode_text(bytes),
                (None, None) => None,
            };
            match text {
                Some(text) => self.open_bulk_import(text),
                None => self.warn("Не смог прочитать файл: нужен текст со списком cookie"),
            }
        }
    }

    fn read_import_file(&mut self, path: &Path) -> Option<String> {
        let size = std::fs::metadata(path).ok()?.len();
        if size > MAX_IMPORT_BYTES {
            self.warn("Файл слишком большой для списка cookie (больше 2 МБ)");
            return None;
        }
        decode_text(&std::fs::read(path).ok()?)
    }

    pub(super) fn bulk_windows(&mut self, ctx: &egui::Context) {
        self.bulk_import_window(ctx);
        self.bulk_edit_window(ctx);
    }

    /// Parse the text again if it changed and count what is already stored.
    fn refresh_import(&self, window: &mut BulkImport) {
        if !window.dirty {
            return;
        }
        window.dirty = false;
        if let Some(old) = window.parsed.as_mut() {
            for entry in &mut old.entries {
                entry.cookie.zeroize();
            }
        }
        let parsed = import::parse(&window.text);
        window.already = self
            .store
            .as_ref()
            .map(|store| {
                parsed
                    .entries
                    .iter()
                    .filter(|entry| store.find_by_cookie(&entry.cookie).is_some())
                    .count()
            })
            .unwrap_or(0);
        window.parsed = Some(parsed);
    }

    fn bulk_import_window(&mut self, ctx: &egui::Context) {
        let Some(mut window) = self.bulk_import.take() else {
            return;
        };
        self.refresh_import(&mut window);

        let mut open = true;
        let mut browse = false;
        let mut add = false;
        let mut close = false;

        let (found, skipped_total, repeated) = window
            .parsed
            .as_ref()
            .map(|parsed| (parsed.entries.len(), parsed.skipped.len(), parsed.repeated))
            .unwrap_or((0, 0, 0));
        let fresh = found.saturating_sub(window.already);
        let groups = self
            .store
            .as_ref()
            .map(|store| store.groups())
            .unwrap_or_default();

        egui::Window::new("Импорт cookie списком")
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .default_width(620.0)
            .show(ctx, |ui| {
                ui.label(
                    "Вставь cookie по одному в строке. Перед cookie можно написать название и \
                     поставить Tab или запятую. Файл .txt/.csv можно перетащить в окно.",
                );
                ui.add_space(4.0);

                let edit = ui.add(
                    egui::TextEdit::multiline(&mut window.text)
                        .password(false)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY)
                        .hint_text("_|WARNING:-DO-NOT-SHARE-THIS...|_..."),
                );
                if edit.changed() {
                    window.dirty = true;
                }

                ui.horizontal(|ui| {
                    if ui.button("Из файла…").clicked() {
                        browse = true;
                    }
                    if ui.button("Очистить").clicked() {
                        window.text.zeroize();
                        window.text.clear();
                        window.dirty = true;
                        window.message.clear();
                    }
                });
                ui.add_space(6.0);

                let mut summary = format!("Найдено cookie: {found}");
                if window.already > 0 {
                    summary.push_str(&format!(" · уже есть в менеджере: {}", window.already));
                }
                if repeated > 0 {
                    summary.push_str(&format!(" · повторов в списке: {repeated}"));
                }
                if skipped_total > 0 {
                    summary.push_str(&format!(" · строк пропущено: {skipped_total}"));
                }
                ui.label(RichText::new(summary).strong());

                if let Some(parsed) = &window.parsed {
                    let warn = Tone::Warn.color(ui);
                    for skip in parsed.skipped.iter().take(SKIPPED_SHOWN) {
                        ui.colored_label(warn, format!("строка {}: {}", skip.line, skip.reason));
                    }
                    if parsed.skipped.len() > SKIPPED_SHOWN {
                        ui.label(
                            RichText::new(format!(
                                "…и ещё строк: {}",
                                parsed.skipped.len() - SKIPPED_SHOWN
                            ))
                            .weak(),
                        );
                    }
                }
                ui.add_space(6.0);

                ui.horizontal_wrapped(|ui| {
                    ui.label("Группа для всех:");
                    ui.add(
                        egui::TextEdit::singleline(&mut window.group)
                            .hint_text("необязательно")
                            .desired_width(160.0),
                    );
                    for name in &groups {
                        if ui.small_button(name).clicked() {
                            window.group = name.clone();
                        }
                    }
                });
                ui.checkbox(
                    &mut window.check_after,
                    "Проверить cookie после добавления (по запросу на каждый)",
                );

                if !window.message.is_empty() {
                    let color = Tone::Error.color(ui);
                    ui.colored_label(color, &window.message);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let label = format!("Добавить {fresh}");
                    if ui
                        .add_enabled(fresh > 0, egui::Button::new(RichText::new(label).strong()))
                        .clicked()
                    {
                        add = true;
                    }
                    if ui.button("Закрыть").clicked() {
                        close = true;
                    }
                });
            });

        if browse {
            let picked = rfd::FileDialog::new()
                .set_title("Файл со списком cookie")
                .add_filter("Текст", &["txt", "csv", "log"])
                .pick_file();
            if let Some(path) = picked {
                match self.read_import_file(&path) {
                    Some(text) => {
                        if !window.text.is_empty() && !window.text.ends_with('\n') {
                            window.text.push('\n');
                        }
                        window.text.push_str(&text);
                        window.dirty = true;
                        window.message.clear();
                    }
                    None => window.message = "Не смог прочитать файл как текст".into(),
                }
            }
        }

        if add {
            let added = self.run_bulk_import(&window);
            if added > 0 {
                return;
            }
            window.message = "Ничего не добавилось: смотри журнал".into();
        }

        if open && !close {
            self.bulk_import = Some(window);
        }
    }

    /// Store the new accounts of an import. Returns how many were added.
    fn run_bulk_import(&mut self, window: &BulkImport) -> usize {
        let Some(parsed) = window.parsed.as_ref() else {
            return 0;
        };
        let group = window.group.trim().to_string();

        let mut added: Vec<String> = Vec::new();
        {
            let Some(store) = self.store.as_mut() else {
                return 0;
            };
            let mut counter = store.accounts.len();
            for entry in &parsed.entries {
                if store.find_by_cookie(&entry.cookie).is_some() {
                    continue;
                }
                counter += 1;
                let label = entry
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("{DEFAULT_LABEL} {counter}"));
                let id = store.add(&label, &entry.cookie, "");
                if !group.is_empty() {
                    store.patch(
                        &id,
                        AccountPatch {
                            group: Some(&group),
                            ..AccountPatch::default()
                        },
                    );
                }
                added.push(id);
            }
        }

        if added.is_empty() {
            self.warn("Все cookie из списка уже есть в менеджере");
            return 0;
        }
        if let Some(err) = self.persist() {
            self.fail(format!("Ошибка сохранения: {err}"));
            return 0;
        }

        let count = added.len();
        if window.check_after {
            for id in &added {
                self.start_validate(id);
            }
            self.ok(format!("Добавлено аккаунтов: {count}. Проверяю cookie…"));
        } else {
            self.ok(format!("Добавлено аккаунтов: {count}"));
        }
        count
    }

    fn bulk_edit_window(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.bulk_edit.take() else {
            return;
        };

        // Accounts deleted while the window was open drop out of the edit.
        if let Some(store) = self.store.as_ref() {
            edit.ids.retain(|id| store.find(id).is_some());
        }
        if edit.ids.is_empty() {
            return;
        }

        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        let groups = self
            .store
            .as_ref()
            .map(|store| store.groups())
            .unwrap_or_default();
        let count = edit.ids.len();

        egui::Window::new("Изменить отмеченные")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(480.0);
                ui.label(format!(
                    "Аккаунтов: {count}. Меняются только поля с галочкой."
                ));
                ui.label(RichText::new("Пустое поле с галочкой очищает значение.").weak());
                ui.add_space(6.0);

                ui.checkbox(&mut edit.set_group, "Группа");
                ui.add_enabled_ui(edit.set_group, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut edit.group)
                                .hint_text("название группы")
                                .desired_width(200.0),
                        );
                        for name in &groups {
                            if ui.small_button(name).clicked() {
                                edit.group = name.clone();
                            }
                        }
                    });
                });
                ui.add_space(4.0);

                ui.checkbox(&mut edit.set_place, "Игра");
                ui.add_enabled(
                    edit.set_place,
                    egui::TextEdit::singleline(&mut edit.place)
                        .hint_text("Place ID или ссылка на игру")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);

                ui.checkbox(&mut edit.set_server, "Сервер");
                ui.add_enabled(
                    edit.set_server,
                    egui::TextEdit::singleline(&mut edit.server)
                        .hint_text("приватный сервер или Job ID")
                        .desired_width(f32::INFINITY),
                );

                if !edit.error.is_empty() {
                    ui.add_space(6.0);
                    let color = Tone::Error.color(ui);
                    ui.colored_label(color, &edit.error);
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("Применить").strong()))
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui.button("Отмена").clicked() {
                        cancel = true;
                    }
                });
            });

        if cancel || !open {
            return;
        }
        if !apply {
            self.bulk_edit = Some(edit);
            return;
        }

        match check_edit(&edit) {
            Err(message) => {
                edit.error = message;
                self.bulk_edit = Some(edit);
            }
            Ok(checked) => {
                if let Some(store) = self.store.as_mut() {
                    for id in &edit.ids {
                        store.patch(
                            id,
                            AccountPatch {
                                group: checked.group.as_deref(),
                                place_id: checked.place.as_deref(),
                                server: checked.server.as_deref(),
                                ..AccountPatch::default()
                            },
                        );
                    }
                }
                match self.persist() {
                    Some(err) => self.fail(format!("Ошибка сохранения: {err}")),
                    None => self.ok(format!("Изменено аккаунтов: {count}")),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit() -> BulkEdit {
        BulkEdit::new(vec!["a".into()])
    }

    #[test]
    fn utf8_with_and_without_bom() {
        assert_eq!(decode_text(b"abc").as_deref(), Some("abc"));
        assert_eq!(decode_text(b"\xEF\xBB\xBFabc").as_deref(), Some("abc"));
    }

    #[test]
    fn utf16_from_notepad_is_read() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "ab\ncd".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_text(&bytes).as_deref(), Some("ab\ncd"));
    }

    #[test]
    fn binary_files_are_refused() {
        assert_eq!(
            decode_text(&[0x89, b'P', b'N', b'G', 0xFF, 0x00, 0xC0]),
            None
        );
    }

    #[test]
    fn nothing_ticked_is_not_an_edit() {
        assert!(check_edit(&edit()).is_err());
    }

    #[test]
    fn only_ticked_fields_are_changed() {
        let mut item = edit();
        item.set_group = true;
        item.group = "  alts ".into();
        // Typed but not ticked: must be ignored.
        item.place = "not a game".into();

        let checked = check_edit(&item).expect("valid");
        assert_eq!(checked.group.as_deref(), Some("alts"));
        assert_eq!(checked.place, None);
        assert_eq!(checked.server, None);
    }

    #[test]
    fn a_ticked_empty_field_clears_the_value() {
        let mut item = edit();
        item.set_place = true;
        item.set_server = true;
        let checked = check_edit(&item).expect("valid");
        assert_eq!(checked.place.as_deref(), Some(""));
        assert_eq!(checked.server.as_deref(), Some(""));
    }

    #[test]
    fn a_game_link_and_a_job_id_are_accepted() {
        let mut item = edit();
        item.set_place = true;
        item.place = "https://www.roblox.com/games/1818/Name".into();
        item.set_server = true;
        item.server = "d1e7b1e3-129e-404f-8882-d29d26c72dcd".into();
        let checked = check_edit(&item).expect("valid");
        assert_eq!(
            checked.server.as_deref(),
            Some("d1e7b1e3-129e-404f-8882-d29d26c72dcd")
        );
    }

    #[test]
    fn garbage_in_a_ticked_field_is_refused() {
        let mut item = edit();
        item.set_place = true;
        item.place = "hello".into();
        assert!(check_edit(&item).is_err());

        let mut item = edit();
        item.set_server = true;
        item.server = "hello".into();
        assert!(check_edit(&item).is_err());
    }
}
