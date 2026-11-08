//! Interface Win32 pure (aucun framework) : une seule fenêtre sert à la fois de
//! widget et de destinataire des messages de l'icône de notification, plus une
//! petite fenêtre de configuration native (URL + jeton).
//!
//! Économie de ressources :
//!  - boucle GetMessage bloquante : 0 % CPU tant qu'aucun message n'arrive ;
//!  - dessin GDI uniquement à la demande (WM_PAINT), et seulement si le widget est visible ;
//!  - working set réduit après chaque masquage / rechargement complet ;
//!  - aucune clé de registre : rien n'est écrit en dehors de %APPDATA%\HaTray.

use crate::{
    config, ha, palette, startup,
    state::{app, Conn, Entity},
};
use std::{
    ffi::c_void,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Instant,
};
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{GetCurrentProcess, SetProcessWorkingSetSize},
        },
        UI::{
            HiDpi::GetDpiForSystem,
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
};

// ------------------------------------------------------------------ constantes

const WM_TRAY: u32 = WM_APP + 1;

const ID_SHOW: usize = 1;
const ID_PIN: usize = 2;
const ID_QUIT: usize = 3;
const ID_SETTINGS: usize = 4;
const ID_ENTITY: usize = 1000; // 1000 + index dans MENU_IDS

// Dimensions logiques (à 96 DPI), converties par s().
const BASE_W: i32 = 280;
const HEADER: i32 = 32;
const ROW_H: i32 = 40;
const PAD: i32 = 12;

// Fenêtre de configuration
const DLG_W: i32 = 420;
const DLG_H: i32 = 240;
const DLG_OK_ID: usize = 1; // IDOK
const DLG_CANCEL_ID: usize = 2; // IDCANCEL
const DLG_URL_ID: usize = 101;
const DLG_TOKEN_ID: usize = 102;
const DLG_STARTUP_ID: usize = 103;
const BM_GETCHECK_MSG: u32 = 0x00F0;
const BM_SETCHECK_MSG: u32 = 0x00F1;
const EM_SETSEL_MSG: u32 = 0x00B1; // EM_SETSEL (module Controls non activé)
const DM_GETDEFID: u32 = 0x0400; // WM_USER : bouton par défaut pour IsDialogMessage
const DC_HASDEFID: isize = 0x534B;

static DPI: AtomicU32 = AtomicU32::new(96);
static USER_MOVED: AtomicBool = AtomicBool::new(false);
static LAST_HIDE_MS: AtomicU64 = AtomicU64::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static MENU_IDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static START: OnceLock<Instant> = OnceLock::new();

// Glissement de la luminosité
static DRAG: Mutex<Option<String>> = Mutex::new(None); // entity_id en cours de réglage
static DRAG_PCT: AtomicI32 = AtomicI32::new(-1); // valeur affichée pendant le glissement
static LAST_SENT_PCT: AtomicI32 = AtomicI32::new(-1);
static LAST_SENT_MS: AtomicU64 = AtomicU64::new(0);
const DRAG_SEND_INTERVAL_MS: u64 = 90; // ~11 requêtes/s max pendant le glissement

// Fenêtre de configuration
static IN_DIALOG: AtomicBool = AtomicBool::new(false);
static DLG_DONE: AtomicBool = AtomicBool::new(false);
static DLG_OK: AtomicBool = AtomicBool::new(false);
static DLG_FIRST: AtomicBool = AtomicBool::new(false);
static DLG_URL_EDIT: AtomicIsize = AtomicIsize::new(0);
static DLG_TOKEN_EDIT: AtomicIsize = AtomicIsize::new(0);
static DLG_STARTUP_CHK: AtomicIsize = AtomicIsize::new(0);
static DLG_OUT: Mutex<(String, String, bool)> = Mutex::new((String::new(), String::new(), false));

// ------------------------------------------------------------------ utilitaires

