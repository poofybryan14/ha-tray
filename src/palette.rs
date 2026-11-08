//! Sélecteur de couleur léger : une petite fenêtre GDI créée à la demande et détruite
//! dès qu'on a choisi (ou qu'on clique ailleurs). Rien n'est conservé en mémoire.
//!
//! - Plan teinte / saturation  -> `hs_color`        (lumières colorées)
//! - Bande de températures     -> `color_temp_kelvin` (lumières à blanc réglable)
//!
//! La luminosité reste réglée par la barre du widget : la couleur ne la modifie pas.
//! Les pixels sont calculés à chaque peinture (quelques milliers d'opérations) puis
//! étirés par StretchDIBits : aucune bitmap, aucune police, aucun pinceau persistant.

use crate::{ha, state::Entity, ui::s};
use std::{
    ffi::c_void,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Instant,
};
use windows::{
    core::w,
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
            WindowsAndMessaging::*,
        },
    },
};

/// Posté au widget quand la palette se ferme (wparam = 1 : choix fait, 0 : perte du focus).
pub const WM_PALETTE_CLOSED: u32 = WM_APP + 3;

// Dimensions logiques (à 96 DPI), converties par s().
const MARGIN: i32 = 12;
const PLANE_W: i32 = 240;
const PLANE_H: i32 = 84;
const STRIP_H: i32 = 22;
const GAP: i32 = 10;

// Résolution du plan de couleurs avant étirement (x2).
const PW: usize = 120;
const PH: usize = 42;

const SEND_INTERVAL_MS: u64 = 90; // ~11 requêtes/s max pendant le glissement

#[derive(Clone)]
struct Cfg {
    id: String,
    can_color: bool,
    can_temp: bool,
    ct_min: u32,
    ct_max: u32,
    plane_top: i32, // logique
    strip_top: i32, // logique
}

static CFG: Mutex<Option<Cfg>> = Mutex::new(None);
static HWND_PAL: AtomicIsize = AtomicIsize::new(0);
static PARENT: AtomicIsize = AtomicIsize::new(0);
static PRESSED: AtomicBool = AtomicBool::new(false);
static ZONE: AtomicI32 = AtomicI32::new(0); // 0 : aucune, 1 : plan, 2 : bande
static BY_SELECTION: AtomicBool = AtomicBool::new(false);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
static REGISTERED: AtomicBool = AtomicBool::new(false);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn lo(l: LPARAM) -> i32 {
    (l.0 & 0xFFFF) as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    ((l.0 >> 16) & 0xFFFF) as i16 as i32
}

pub fn is_open() -> bool {
    HWND_PAL.load(Ordering::SeqCst) != 0
}

