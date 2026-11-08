// Pas de fenêtre console en release.
#![windows_subsystem = "windows"]

mod config;
mod ha;
mod palette;
mod startup;
mod state;
mod ui;

use state::{App, Conn, APP};
use std::{
    collections::BTreeMap,
    sync::{atomic::AtomicBool, Mutex},
};
use windows::{
    core::{w, PCWSTR},
    Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
};

fn error_box(msg: &str) {
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), w!("HaTray"), MB_OK | MB_ICONERROR);
    }
}

fn main() {
    // Configuration absente, vide ou illisible => assistant de premier lancement.
    let cfg = match config::load() {
        Some(c) => c,
        None => {
            let Some((url, token, autostart)) = ui::prompt_settings("http://homeassistant.local:8123", true) else {
                return; // l'utilisateur a annulé : on quitte sans rien écrire
            };
            if let Err(e) = config::save(&url, &token) {
                error_box(&format!("Enregistrement impossible : {e}"));
                return;
            }
            if autostart != startup::is_enabled() {
                if let Err(e) = startup::set(autostart) {
                    error_box(&format!("Lancement au démarrage impossible : {e}"));
                }
            }
            match config::load() {
                Some(c) => c,
                None => {
                    error_box("Configuration illisible après enregistrement.");
                    return;
                }
            }
        }
    };

    let _ = APP.set(App {
        conn: Mutex::new(Conn { url: cfg.url, token: cfg.token }),
        entities: cfg.entities,
        states: Mutex::new(BTreeMap::new()),
        connected: AtomicBool::new(false),
        pinned: AtomicBool::new(false),
        last_error: Mutex::new(String::new()),
    });

    if let Err(e) = ui::run() {
        error_box(&format!("Erreur d'interface : {e}"));
    }
}
