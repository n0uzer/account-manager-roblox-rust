//! RbxAccountManager — a local open-source Roblox account manager.
//!
//! Windows-only: multiple game windows are started through
//! `RobloxPlayerBeta.exe --app`, single launches go through the
//! `roblox-player:` protocol handler.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod avatar;
mod client_settings;
mod config;
mod crypto;
mod dpapi;
mod jobs;
mod roblox;
mod singleton;
mod store;
mod throttle;
mod ui;
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
        Box::new(|_cc| Ok(Box::new(ui::App::new()))),
    )
}
