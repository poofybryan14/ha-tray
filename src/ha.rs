//! Client Home Assistant.
//!
//! - WebSocket (thread dédié, lecture BLOQUANTE) : état initial + abonnement aux changements.
//!   Le thread dort dans le noyau tant que rien n'arrive => 0 % CPU au repos.
//!   Un ping n'est envoyé qu'après 40 s de silence (détection de connexion morte).
//! - REST (un thread éphémère par action) : toggle, luminosité, scènes.
//! - Changement de paramètres : `reconnect()` ferme la socket (la lecture bloquante
//!   échoue aussitôt) et réveille le thread s'il dort dans son délai de reconnexion.

use crate::state::{app, Conn, Entity};
use serde::Deserialize;
use serde_json::json;
use std::{
    io::ErrorKind,
    net::{Shutdown, TcpStream},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::Thread,
    time::Duration,
};
use tungstenite::{stream::MaybeTlsStream, Error as WsError, Message, WebSocket};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{PostMessageW, WM_APP},
};

/// Message posté à la fenêtre : l'état a changé (wparam = 1 => la liste des entités a changé).
pub const WM_STATE: u32 = WM_APP + 2;

type Sock = WebSocket<MaybeTlsStream<TcpStream>>;

/// Incrémenté à chaque changement de paramètres : les sessions obsolètes s'arrêtent.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Clone de la socket courante, pour pouvoir la fermer depuis un autre thread.
static STREAM: Mutex<Option<TcpStream>> = Mutex::new(None);
static WS_THREAD: OnceLock<Thread> = OnceLock::new();

// ------------------------------------------------------------------ JSON typé minimal
// serde ignore tous les champs non déclarés : on n'alloue que le strict nécessaire.

#[derive(Deserialize)]
struct Head {
    id: Option<u64>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct StatesMsg {
    result: Vec<RawState>,
}

#[derive(Deserialize)]
struct EventMsg {
    event: EventBody,
}

#[derive(Deserialize)]
struct EventBody {
    data: EventData,
}

#[derive(Deserialize)]
struct EventData {
    entity_id: String,
    new_state: Option<RawState>,
}

#[derive(Deserialize)]
struct RawState {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: Attrs,
}

#[derive(Deserialize, Default)]
struct Attrs {
    friendly_name: Option<String>,
    brightness: Option<f64>,
    rgb_color: Option<Vec<f64>>,
    supported_color_modes: Option<Vec<String>>,
    min_color_temp_kelvin: Option<f64>,
    max_color_temp_kelvin: Option<f64>,
}

fn to_entity(r: &RawState) -> Entity {
    let a = &r.attributes;
    let modes = a.supported_color_modes.as_deref().unwrap_or(&[]);
    let can_color = modes
        .iter()
        .any(|m| matches!(m.as_str(), "hs" | "xy" | "rgb" | "rgbw" | "rgbww"));
    let can_temp = modes.iter().any(|m| m == "color_temp");
    let rgb = a
        .rgb_color
        .as_ref()
        .filter(|v| v.len() >= 3)
        .map(|v| [v[0].clamp(0.0, 255.0) as u8, v[1].clamp(0.0, 255.0) as u8, v[2].clamp(0.0, 255.0) as u8]);

    Entity {
        name: a.friendly_name.clone().unwrap_or_else(|| r.entity_id.clone()),
        on: r.state == "on",
        brightness: a.brightness.unwrap_or(0.0).clamp(0.0, 255.0) as u8,
        is_light: r.entity_id.starts_with("light."),
        rgb,
        can_color,
        can_temp,
        ct_min: a.min_color_temp_kelvin.unwrap_or(2000.0).clamp(1000.0, 10000.0) as u16,
        ct_max: a.max_color_temp_kelvin.unwrap_or(6500.0).clamp(1000.0, 10000.0) as u16,
    }
}

// ------------------------------------------------------------------ WebSocket

fn notify(hwnd: isize, full: bool) {
    unsafe {
        let _ = PostMessageW(HWND(hwnd as *mut _), WM_STATE, WPARAM(full as usize), LPARAM(0));
    }
}

/// Appelée après un changement d'URL / de jeton : reconnexion immédiate avec les
/// nouveaux paramètres, sans polling ni timer.
pub fn reconnect() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Some(s) = STREAM.lock().unwrap().take() {
        let _ = s.shutdown(Shutdown::Both); // la lecture bloquante du thread WS échoue aussitôt
    }
    if let Some(t) = WS_THREAD.get() {
        t.unpark(); // réveille le thread s'il attend son délai de reconnexion
    }
}

