//! 教材前端設定（環境變數）。
//! 不依賴 `config.rs` 是否有 `MaterialsConfig`，Pterodactyl 舊版也能編譯。
//! Nextcloud WebDAV 已停用：檔案走本機 `cloud/` + Filebrowser。

use std::sync::RwLock;

static FRONTEND_URL: RwLock<Option<String>> = RwLock::new(None);
static FRONTEND_MODE: RwLock<String> = RwLock::new(String::new());
static LOADED: RwLock<bool> = RwLock::new(false);

fn load_from_env() {
    if *LOADED.read().unwrap() {
        return;
    }
    let front = std::env::var("TRONCLASS_MATERIALS_FRONTEND_URL")
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    *FRONTEND_URL.write().unwrap() = if front.is_empty() {
        None
    } else {
        Some(front)
    };
    let mut mode = std::env::var("TRONCLASS_MATERIALS_FRONTEND_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mode.is_empty() {
        mode = "off".to_string();
    }
    *FRONTEND_MODE.write().unwrap() = mode;
    *LOADED.write().unwrap() = true;
}

/// 舊版 main 不會呼叫；新版若傳 `&config.materials` 會對不上型別，請改呼叫無參數版。
pub fn configure() {
    *LOADED.write().unwrap() = false;
    load_from_env();
}

pub fn enabled() -> bool {
    false
}

pub fn frontend_url() -> Option<String> {
    load_from_env();
    FRONTEND_URL.read().unwrap().clone()
}

pub fn frontend_mode() -> String {
    load_from_env();
    FRONTEND_MODE.read().unwrap().clone()
}

pub async fn list_tree() -> Result<(Vec<super::utils::MaterialFileEntry>, Vec<String>), String> {
    Err("WebDAV 已停用，改用本機／Filebrowser".into())
}

pub async fn get_bytes(_rel: &str) -> Result<(Vec<u8>, String), String> {
    Err("WebDAV 已停用".into())
}

pub async fn put_bytes(_rel: &str, _data: &[u8]) -> Result<(), String> {
    Err("WebDAV 已停用".into())
}

pub async fn delete_path(_rel: &str) -> Result<(), String> {
    Err("WebDAV 已停用".into())
}

pub async fn mkdir(_rel: &str) -> Result<(), String> {
    Err("WebDAV 已停用".into())
}

pub async fn exists_file(_rel: &str) -> Result<bool, String> {
    Ok(false)
}