/// Ouvre la palette près du curseur pour la lumière `id`.
pub unsafe fn open(parent: HWND, id: &str, e: &Entity, at: POINT) {
    if !(e.can_color || e.can_temp) {
        return;
    }
    BY_SELECTION.store(true, Ordering::SeqCst); // fermeture volontaire d'une éventuelle palette précédente
    close();

    // Géométrie logique : plan (si couleur) puis bande (si blanc réglable).
    let mut ly = MARGIN;
    let plane_top = ly;
    if e.can_color {
        ly += PLANE_H + GAP;
    }
    let strip_top = ly;
    if e.can_temp {
        ly += STRIP_H + GAP;
    }
    let height = ly - GAP + MARGIN;

    *CFG.lock().unwrap() = Some(Cfg {
        id: id.to_string(),
        can_color: e.can_color,
        can_temp: e.can_temp,
        ct_min: e.ct_min as u32,
        ct_max: (e.ct_max as u32).max(e.ct_min as u32 + 1),
        plane_top,
        strip_top,
    });

    let Ok(module) = GetModuleHandleW(None) else { return };
    let hinst: HINSTANCE = module.into();
    let class = w!("HaTrayPalette");
    if !REGISTERED.swap(true, Ordering::SeqCst) {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(palette_proc),
            hInstance: hinst,
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }

    let style = WS_POPUP | WS_BORDER;
    let mut rc = RECT { left: 0, top: 0, right: s(MARGIN * 2 + PLANE_W), bottom: s(height) };
    let _ = AdjustWindowRect(&mut rc, style, false);
    let (ww, hh) = (rc.right - rc.left, rc.bottom - rc.top);

    // Placée au-dessus du curseur (sous lui si pas la place), bornée à la zone de travail.
    let mut wa = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut wa as *mut RECT as *mut c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );
    let mut x = at.x - ww / 2;
    let mut y = at.y - hh - s(10);
    if y < wa.top {
        y = at.y + s(10);
    }
    x = x.clamp(wa.left, (wa.right - ww).max(wa.left));
    y = y.clamp(wa.top, (wa.bottom - hh).max(wa.top));

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        class,
        w!(""),
        style,
        x,
        y,
        ww,
        hh,
        None,
        None,
        hinst,
        None,
    ) else {
        return;
    };

    // Enregistré AVANT l'affichage : le widget saura qu'une palette est ouverte
    // quand il perdra le focus (et ne se rétractera pas).
    HWND_PAL.store(hwnd.0 as isize, Ordering::SeqCst);
    PARENT.store(parent.0 as isize, Ordering::SeqCst);
    BY_SELECTION.store(false, Ordering::SeqCst);
    PRESSED.store(false, Ordering::SeqCst);
    ZONE.store(0, Ordering::SeqCst);

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

pub unsafe fn close() {
    let h = HWND_PAL.swap(0, Ordering::SeqCst);
    if h != 0 {
        let _ = DestroyWindow(HWND(h as *mut c_void));
    }
}