/// Lance le thread de connexion avec reconnexion automatique (backoff 2 s -> 60 s).
pub fn spawn(hwnd: isize) {
    let _ = std::thread::Builder::new()
        .name("ha-ws".into())
        .stack_size(1024 * 1024) // réservation virtuelle seulement, pas de mémoire consommée
        .spawn(move || {
            let _ = WS_THREAD.set(std::thread::current());
            let mut delay = 2u64;
            loop {
                let gen = GENERATION.load(Ordering::SeqCst);
                let result = session(hwnd, &mut delay, gen);

                *STREAM.lock().unwrap() = None;
                let a = app();
                a.connected.store(false, Ordering::Relaxed);

                if GENERATION.load(Ordering::SeqCst) != gen {
                    // Paramètres modifiés : on se reconnecte tout de suite.
                    a.last_error.lock().unwrap().clear();
                    delay = 2;
                    notify(hwnd, false);
                    continue;
                }
                if let Err(e) = result {
                    *a.last_error.lock().unwrap() = e;
                }
                notify(hwnd, false);
                std::thread::park_timeout(Duration::from_secs(delay));
                delay = (delay * 2).min(60);
            }
        });
}

fn set_timeout(sock: &mut Sock, d: Option<Duration>) {
    match sock.get_mut() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(d);
        }
        MaybeTlsStream::NativeTls(s) => {
            let _ = s.get_mut().set_read_timeout(d);
        }
        _ => {}
    }
}

fn clone_stream(sock: &Sock) -> Option<TcpStream> {
    match sock.get_ref() {
        MaybeTlsStream::Plain(s) => s.try_clone().ok(),
        MaybeTlsStream::NativeTls(s) => s.get_ref().try_clone().ok(),
        _ => None,
    }
}

fn send(sock: &mut Sock, v: serde_json::Value) -> Result<(), String> {
    sock.send(Message::Text(v.to_string())).map_err(|e| e.to_string())
}

fn read_text(sock: &mut Sock) -> Result<String, String> {
    loop {
        match sock.read().map_err(|e| e.to_string())? {
            Message::Text(t) => return Ok(t),
            Message::Close(_) => return Err("connexion fermée par le serveur".into()),
            _ => {}
        }
    }
}

