//! État global partagé entre le thread WebSocket et le thread d'interface.
//! On ne garde que ce qui est affiché : nom, on/off, luminosité.

use std::{
    collections::BTreeMap,
    sync::{atomic::AtomicBool, Mutex, OnceLock},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Entity {
    pub name: String,
    pub on: bool,
    /// Luminosité Home Assistant (0-255). 0 si inconnue / éteinte.
    pub brightness: u8,
    pub is_light: bool, // sinon : scène
}

/// Paramètres de connexion. Modifiables à chaud via le menu « Paramètres ».
#[derive(Clone)]
pub struct Conn {
    pub url: String,
    pub token: String,
}

pub struct App {
    pub conn: Mutex<Conn>,
    /// Liste blanche d'entity_id ; vide = toutes les lumières et scènes.
    pub entities: Vec<String>,
    /// BTreeMap => ordre stable (lumières avant scènes, tri alphabétique).
    pub states: Mutex<BTreeMap<String, Entity>>,
    pub connected: AtomicBool,
    pub pinned: AtomicBool,
    pub last_error: Mutex<String>,
}

pub static APP: OnceLock<App> = OnceLock::new();

pub fn app() -> &'static App {
    APP.get().expect("APP non initialisée")
}

impl App {
    /// Copie des paramètres de connexion courants.
    pub fn conn(&self) -> Conn {
        self.conn.lock().unwrap().clone()
    }

    /// Seuls les domaines light.* et scene.* nous intéressent.
    pub fn wanted(&self, id: &str) -> bool {
        (id.starts_with("light.") || id.starts_with("scene."))
            && (self.entities.is_empty() || self.entities.iter().any(|e| e == id))
    }
}
