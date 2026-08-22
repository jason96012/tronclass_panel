//! Nextcloud / ownCloud / 任意 WebDAV 教材後端。

use super::*;
use std::sync::RwLock;

use crate::config::MaterialsConfig;

#[derive(Debug, Clone)]
struct WebDavCfg {
    base: String,
    user: String,
    pass: String,
}

static WEBDAV: RwLock<Option<WebDavCfg>> = RwLock::new(None);
static FRONTEND_URL: RwLock<Option<String>> = RwLock::new(None);
static FRONTEND_MODE: RwLock<String> = RwLock::new(String::new());

pub fn configure(cfg: &MaterialsConfig) {
    let mut backend = cfg.backend.trim().to_ascii_lowercase();
    let mut url = cfg.webdav_url.trim().to_string();
    let mut user = cfg.webdav_username.trim().to_string();
    let mut pass = cfg.webdav_password.clone();
    if let Ok(v) = std::env::var("TRONCLASS_MATERIALS_BACKEND") {
        if !v.trim().is_empty() {
            backend = v.trim().to_ascii_lowercase();
        }
    }
    if let Ok(v) = std::env::var("TRONCLASS_WEBDAV_URL") {
        if !v.trim().is_empty() {
            url = v.trim().to_string();
        }
    }
    if let Ok(v) = std::env::var("TRONCLASS_WEBDAV_USERNAME") {
        if !v.trim().is_empty() {
            user = v.trim().to_string();
        }
    }
    if let Ok(v) = std::env::var("TRONCLASS_WEBDAV_PASSWORD") {
        pass = v;
    }
    let mut front = cfg.frontend_url.trim().to_string();
    if let Ok(v) = std::env::var("TRONCLASS_MATERIALS_FRONTEND_URL") {
        if !v.trim().is_empty() {
            front = v.trim().to_string();
        }
    }
    *FRONTEND_URL.write().unwrap() = if front.is_empty() {
        None
    } else {
        info!(url = %front, "教材前端連結：Nextcloud / ownCloud");
        Some(front.trim_end_matches('/').to_string())
    };
    let mut mode = cfg.frontend_mode.trim().to_ascii_lowercase();
    if let Ok(v) = std::env::var("TRONCLASS_MATERIALS_FRONTEND_MODE") {
        if !v.trim().is_empty() {
            mode = v.trim().to_ascii_lowercase();
        }
    }
    if mode.is_empty() {
        mode = "off".to_string();
    }
    *FRONTEND_MODE.write().unwrap() = mode;
    let enabled = backend == "webdav" && !url.is_empty() && !user.is_empty();
    *WEBDAV.write().unwrap() = if enabled {
        let base = url.trim_end_matches('/').to_string();
        info!(base = %base, user = %user, "教材後端：WebDAV / Nextcloud");
        Some(WebDavCfg {
            base,
            user,
            pass,
        })
    } else {
        info!("教材後端：本機 materials/");
        None
    };
}

pub fn enabled() -> bool {
    WEBDAV.read().unwrap().is_some()
}

pub fn frontend_url() -> Option<String> {
    FRONTEND_URL.read().unwrap().clone()
}

pub fn frontend_mode() -> String {
    FRONTEND_MODE.read().unwrap().clone()
}

fn cfg() -> Option<WebDavCfg> {
    WEBDAV.read().unwrap().clone()
}