fn session(hwnd: isize, delay: &mut u64, gen: u64) -> Result<(), String> {
    let a = app();
    let Conn { url, token } = a.conn();
    let ws_url = format!("{}/api/websocket", url.trim_end_matches('/').replacen("http", "ws", 1));
    let (mut sock, _) = tungstenite::connect(ws_url.as_str()).map_err(|e| e.to_string())?;

    // On garde un clone de la socket pour pouvoir la fermer depuis reconnect().
    *STREAM.lock().unwrap() = clone_stream(&sock);
    if GENERATION.load(Ordering::SeqCst) != gen {
        return Err("paramètres modifiés".into());
    }

    // 1) Authentification : auth_required -> auth -> auth_ok
    let _auth_required = read_text(&mut sock)?;
    send(&mut sock, json!({ "type": "auth", "access_token": token }))?;
    let reply: Head = serde_json::from_str(&read_text(&mut sock)?).map_err(|e| e.to_string())?;
    if reply.kind != "auth_ok" {
        return Err("authentification refusée (jeton invalide ?)".into());
    }
    *delay = 2; // connexion saine : on remet le backoff à zéro

    // 2) État initial (id 1) puis abonnement aux changements (id 2)
    set_timeout(&mut sock, Some(Duration::from_secs(40)));
    send(&mut sock, json!({ "id": 1, "type": "get_states" }))?;
    send(&mut sock, json!({ "id": 2, "type": "subscribe_events", "event_type": "state_changed" }))?;

    a.connected.store(true, Ordering::Relaxed);
    a.last_error.lock().unwrap().clear();
    notify(hwnd, false);

    // 3) Boucle de lecture bloquante
    let mut next_id = 3u64;
    let mut pending_ping = false;
    loop {
        match sock.read() {
            Ok(Message::Text(t)) => {
                pending_ping = false;
                handle(&t, hwnd);
            }
            Ok(Message::Close(_)) => return Err("connexion fermée par le serveur".into()),
            Ok(_) => pending_ping = false,
            // Silence pendant 40 s : on sonde le serveur ; sans réponse au tour suivant => reconnexion.
            Err(WsError::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if pending_ping {
                    return Err("pas de réponse du serveur".into());
                }
                send(&mut sock, json!({ "id": next_id, "type": "ping" }))?;
                next_id += 1;
                pending_ping = true;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn handle(text: &str, hwnd: isize) {
    let a = app();
    let Ok(head) = serde_json::from_str::<Head>(text) else { return };

    match (head.kind.as_str(), head.id) {
        // Réponse à get_states : remplace tout l'état (un seul gros parse, libéré aussitôt).
        ("result", Some(1)) => {
            if let Ok(m) = serde_json::from_str::<StatesMsg>(text) {
                let mut map = a.states.lock().unwrap();
                map.clear();
                for r in &m.result {
                    if a.wanted(&r.entity_id) {
                        map.insert(r.entity_id.clone(), to_entity(r));
                    }
                }
                drop(map);
                notify(hwnd, true);
            }
        }
        // Événement state_changed : on filtre immédiatement et on ne réveille l'UI que si nécessaire.
        ("event", Some(2)) => {
            let Ok(m) = serde_json::from_str::<EventMsg>(text) else { return };
            let d = m.event.data;
            if !a.wanted(&d.entity_id) {
                return;
            }
            let mut map = a.states.lock().unwrap();
            let full;
            match d.new_state {
                Some(ns) => {
                    let e = to_entity(&ns);
                    full = !map.contains_key(&d.entity_id);
                    if map.get(&d.entity_id) == Some(&e) {
                        return; // rien de visible n'a changé
                    }
                    map.insert(d.entity_id, e);
                }
                None => {
                    if map.remove(&d.entity_id).is_none() {
                        return;
                    }
                    full = true;
                }
            }
            drop(map);
            notify(hwnd, full);
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ REST

pub fn toggle(id: &str) {
    call("light", "toggle", json!({ "entity_id": id }));
}

/// pct = 0 => extinction.
pub fn set_brightness(id: &str, pct: i32) {
    if pct <= 0 {
        call("light", "turn_off", json!({ "entity_id": id }));
    } else {
        call("light", "turn_on", json!({ "entity_id": id, "brightness_pct": pct.min(100) }));
    }
}

/// Couleur : teinte 0..360, saturation 0..100. La luminosité n'est pas modifiée.
pub fn set_hs(id: &str, hue: f64, sat: f64) {
    let (h, s) = ((hue * 10.0).round() / 10.0, (sat * 10.0).round() / 10.0);
    call("light", "turn_on", json!({ "entity_id": id, "hs_color": [h, s] }));
}

/// Température de blanc en Kelvin (dans la plage supportée par l'ampoule).
pub fn set_kelvin(id: &str, kelvin: u32) {
    call("light", "turn_on", json!({ "entity_id": id, "color_temp_kelvin": kelvin }));
}

pub fn activate_scene(id: &str) {
    call("scene", "turn_on", json!({ "entity_id": id }));
}

/// Thread éphémère : l'UI ne bloque jamais. Le retour visuel arrive par le WebSocket.
fn call(domain: &'static str, service: &'static str, body: serde_json::Value) {
    std::thread::spawn(move || {
        if let Err(e) = post(domain, service, body) {
            *app().last_error.lock().unwrap() = format!("{domain}.{service} : {e}");
        }
    });
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(5));
        if let Ok(c) = native_tls::TlsConnector::new() {
            b = b.tls_connector(Arc::new(c));
        }
        b.build()
    })
}

fn post(domain: &str, service: &str, body: serde_json::Value) -> Result<(), String> {
    let Conn { url, token } = app().conn();
    let url = format!("{}/api/services/{}/{}", url.trim_end_matches('/'), domain, service);
    agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body)
        .map_err(|e| e.to_string())?;
    Ok(())
}