pub(crate) fn s(v: i32) -> i32 {
    v * DPI.load(Ordering::Relaxed) as i32 / 96
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn wide(t: &str) -> Vec<u16> {
    t.encode_utf16().chain(std::iter::once(0)).collect()
}

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn lo(l: LPARAM) -> i32 {
    (l.0 & 0xFFFF) as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    ((l.0 >> 16) & 0xFFFF) as i16 as i32
}

/// Luminosité HA (0-255) -> pourcentage.
fn pct(b: u8) -> i32 {
    (b as i32 * 100 + 127) / 255
}

/// Pourcentage affiché pour une lumière (une lumière non dimmable allumée = 100 %).
fn level(e: &Entity) -> i32 {
    if !e.on {
        0
    } else if e.brightness == 0 {
        100
    } else {
        pct(e.brightness)
    }
}

unsafe fn trim_memory() {
    // Rend au système les pages inactives du processus.
    let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
}

fn init_dpi() {
    unsafe {
        let _ = SetProcessDPIAware();
        DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
}

/// Charge l'icône embarquée (ressource n°1, voir build.rs) à la taille demandée ;
/// repli sur l'icône système par défaut si elle est absente.
unsafe fn load_icon(px: i32) -> HICON {
    if let Ok(module) = GetModuleHandleW(None) {
        let hinst: HINSTANCE = module.into();
        if let Ok(h) = LoadImageW(hinst, PCWSTR(1 as *const u16), IMAGE_ICON, px, px, LR_DEFAULTCOLOR) {
            return HICON(h.0);
        }
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

unsafe fn msgbox(owner: HWND, text: &str, flags: MESSAGEBOX_STYLE) {
    let t = wide(text);
    MessageBoxW(owner, PCWSTR(t.as_ptr()), w!("HaTray"), MB_OK | flags);
}

// ------------------------------------------------------------------ point d'entrée UI

pub fn run() -> windows::core::Result<()> {
    unsafe {
        init_dpi();

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("HaTrayWidget");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // Fenêtre : sans bordure (WS_POPUP), absente de la barre des tâches (TOOLWINDOW),
        // toujours au-dessus (TOPMOST), semi-transparente (LAYERED).
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class,
            w!("Home Assistant"),
            WS_POPUP,
            0,
            0,
            s(BASE_W),
            s(HEADER + ROW_H),
            None,
            None,
            hinst,
            None,
        )?;
        SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA)?;

        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        add_tray(hwnd)?;
        ha::spawn(hwnd.0 as isize);

        // Boucle de messages : GetMessageW BLOQUE => aucun cycle CPU consommé au repos.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ procédure de fenêtre

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Explorer a redémarré : on remet l'icône.
    let tb = TASKBAR_CREATED.load(Ordering::Relaxed);
    if tb != 0 && msg == tb {
        let _ = add_tray(hwnd);
        return LRESULT(0);
    }

    match msg {
        WM_PAINT => {
            paint_window(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // tout est dessiné en double tampon

        // L'en-tête sert de poignée de déplacement.
        WM_NCHITTEST => {
            let mut pt = POINT { x: lo(lp), y: hi(lp) };
            let _ = ScreenToClient(hwnd, &mut pt);
            LRESULT(if pt.y < s(HEADER) { 2 } else { 1 }) // HTCAPTION : HTCLIENT
        }
        WM_EXITSIZEMOVE => {
            USER_MOVED.store(true, Ordering::Relaxed);
            LRESULT(0)
        }

        WM_LBUTTONDOWN => {
            on_button_down(hwnd, lo(lp), hi(lp));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if DRAG.lock().unwrap().is_some() {
                drag_to(hwnd, lo(lp));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            end_drag(hwnd);
            LRESULT(0)
        }
        // Capture perdue (Alt+Tab, etc.) : on abandonne proprement le glissement.
        WM_CAPTURECHANGED => {
            let was_dragging = DRAG.lock().unwrap().take().is_some();
            DRAG_PCT.store(-1, Ordering::Relaxed);
            if was_dragging {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        // Clic droit sur une lumière : sélecteur de couleur.
        WM_RBUTTONUP => {
            if let Some((idx, _)) = row_at(hi(lp)) {
                if let Some((id, e)) = entity_at(idx) {
                    open_palette(hwnd, &id, &e);
                }
            }
            LRESULT(0)
        }
        // La palette vient de se fermer : on rend le focus au widget, ou on le rétracte
        // si l'utilisateur est allé cliquer ailleurs.
        palette::WM_PALETTE_CLOSED => {
            if wp.0 != 0 {
                let _ = SetForegroundWindow(hwnd);
            } else if !app().pinned.load(Ordering::Relaxed)
                && GetForegroundWindow() != hwnd
                && IsWindowVisible(hwnd).as_bool()
            {
                hide(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = (wp.0 >> 16) as u16 as i16 as i32;
            let mut pt = POINT { x: lo(lp), y: hi(lp) };
            let _ = ScreenToClient(hwnd, &mut pt);
            on_wheel(pt.y, delta);
            LRESULT(0)
        }

        // Perte du focus : le widget se rétracte, sauf s'il est épinglé.
        WM_ACTIVATE => {
            let inactive = (wp.0 & 0xFFFF) == 0;
            if inactive
                && !app().pinned.load(Ordering::Relaxed)
                && DRAG.lock().unwrap().is_none()
                && !palette::is_open()
                && IsWindowVisible(hwnd).as_bool()
            {
                hide(hwnd);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }

        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => toggle_widget(hwnd),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_command(hwnd, wp.0 & 0xFFFF);
            LRESULT(0)
        }

        // Posté par le thread WebSocket (voir ha.rs).
        ha::WM_STATE => {
            on_state(hwnd, wp.0 != 0);
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ------------------------------------------------------------------ affichage / masquage

unsafe fn toggle_widget(hwnd: HWND) {
    if IsWindowVisible(hwnd).as_bool() {
        hide(hwnd);
    } else if now_ms().saturating_sub(LAST_HIDE_MS.load(Ordering::Relaxed)) > 300 {
        // Garde anti-rebond : un clic sur l'icône alors que le widget vient de se
        // masquer (perte de focus) ne doit pas le rouvrir aussitôt.
        show(hwnd);
    }
}

unsafe fn show(hwnd: HWND) {
    relayout(hwnd);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = InvalidateRect(hwnd, None, false);
}

unsafe fn hide(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_HIDE);
    LAST_HIDE_MS.store(now_ms(), Ordering::Relaxed);
    trim_memory();
}

/// Dimensionne la fenêtre selon le nombre d'entités ; la place en bas à droite
/// (près de la zone de notification) tant que l'utilisateur ne l'a pas déplacée.
unsafe fn relayout(hwnd: HWND) {
    let n = app().states.lock().unwrap().len().max(1) as i32;
    let width = s(BASE_W);
    let height = s(HEADER) + n * s(ROW_H) + s(8);

    let (mut x, mut y) = (0, 0);
    let mut flags = SWP_NOACTIVATE;
    if USER_MOVED.load(Ordering::Relaxed) {
        flags |= SWP_NOMOVE;
    } else {
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut RECT as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        x = wa.right - width - s(12);
        y = wa.bottom - height - s(12);
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, flags);

    // Coins arrondis : la région devient propriété du système après SetWindowRgn.
    let rgn = CreateRoundRectRgn(0, 0, width + 1, height + 1, s(14), s(14));
    SetWindowRgn(hwnd, rgn, true);
}

unsafe fn on_state(hwnd: HWND, full: bool) {
    let visible = IsWindowVisible(hwnd).as_bool();
    if visible {
        if full {
            relayout(hwnd);
        }
        let _ = InvalidateRect(hwnd, None, false);
    }
    update_tip(hwnd);
    if full {
        trim_memory(); // le gros JSON initial vient d'être libéré
    }
}

// ------------------------------------------------------------------ interactions

fn row_at(y: i32) -> Option<(usize, i32)> {
    let y0 = s(HEADER);
    if y < y0 {
        return None;
    }
    Some((((y - y0) / s(ROW_H)) as usize, (y - y0) % s(ROW_H)))
}

fn entity_at(idx: usize) -> Option<(String, Entity)> {
    app().states.lock().unwrap().iter().nth(idx).map(|(k, v)| (k.clone(), v.clone()))
}

/// Position x (pixels client) -> pourcentage brut (peut sortir de 0..100).
fn raw_pct(x: i32) -> i32 {
    let track = (s(BASE_W) - 2 * s(PAD)).max(1);
    ((x - s(PAD)) * 100 + track / 2).div_euclid(track)
}

/// Mise à jour locale immédiate (le serveur confirmera par le WebSocket).
fn apply_local(id: &str, p: i32) {
    if let Some(e) = app().states.lock().unwrap().get_mut(id) {
        e.on = p > 0;
        if p > 0 {
            e.brightness = (p * 255 / 100).clamp(1, 255) as u8;
        }
    }
}

/// Appui : scène => activation ; lumière => bascule, ou début de glissement
/// si l'on appuie dans la barre de luminosité en bas de la ligne.
unsafe fn on_button_down(hwnd: HWND, x: i32, y: i32) {
    let Some((idx, ly)) = row_at(y) else { return };
    let Some((id, e)) = entity_at(idx) else { return };

    if !e.is_light {
        ha::activate_scene(&id);
    } else if ly > s(ROW_H) - s(18) {
        *DRAG.lock().unwrap() = Some(id);
        DRAG_PCT.store(-1, Ordering::Relaxed);
        LAST_SENT_PCT.store(-1, Ordering::Relaxed);
        LAST_SENT_MS.store(0, Ordering::Relaxed);
        let _ = SetCapture(hwnd); // la souris peut sortir de la fenêtre sans interrompre le geste
        drag_to(hwnd, x);
    } else if x < s(PAD + 20) && (e.can_color || e.can_temp) {
        open_palette(hwnd, &id, &e); // pastille de couleur à gauche du nom
    } else {
        ha::toggle(&id);
    }
}

/// Ouvre le sélecteur de couleur près du curseur (lumières colorées ou à blanc réglable).
unsafe fn open_palette(hwnd: HWND, id: &str, e: &Entity) {
    if !e.is_light || !(e.can_color || e.can_temp) {
        return;
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    palette::open(hwnd, id, e, pt);
}

/// Pendant le glissement : affichage immédiat + envoi limité (~11/s) vers Home Assistant.
unsafe fn drag_to(hwnd: HWND, x: i32) {
    let Some(id) = DRAG.lock().unwrap().clone() else { return };
    let p = raw_pct(x).clamp(0, 100);

    if DRAG_PCT.swap(p, Ordering::Relaxed) != p {
        let _ = InvalidateRect(hwnd, None, false);
    }

    let now = now_ms();
    let to_send = p.max(1); // on n'éteint pas en cours de route ; l'extinction se fait au relâchement
    if to_send != LAST_SENT_PCT.load(Ordering::Relaxed)
        && now.saturating_sub(LAST_SENT_MS.load(Ordering::Relaxed)) >= DRAG_SEND_INTERVAL_MS
    {
        ha::set_brightness(&id, to_send);
        LAST_SENT_PCT.store(to_send, Ordering::Relaxed);
        LAST_SENT_MS.store(now, Ordering::Relaxed);
    }
}

/// Relâchement : envoi de la valeur finale (0 => extinction), puis fin de la capture.
unsafe fn end_drag(hwnd: HWND) {
    let id = DRAG.lock().unwrap().take();
    let Some(id) = id else { return };

    let p = DRAG_PCT.swap(-1, Ordering::Relaxed);
    if p >= 0 {
        if p != LAST_SENT_PCT.load(Ordering::Relaxed) {
            ha::set_brightness(&id, p);
        }
        apply_local(&id, p);
    }
    let _ = ReleaseCapture();
    let _ = InvalidateRect(hwnd, None, false);
}

/// Molette sur une lumière : ±10 % de luminosité.
fn on_wheel(y: i32, delta: i32) {
    let Some((idx, _)) = row_at(y) else { return };
    let Some((id, e)) = entity_at(idx) else { return };
    if !e.is_light {
        return;
    }
    let step = if delta > 0 { 10 } else { -10 };
    ha::set_brightness(&id, (level(&e) + step).clamp(0, 100));
}

unsafe fn on_command(hwnd: HWND, id: usize) {
    match id {
        ID_SHOW => toggle_widget(hwnd),
        ID_PIN => {
            let pinned = !app().pinned.load(Ordering::Relaxed);
            app().pinned.store(pinned, Ordering::Relaxed);
            if pinned {
                show(hwnd);
            }
        }
        ID_SETTINGS => open_settings(hwnd),
        ID_QUIT => {
            remove_tray(hwnd);
            let _ = DestroyWindow(hwnd);
        }
        n if n >= ID_ENTITY => {
            let target = MENU_IDS.lock().unwrap().get(n - ID_ENTITY).cloned();
            if let Some(eid) = target {
                if eid.starts_with("light.") {
                    ha::toggle(&eid);
                } else {
                    ha::activate_scene(&eid);
                }
            }
        }
        _ => {}
    }
}

/// Menu « Paramètres » : rouvre la fenêtre de configuration et applique le résultat à chaud.
unsafe fn open_settings(hwnd: HWND) {
    if IN_DIALOG.swap(true, Ordering::SeqCst) {
        return; // déjà ouverte
    }
    let cur = app().conn();

    if let Some((url, token, autostart)) = prompt_settings(&cur.url, false) {
        // Lancement au démarrage : raccourci dans le dossier Démarrage (aucun registre).
        if autostart != startup::is_enabled() {
            if let Err(e) = startup::set(autostart) {
                msgbox(hwnd, &format!("Lancement au démarrage impossible : {e}"), MB_ICONERROR);
            }
        }
        // Jeton laissé vide => on conserve l'actuel.
        let token = if token.is_empty() { cur.token.clone() } else { token };
        if url != cur.url || token != cur.token {
            match config::save(&url, &token) {
                Ok(()) => {
                    *app().conn.lock().unwrap() = Conn { url, token };
                    app().states.lock().unwrap().clear();
                    app().last_error.lock().unwrap().clear();
                    ha::reconnect(); // reconnexion immédiate, sans redémarrer l'application
                    on_state(hwnd, true);
                }
                Err(e) => msgbox(hwnd, &format!("Enregistrement impossible : {e}"), MB_ICONERROR),
            }
        }
    }
    IN_DIALOG.store(false, Ordering::SeqCst);
}

// ------------------------------------------------------------------ icône de notification

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

fn set_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    nid.szTip = [0; 128];
    for (i, c) in text.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = c;
    }
}

fn tip_text() -> String {
    let a = app();
    if a.connected.load(Ordering::Relaxed) {
        let on = a.states.lock().unwrap().values().filter(|e| e.is_light && e.on).count();
        format!("Home Assistant — {on} lumière(s) allumée(s)")
    } else {
        let err = a.last_error.lock().unwrap().clone();
        if err.is_empty() {
            "Home Assistant — connexion…".to_string()
        } else {
            format!("Home Assistant — hors ligne : {err}")
        }
    }
}

unsafe fn add_tray(hwnd: HWND) -> windows::core::Result<()> {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // Icône embarquée, à la taille exacte demandée par la zone de notification (nette en haute densité).
    nid.hIcon = load_icon(GetSystemMetrics(SM_CXSMICON));
    set_tip(&mut nid, &tip_text());
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    Ok(())
}

unsafe fn update_tip(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_TIP;
    set_tip(&mut nid, &tip_text());
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn remove_tray(hwnd: HWND) {
    let nid = tray_data(hwnd);
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

// ------------------------------------------------------------------ menu contextuel dynamique

unsafe fn add_item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, label: &str) {
    let text = wide(label);
    let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
}

/// Le menu est reconstruit à chaque clic droit à partir de l'état courant :
/// aucune structure de menu n'est conservée en mémoire entre deux affichages.
unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let Ok(scenes) = CreatePopupMenu() else { return };
    let mut has_scenes = false;
    let mut has_lights = false;

    {
        // Verrous relâchés AVANT TrackPopupMenu (boucle modale => risque d'interblocage).
        let st = app().states.lock().unwrap();
        let mut ids = MENU_IDS.lock().unwrap();
        ids.clear();
        for (id, e) in st.iter() {
            let idx = ids.len();
            ids.push(id.clone());
            if e.is_light {
                has_lights = true;
                let mark = if e.on { "●" } else { "○" };
                add_item(menu, MF_STRING, ID_ENTITY + idx, &format!("{mark}  {}", e.name));
            } else {
                has_scenes = true;
                add_item(scenes, MF_STRING, ID_ENTITY + idx, &e.name);
            }
        }
    }

    if has_scenes {
        let text = wide("Scènes");
        let _ = AppendMenuW(menu, MF_POPUP, scenes.0 as usize, PCWSTR(text.as_ptr()));
    } else {
        let _ = DestroyMenu(scenes);
    }
    if has_lights || has_scenes {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
    add_item(menu, MF_STRING, ID_SHOW, "Afficher / masquer le widget");
    let pin_flag = if app().pinned.load(Ordering::Relaxed) { MF_CHECKED } else { MF_UNCHECKED };
    add_item(menu, MF_STRING | pin_flag, ID_PIN, "Toujours visible");
    add_item(menu, MF_STRING, ID_SETTINGS, "Paramètres…");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    add_item(menu, MF_STRING, ID_QUIT, "Quitter");

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd); // requis pour que le menu se ferme au clic extérieur
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu); // détruit aussi le sous-menu des scènes
}

// ------------------------------------------------------------------ fenêtre de configuration

/// Affiche la fenêtre de configuration (URL + jeton) et attend la fermeture.
///
/// - `first_run = true`  : assistant du premier lancement, jeton obligatoire.
/// - `first_run = false` : menu « Paramètres », jeton vide = conserver l'actuel.
///
/// Renvoie `Some((url, jeton, lancement_au_démarrage))` si l'utilisateur valide, `None` s'il annule.
/// Boucle de messages locale : le reste de l'application continue d'être servi.
pub fn prompt_settings(current_url: &str, first_run: bool) -> Option<(String, String, bool)> {
    unsafe {
        init_dpi();
        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class = w!("HaTraySettings");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(dlg_proc),
            hInstance: hinst,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        RegisterClassW(&wc); // renvoie 0 si déjà enregistrée : sans conséquence

        DLG_DONE.store(false, Ordering::SeqCst);
        DLG_OK.store(false, Ordering::SeqCst);
        DLG_FIRST.store(first_run, Ordering::SeqCst);

        // Fenêtre centrée, taille client fixe.
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT { left: 0, top: 0, right: s(DLG_W), bottom: s(DLG_H) };
        let _ = AdjustWindowRect(&mut rc, style, false);
        let (ww, hh) = (rc.right - rc.left, rc.bottom - rc.top);
        let x = (GetSystemMetrics(SM_CXSCREEN) - ww) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - hh) / 3;
        let title = if first_run { w!("HaTray — Configuration initiale") } else { w!("HaTray — Paramètres") };

        let hdlg = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_CONTROLPARENT,
            class,
            title,
            style,
            x,
            y,
            ww,
            hh,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;

        // Contrôles natifs (STATIC / EDIT / BUTTON) : aucun fichier de ressources.
        let lbl_url = child(hdlg, hinst, w!("STATIC"), "URL de Home Assistant", WINDOW_STYLE(0), 0, 16, 12, 388, 20, 0);
        let edit_url = child(
            hdlg,
            hinst,
            w!("EDIT"),
            current_url,
            WS_TABSTOP | WINDOW_STYLE(0x0080), // ES_AUTOHSCROLL
            0x0200,                            // WS_EX_CLIENTEDGE
            16,
            34,
            388,
            26,
            DLG_URL_ID,
        );
        let lbl_token = child(hdlg, hinst, w!("STATIC"), "Jeton d'accès longue durée", WINDOW_STYLE(0), 0, 16, 72, 388, 20, 0);
        let edit_token = child(
            hdlg,
            hinst,
            w!("EDIT"),
            "",
            WS_TABSTOP | WINDOW_STYLE(0x0080 | 0x0020), // ES_AUTOHSCROLL | ES_PASSWORD
            0x0200,
            16,
            94,
            388,
            26,
            DLG_TOKEN_ID,
        );
        let hint_text = if first_run {
            "Créez-le dans Home Assistant : votre profil → Sécurité → Jetons d'accès longue durée."
        } else {
            "Laissez le jeton vide pour conserver le jeton actuel."
        };
        let hint = child(hdlg, hinst, w!("STATIC"), hint_text, WINDOW_STYLE(0), 0, 16, 126, 388, 42, 0);
        // Case « Lancer au démarrage » (BS_AUTOCHECKBOX), pré-cochée si le raccourci existe déjà.
        let startup_chk = child(
            hdlg,
            hinst,
            w!("BUTTON"),
            "Lancer HaTray au démarrage de Windows",
            WS_TABSTOP | WINDOW_STYLE(0x0003),
            0,
            16,
            170,
            388,
            24,
            DLG_STARTUP_ID,
        );
        if startup::is_enabled() {
            SendMessageW(startup_chk, BM_SETCHECK_MSG, WPARAM(1), LPARAM(0));
        }
        let ok = child(hdlg, hinst, w!("BUTTON"), "OK", WS_TABSTOP | WINDOW_STYLE(0x0001), 0, 212, 204, 90, 28, DLG_OK_ID); // BS_DEFPUSHBUTTON
        let cancel = child(hdlg, hinst, w!("BUTTON"), "Annuler", WS_TABSTOP, 0, 314, 204, 90, 28, DLG_CANCEL_ID);

        DLG_URL_EDIT.store(edit_url.0 as isize, Ordering::SeqCst);
        DLG_TOKEN_EDIT.store(edit_token.0 as isize, Ordering::SeqCst);
        DLG_STARTUP_CHK.store(startup_chk.0 as isize, Ordering::SeqCst);

        let f = font(15, 400);
        for h in [lbl_url, edit_url, lbl_token, edit_token, hint, startup_chk, ok, cancel] {
            SendMessageW(h, WM_SETFONT, WPARAM(f.0 as usize), LPARAM(1));
        }

        // Icône de la fenêtre de configuration (barre de titre et barre des tâches).
        SendMessageW(hdlg, WM_SETICON, WPARAM(0), LPARAM(load_icon(GetSystemMetrics(SM_CXSMICON)).0 as isize));
        SendMessageW(hdlg, WM_SETICON, WPARAM(1), LPARAM(load_icon(GetSystemMetrics(SM_CXICON)).0 as isize));

        let _ = ShowWindow(hdlg, SW_SHOW);
        let _ = SetForegroundWindow(hdlg);
        let _ = SetFocus(edit_url);
        SendMessageW(edit_url, EM_SETSEL_MSG, WPARAM(0), LPARAM(-1));

        // Boucle modale locale. IsDialogMessageW gère Tab, Entrée (OK) et Échap (Annuler).
        let mut msg = MSG::default();
        while !DLG_DONE.load(Ordering::SeqCst) {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                if r.0 == 0 {
                    PostQuitMessage(msg.wParam.0 as i32); // « Quitter » pendant la saisie : on propage
                }
                break;
            }
            if !IsDialogMessageW(hdlg, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let _ = DestroyWindow(hdlg);
        let _ = DeleteObject(HGDIOBJ(f.0));

        if DLG_OK.load(Ordering::SeqCst) {
            Some(DLG_OUT.lock().unwrap().clone())
        } else {
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn child(
    parent: HWND,
    hinst: HINSTANCE,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    ex: u32,
    x: i32,
    y: i32,
    cw: i32,
    ch: i32,
    id: usize,
) -> HWND {
    let t = wide(text);
    CreateWindowExW(
        WINDOW_EX_STYLE(ex),
        class,
        PCWSTR(t.as_ptr()),
        WS_CHILD | WS_VISIBLE | style,
        s(x),
        s(y),
        s(cw),
        s(ch),
        parent,
        HMENU(id as *mut c_void), // pour un contrôle enfant, ce paramètre est son identifiant
        hinst,
        None,
    )
    .unwrap_or_default()
}

unsafe fn get_text(h: HWND) -> String {
    let n = GetWindowTextLengthW(h);
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, &mut buf);
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                DLG_OK_ID => dlg_validate(hwnd),
                DLG_CANCEL_ID => DLG_DONE.store(true, Ordering::SeqCst),
                _ => {}
            }
            LRESULT(0)
        }
        // Croix de la fenêtre : on laisse prompt_settings détruire la fenêtre.
        WM_CLOSE => {
            DLG_DONE.store(true, Ordering::SeqCst);
            LRESULT(0)
        }
        // Indique à IsDialogMessageW que OK est le bouton par défaut (touche Entrée).
        DM_GETDEFID => LRESULT((DC_HASDEFID << 16) | DLG_OK_ID as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn dlg_validate(hdlg: HWND) {
    let url_edit = HWND(DLG_URL_EDIT.load(Ordering::SeqCst) as *mut c_void);
    let token_edit = HWND(DLG_TOKEN_EDIT.load(Ordering::SeqCst) as *mut c_void);

    let url = get_text(url_edit).trim().trim_end_matches('/').to_string();
    let token = get_text(token_edit).trim().to_string();

    let scheme_ok = url.starts_with("http://") || url.starts_with("https://");
    if !scheme_ok || url.len() < 10 {
        msgbox(hdlg, "L'URL doit commencer par http:// ou https:// (exemple : http://192.168.1.50:8123).", MB_ICONWARNING);
        let _ = SetFocus(url_edit);
        return;
    }
    if token.is_empty() && DLG_FIRST.load(Ordering::SeqCst) {
        msgbox(hdlg, "Le jeton d'accès longue durée est obligatoire.", MB_ICONWARNING);
        let _ = SetFocus(token_edit);
        return;
    }

    let chk = HWND(DLG_STARTUP_CHK.load(Ordering::SeqCst) as *mut c_void);
    let autostart = SendMessageW(chk, BM_GETCHECK_MSG, WPARAM(0), LPARAM(0)).0 == 1;

    *DLG_OUT.lock().unwrap() = (url, token, autostart);
    DLG_OK.store(true, Ordering::SeqCst);
    DLG_DONE.store(true, Ordering::SeqCst);
}

// ------------------------------------------------------------------ dessin GDI

// NB : CreateFontW attend des u32 bruts dans windows 0.58 (d'où les « .0 as u32 »).
unsafe fn font(px: i32, weight: i32) -> HFONT {
    CreateFontW(
        -s(px),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        0,
        w!("Segoe UI"),
    )
}

unsafe fn fill(hdc: HDC, r: &RECT, c: COLORREF) {
    let brush = CreateSolidBrush(c);
    FillRect(hdc, r, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

unsafe fn text(hdc: HDC, t: &str, mut r: RECT, c: COLORREF, align: DRAW_TEXT_FORMAT) {
    SetTextColor(hdc, c);
    let mut v: Vec<u16> = t.encode_utf16().collect();
    DrawTextW(hdc, &mut v, &mut r, align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS);
}

unsafe fn paint_window(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (wd, ht) = (rc.right, rc.bottom);

    // Double tampon : on dessine hors écran puis on copie d'un bloc (pas de scintillement,
    // indispensable pour un glissement fluide).
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, wd, ht);
    let old_bmp = SelectObject(mem, HGDIOBJ(bmp.0));

    draw(mem, wd, ht);

    let _ = BitBlt(hdc, 0, 0, wd, ht, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old_bmp);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn draw(hdc: HDC, wd: i32, ht: i32) {
    let a = app();
    fill(hdc, &RECT { left: 0, top: 0, right: wd, bottom: ht }, rgb(30, 30, 36));
    SetBkMode(hdc, TRANSPARENT);

    // Les polices sont créées/détruites à chaque peinture : rien ne reste en mémoire au repos.
    let f_norm = font(15, 400);
    let f_bold = font(15, 700);
    let old_font = SelectObject(hdc, HGDIOBJ(f_norm.0));

    // --- En-tête : pastille d'état de connexion + titre
    let dot = if a.connected.load(Ordering::Relaxed) { rgb(90, 200, 120) } else { rgb(230, 90, 90) };
    text(hdc, "●", RECT { left: s(PAD), top: 0, right: s(PAD + 16), bottom: s(HEADER) }, dot, DT_LEFT);
    SelectObject(hdc, HGDIOBJ(f_bold.0));
    text(
        hdc,
        "Home Assistant",
        RECT { left: s(PAD + 20), top: 0, right: wd - s(PAD), bottom: s(HEADER) },
        rgb(235, 235, 240),
        DT_LEFT,
    );
    SelectObject(hdc, HGDIOBJ(f_norm.0));

    // Instantané du glissement AVANT de verrouiller l'état (ordre de verrouillage constant).
    let drag_id = DRAG.lock().unwrap().clone();
    let drag_pct = DRAG_PCT.load(Ordering::Relaxed);

    // --- Lignes
    let st = a.states.lock().unwrap();
    if st.is_empty() {
        let msg = if a.connected.load(Ordering::Relaxed) { "Aucune lumière ni scène" } else { "Connexion en cours…" };
        text(
            hdc,
            msg,
            RECT { left: s(PAD), top: s(HEADER), right: wd - s(PAD), bottom: s(HEADER + ROW_H) },
            rgb(160, 160, 168),
            DT_LEFT,
        );
    }
    for (i, (id, e)) in st.iter().enumerate() {
        let top = s(HEADER) + i as i32 * s(ROW_H);
        if i > 0 {
            fill(hdc, &RECT { left: s(PAD), top, right: wd - s(PAD), bottom: top + 1 }, rgb(50, 50, 58));
        }

        // Pendant le glissement, la valeur de la souris prime sur l'état serveur.
        let dragged = e.is_light && drag_pct >= 0 && drag_id.as_deref() == Some(id.as_str());
        let lv = if dragged { drag_pct } else { level(e) };
        let lit = if dragged { lv > 0 } else { e.on };

        let band = if e.is_light { s(ROW_H) - s(12) } else { s(ROW_H) };
        let on_color = rgb(255, 200, 80);
        let dim_color = rgb(150, 150, 158);
        // Couleur réelle de la lumière (si fournie) pour la pastille et la barre.
        let light_color = e.rgb.filter(|_| lit).map(|c| rgb(c[0], c[1], c[2])).unwrap_or(on_color);

        let name_color = if e.is_light && !lit { dim_color } else { rgb(235, 235, 240) };
        if e.is_light && (e.can_color || e.can_temp) {
            let dot_color = if lit { light_color } else { dim_color };
            text(hdc, "●", RECT { left: s(PAD), top, right: s(PAD + 18), bottom: top + band }, dot_color, DT_LEFT);
        }
        text(hdc, &e.name, RECT { left: s(PAD + 20), top, right: wd - s(96), bottom: top + band }, name_color, DT_LEFT);

        let (label, label_color) = if !e.is_light {
            ("▶ Activer".to_string(), rgb(120, 180, 255))
        } else if lit {
            (format!("{lv} %"), on_color)
        } else {
            ("Éteinte".to_string(), dim_color)
        };
        text(hdc, &label, RECT { left: wd - s(96), top, right: wd - s(PAD), bottom: top + band }, label_color, DT_RIGHT);

        // Barre de luminosité : cliquable et déplaçable (lumières uniquement)
        if e.is_light {
            let (bx0, bx1) = (s(PAD), wd - s(PAD));
            let (by0, by1) = (top + s(ROW_H) - s(12), top + s(ROW_H) - s(8));
            fill(hdc, &RECT { left: bx0, top: by0, right: bx1, bottom: by1 }, rgb(58, 58, 66));
            let fx = bx0 + (bx1 - bx0) * lv / 100;
            if fx > bx0 {
                fill(hdc, &RECT { left: bx0, top: by0, right: fx, bottom: by1 }, light_color);
            }
        }
    }
    drop(st);

    SelectObject(hdc, old_font);
    let _ = DeleteObject(HGDIOBJ(f_norm.0));
    let _ = DeleteObject(HGDIOBJ(f_bold.0));
}