fn encode_seg(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_rel(rel: &str) -> String {
    rel.split('/')
        .filter(|p| !p.is_empty())
        .map(encode_seg)
        .collect::<Vec<_>>()
        .join("/")
}

fn decode_pct(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn href_to_rel(base: &str, href: &str) -> String {
    let decoded = decode_pct(href).replace('\\', "/");
    let mut base_path = url::Url::parse(base)
        .ok()
        .map(|u| u.path().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if let Some(idx) = decoded.find(&base_path) {
        return decoded[idx + base_path.len()..]
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
    }
    if decoded.starts_with("http") {
        if let Ok(u) = url::Url::parse(&decoded) {
            let path = u.path().trim_end_matches('/');
            if path.starts_with(&base_path) {
                return path[base_path.len()..]
                    .trim_start_matches('/')
                    .to_string();
            }
            return path.trim_start_matches('/').to_string();
        }
    }
    let _ = &mut base_path;
    decoded.trim_start_matches('/').to_string()
}

fn tag_between(xml: &str, names: &[&str]) -> String {
    let lower = xml.to_ascii_lowercase();
    for name in names {
        let open = format!("<{name}");
        let close = format!("</{name}>");
        if let Some(start) = lower.find(&open) {
            if let Some(gt) = xml[start..].find('>') {
                let content_at = start + gt + 1;
                if let Some(end_rel) = lower[content_at..].find(&close) {
                    return xml[content_at..content_at + end_rel].trim().to_string();
                }
            }
        }
    }
    String::new()
}

fn is_collection(xml: &str) -> bool {
    let l = xml.to_ascii_lowercase();
    l.contains("<d:collection") || l.contains("<d:collection/>") || l.contains("<collection")
}

async fn client() -> Result<(reqwest::Client, WebDavCfg), String> {
    let cfg = cfg().ok_or_else(|| "WebDAV 未設定".to_string())?;
    let c = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    Ok((c, cfg))
}

fn auth(req: reqwest::RequestBuilder, cfg: &WebDavCfg) -> reqwest::RequestBuilder {
    req.basic_auth(&cfg.user, Some(&cfg.pass))
}

pub async fn list_tree() -> Result<(Vec<MaterialFileEntry>, Vec<String>), String> {
    let (client, cfg) = client().await?;
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop>
<d:resourcetype/><d:getcontentlength/><d:getlastmodified/><d:displayname/>
</d:prop></d:propfind>"#;
    let res = auth(client.request(Method::from_bytes(b"PROPFIND").unwrap(), &cfg.base), &cfg)
        .header("Depth", "infinity")
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("WebDAV PROPFIND 失敗：{e}"))?;
    if !res.status().is_success() && res.status().as_u16() != 207 {
        return Err(format!("WebDAV PROPFIND HTTP {}", res.status()));
    }
    let xml = res.text().await.map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    let mut folders = Vec::new();
    let chunks: Vec<&str> = xml
        .split_inclusive("</")
        .collect();
    // split by response tags case-insensitive
    let lower = xml.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<d:response") {
        let start = pos + rel;
        let end = lower[start..]
            .find("</d:response>")
            .map(|e| start + e + 13)
            .unwrap_or(xml.len());
        let block = &xml[start..end];
        pos = end;
        let href = tag_between(block, &["d:href", "href"]);
        if href.is_empty() {
            continue;
        }
        let rel_path = href_to_rel(&cfg.base, &href);
        if rel_path.is_empty() {
            continue;
        }
        if is_collection(block) {
            folders.push(rel_path);
            continue;
        }
        let name = rel_path
            .rsplit('/')
            .next()
            .unwrap_or(&rel_path)
            .to_string();
        let folder = rel_path
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        let size = tag_between(block, &["d:getcontentlength", "getcontentlength"])
            .parse::<u64>()
            .unwrap_or(0);
        let modified = tag_between(block, &["d:getlastmodified", "getlastmodified"]);
        let modified = if modified.is_empty() {
            "-".to_string()
        } else {
            modified
        };
        let extension = Path::new(&name)
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        files.push(MaterialFileEntry {
            path: rel_path,
            name,
            folder,
            size,
            size_label: format_file_size(size),
            modified,
            extension,
        });
        if files.len() + folders.len() >= MATERIALS_MAX_LIST_ENTRIES {
            break;
        }
    }
    let _ = chunks;
    folders.sort();
    folders.dedup();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((files, folders))
}

fn join_url(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        base.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), encode_rel(rel))
    }
}

pub async fn get_bytes(rel: &str) -> Result<(Vec<u8>, String), String> {
    let (client, cfg) = client().await?;
    let url = join_url(&cfg.base, rel);
    let res = auth(client.get(&url), &cfg)
        .send()
        .await
        .map_err(|e| format!("WebDAV GET 失敗：{e}"))?;
    if !res.status().is_success() {
        return Err(format!("WebDAV GET HTTP {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    Ok((bytes.to_vec(), url))
}

pub async fn put_bytes(rel: &str, data: &[u8]) -> Result<(), String> {
    let (client, cfg) = client().await?;
    let url = join_url(&cfg.base, rel);
    let res = auth(client.put(&url), &cfg)
        .header("Content-Type", "application/octet-stream")
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("WebDAV PUT 失敗：{e}"))?;
    if !res.status().is_success() && res.status().as_u16() != 201 && res.status().as_u16() != 204 {
        return Err(format!("WebDAV PUT HTTP {}", res.status()));
    }
    Ok(())
}

pub async fn delete_path(rel: &str) -> Result<(), String> {
    let (client, cfg) = client().await?;
    let url = join_url(&cfg.base, rel);
    let res = auth(client.delete(&url), &cfg)
        .send()
        .await
        .map_err(|e| format!("WebDAV DELETE 失敗：{e}"))?;
    if !res.status().is_success() && res.status().as_u16() != 204 {
        return Err(format!("WebDAV DELETE HTTP {}", res.status()));
    }
    Ok(())
}

pub async fn mkdir(rel: &str) -> Result<(), String> {
    let (client, cfg) = client().await?;
    // create parents
    let mut acc = String::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        let url = join_url(&cfg.base, &acc);
        let res = auth(
            client.request(Method::from_bytes(b"MKCOL").unwrap(), &url),
            &cfg,
        )
        .send()
        .await
        .map_err(|e| format!("WebDAV MKCOL 失敗：{e}"))?;
        let code = res.status().as_u16();
        if !res.status().is_success() && code != 201 && code != 405 && code != 409 && code != 301 {
            return Err(format!("WebDAV MKCOL HTTP {}", res.status()));
        }
    }
    Ok(())
}

pub async fn exists_file(rel: &str) -> Result<bool, String> {
    let (files, _) = list_tree().await?;
    Ok(files.iter().any(|f| f.path == rel))
}
