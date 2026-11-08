//! Configuration : %APPDATA%\HaTray\config.json  (SEUL emplacement utilisé par l'application).
//!
//! Aucune clé de registre n'est lue ni écrite. Le jeton est chiffré avec DPAPI
//! (lié au compte Windows courant) et stocké en hexadécimal dans "token_dpapi".
//! Désinstallation = supprimer ce dossier et l'exécutable.

use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::{
            CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        },
    },
};

#[derive(Serialize, Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    url: String,
    /// Jeton en clair : toléré en entrée (édition manuelle), jamais réécrit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_dpapi: Option<String>,
    #[serde(default)]
    entities: Vec<String>,
}

pub struct Loaded {
    pub url: String,
    pub token: String,
    pub entities: Vec<String>,
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
    base.join("HaTray").join("config.json")
}

/// Écriture atomique : fichier temporaire puis renommage (jamais de config à moitié écrite).
fn write_cfg(path: &PathBuf, cfg: &ConfigFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Charge la configuration. Renvoie `None` si le fichier est absent, vide, illisible,
/// sans URL ou sans jeton exploitable : l'appelant lance alors l'assistant de configuration.
pub fn load() -> Option<Loaded> {
    let path = config_path();
    let raw = fs::read_to_string(&path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let mut cfg: ConfigFile = serde_json::from_str(&raw).ok()?;
    if cfg.url.trim().is_empty() {
        return None;
    }

    // Jeton en clair saisi à la main : on le chiffre et on le retire du fichier.
    if let Some(plain) = cfg.token.take().filter(|t| !t.trim().is_empty()) {
        cfg.token_dpapi = Some(to_hex(&protect(plain.trim().as_bytes()).ok()?));
        let _ = write_cfg(&path, &cfg);
    }

    let blob = cfg.token_dpapi.as_deref()?;
    let token = String::from_utf8(unprotect(&from_hex(blob).ok()?).ok()?).ok()?;
    if token.is_empty() {
        return None;
    }

    Some(Loaded {
        url: cfg.url.trim().trim_end_matches('/').to_string(),
        token,
        entities: cfg.entities,
    })
}

/// Enregistre URL + jeton (chiffré). La liste blanche `entities` existante est conservée.
pub fn save(url: &str, token: &str) -> Result<(), String> {
    let path = config_path();
    let entities = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<ConfigFile>(&raw).ok())
        .map(|c| c.entities)
        .unwrap_or_default();

    fs::create_dir_all(path.parent().ok_or("chemin invalide")?).map_err(|e| e.to_string())?;
    let cfg = ConfigFile {
        url: url.trim().trim_end_matches('/').to_string(),
        token: None,
        token_dpapi: Some(to_hex(&protect(token.as_bytes())?)),
        entities,
    };
    write_cfg(&path, &cfg)
}

// ---------------------------------------------------------------- DPAPI

fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptProtectData(&input, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| e.to_string())?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut _));
        Ok(v)
    }
}

fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .map_err(|e| e.to_string())?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as *mut _));
        Ok(v)
    }
}

// ------------------------------------------------- hex minimal (évite une dépendance)

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return Err("token_dpapi corrompu".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "token_dpapi corrompu".to_string()))
        .collect()
}