unsafe extern "system" fn palette_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),

        // Appui : on choisit tout de suite ; le glissement affine en direct (envoi limité).
        WM_LBUTTONDOWN => {
            PRESSED.store(true, Ordering::SeqCst);
            let _ = SetCapture(hwnd);
            pick(lo(lp), hi(lp), true, true);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if PRESSED.load(Ordering::SeqCst) {
                pick(lo(lp), hi(lp), false, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if PRESSED.swap(false, Ordering::SeqCst) {
                let _ = ReleaseCapture();
                pick(lo(lp), hi(lp), false, true); // valeur finale
                BY_SELECTION.store(true, Ordering::SeqCst);
                close();
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wp.0 == 0x1B {
                close(); // Échap
            }
            LRESULT(0)
        }
        // Clic ailleurs : la palette disparaît.
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) == 0 {
                close();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_DESTROY => {
            HWND_PAL.store(0, Ordering::SeqCst);
            PRESSED.store(false, Ordering::SeqCst);
            ZONE.store(0, Ordering::SeqCst);
            let parent = PARENT.load(Ordering::SeqCst);
            if parent != 0 {
                let chosen = BY_SELECTION.swap(false, Ordering::SeqCst);
                let _ = PostMessageW(HWND(parent as *mut c_void), WM_PALETTE_CLOSED, WPARAM(chosen as usize), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Convertit une position de souris en couleur / température et l'envoie à Home Assistant.
unsafe fn pick(x: i32, y: i32, start: bool, force: bool) {
    let Some(c) = CFG.lock().unwrap().clone() else { return };

    // La zone est fixée à l'appui : on peut sortir du cadre en glissant sans « sauter » de zone.
    if start {
        let zone = if c.can_color && y >= s(c.plane_top) && y < s(c.plane_top + PLANE_H) {
            1
        } else if c.can_temp && y >= s(c.strip_top) && y < s(c.strip_top + STRIP_H) {
            2
        } else {
            0
        };
        ZONE.store(zone, Ordering::SeqCst);
    }
    let zone = ZONE.load(Ordering::SeqCst);
    if zone == 0 {
        return;
    }

    let now = now_ms();
    if !force && now.saturating_sub(LAST_MS.load(Ordering::Relaxed)) < SEND_INTERVAL_MS {
        return;
    }
    LAST_MS.store(now, Ordering::Relaxed);

    let fx = (x - s(MARGIN)).clamp(0, s(PLANE_W)) as f64 / s(PLANE_W) as f64; // 0..1
    if zone == 1 {
        let fy = (y - s(c.plane_top)).clamp(0, s(PLANE_H)) as f64 / s(PLANE_H) as f64;
        ha::set_hs(&c.id, fx * 360.0, (1.0 - fy) * 100.0); // haut = saturé, bas = blanc
    } else {
        let k = c.ct_min as f64 + (c.ct_max - c.ct_min) as f64 * fx;
        ha::set_kelvin(&c.id, k.round() as u32);
    }
}

// ------------------------------------------------------------------ dessin

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let bg = CreateSolidBrush(COLORREF(0x0024_1E1E)); // rgb(30, 30, 36)
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(HGDIOBJ(bg.0));

    let cfg = CFG.lock().unwrap().clone();
    if let Some(c) = cfg {
        if c.can_color {
            // Plan teinte (horizontal) x saturation (vertical, saturé en haut, blanc en bas).
            let mut buf = vec![0u32; PW * PH];
            for yy in 0..PH {
                let sat = 1.0 - yy as f64 / (PH - 1) as f64;
                for xx in 0..PW {
                    let hue = xx as f64 * 360.0 / (PW - 1) as f64;
                    buf[yy * PW + xx] = pack(hsv(hue, sat, 1.0));
                }
            }
            blit(hdc, &buf, PW as i32, PH as i32, s(MARGIN), s(c.plane_top), s(PLANE_W), s(PLANE_H));
        }
        if c.can_temp {
            // Bande de blanc chaud (gauche) à blanc froid (droite), dans la plage de l'ampoule.
            let mut buf = vec![0u32; PW];
            for xx in 0..PW {
                let k = c.ct_min as f64 + (c.ct_max - c.ct_min) as f64 * xx as f64 / (PW - 1) as f64;
                buf[xx] = pack(kelvin_rgb(k));
            }
            blit(hdc, &buf, PW as i32, 1, s(MARGIN), s(c.strip_top), s(PLANE_W), s(STRIP_H));
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Affiche un tampon de pixels 0x00RRGGBB (haut vers le bas) étiré dans le rectangle donné.
#[allow(clippy::too_many_arguments)]
unsafe fn blit(hdc: HDC, buf: &[u32], bw: i32, bh: i32, x: i32, y: i32, w: i32, h: i32) {
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = bw;
    bmi.bmiHeader.biHeight = -bh; // négatif : origine en haut à gauche
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB
    StretchDIBits(
        hdc,
        x,
        y,
        w,
        h,
        0,
        0,
        bw,
        bh,
        Some(buf.as_ptr() as *const c_void),
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

fn pack((r, g, b): (u8, u8, u8)) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// h : 0..360, s et v : 0..1.
fn hsv(h: f64, s: f64, v: f64) -> (u8, u8, u8) {
    let c = v * s;
    let hp = (h / 60.0) % 6.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// Température de couleur (Kelvin) -> RGB approximatif (algorithme de Tanner Helland).
fn kelvin_rgb(k: f64) -> (u8, u8, u8) {
    let t = k.clamp(1000.0, 40000.0) / 100.0;
    let r = if t <= 66.0 { 255.0 } else { 329.698727446 * (t - 60.0).powf(-0.1332047592) };
    let g = if t <= 66.0 {
        99.4708025861 * t.ln() - 161.1195681661
    } else {
        288.1221695283 * (t - 60.0).powf(-0.0755148492)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.5177312231 * (t - 10.0).ln() - 305.0447927307
    };
    (r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8)
}
