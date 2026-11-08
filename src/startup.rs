//! Lancement au démarrage de Windows SANS registre.
//!
//! On crée (ou supprime) un simple raccourci « HaTray.lnk » dans le dossier Démarrage
//! de l'utilisateur : %APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup
//! (le même que `shell:startup`). Aucune clé « Run » n'est touchée.
//!
//! Le raccourci pointe vers l'exécutable actuellement lancé : si vous déplacez
//! ha-tray.exe, désactivez puis réactivez l'option.
//! L'installateur (installer\HaTray.iss) utilise exactement le même nom de raccourci.

use std::{
    fs,
    io::ErrorKind,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use windows::{
    core::{Interface, PCWSTR},
    Win32::{
        Foundation::BOOL,
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        },
        UI::Shell::{IShellLinkW, ShellLink},
    },
};

fn shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
            .join("HaTray.lnk"),
    )
}

/// Le lancement au démarrage est actif si le raccourci existe.
pub fn is_enabled() -> bool {
    shortcut_path().map(|p| p.exists()).unwrap_or(false)
}

/// Active ou désactive le lancement au démarrage.
pub fn set(enable: bool) -> Result<(), String> {
    let lnk = shortcut_path().ok_or("dossier Démarrage introuvable")?;

    if !enable {
        return match fs::remove_file(&lnk) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // COM est initialisé puis libéré dans un thread éphémère : le thread d'interface
    // reste exempt de COM, et rien n'est conservé en mémoire ensuite.
    std::thread::spawn(move || unsafe { create_link(&exe, &lnk) })
        .join()
        .map_err(|_| "échec interne".to_string())?
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn create_link(exe: &Path, lnk: &Path) -> Result<(), String> {
    let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let result = (|| -> windows::core::Result<()> {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;

        let exe_w = wide_path(exe);
        link.SetPath(PCWSTR(exe_w.as_ptr()))?;

        if let Some(dir) = exe.parent() {
            let dir_w = wide_path(dir);
            link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()))?;
        }
        let desc_w = wide_str("HaTray - Home Assistant");
        link.SetDescription(PCWSTR(desc_w.as_ptr()))?;

        let file: IPersistFile = link.cast()?;
        let lnk_w = wide_path(lnk);
        file.Save(PCWSTR(lnk_w.as_ptr()), BOOL(1))
    })();

    if init.is_ok() {
        CoUninitialize();
    }
    result.map_err(|e| e.to_string())
}
