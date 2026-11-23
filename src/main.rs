//! RbxAccountManager — a local open-source Roblox account manager.
//!
//! Windows starts game windows through `RobloxPlayerBeta.exe --app`, single
//! launches go through the `roblox-player:` protocol handler. Linux runs the
//! Android client through Sober, one Flatpak profile per account.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod antiafk;
mod autostart;
mod avatar;
mod backup;
mod client_settings;
mod config;
mod crypto;
mod dpapi;
mod game_settings;
mod import;
mod jobs;
#[cfg(target_os = "linux")]
mod kwin;
mod lookup;
mod memory;
mod presets;
mod processes;
mod roblox;
mod singleton;
#[cfg(not(windows))]
mod sober;
mod store;
mod ui;
#[cfg(target_os = "linux")]
mod uinput;
mod watch;
mod window;

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("RbxAccountManager"),
        ..Default::default()
    };

    eframe::run_native(
        "RbxAccountManager",
        options,
        Box::new(|cc| {
            let app = ui::App::new();
            app.apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
