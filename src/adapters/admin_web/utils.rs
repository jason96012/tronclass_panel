//! Shared helper utilities for the Admin Web adapter.
//!
//! Kept separate from router/handler code so `admin_web/mod.rs` remains readable.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;

// ─── Extracted from admin_web/mod.rs ────────────────────────────────────────

pub(super) const MATERIALS_MAX_FILE_SIZE: usize = 50 * 1024 * 1024;
pub(super) const MATERIALS_MAX_LIST_ENTRIES: usize = 5000;

pub(super) const MATERIAL_ALLOWED_EXTENSIONS: &[&str] = &[
    "pdf", "docx", "pptx", "xlsx", "txt", "md", "jpg", "jpeg", "png", "zip", "mp4", "heic", "gif", "mov",
    // UPLOAD_LEGACY_OFFICE_V1: 允許舊版 Office 與 RTF/ODF，與預覽能力對齊；
    // 舊版 .doc/.ppt/.xls 預覽走 Office Online 線上檢視。
    "doc", "ppt", "xls", "rtf", "odt", "ods", "odp",
];

#[derive(Debug, Clone, Serialize)]
pub(super) struct MaterialFileEntry {
    path: String,
    name: String,
    folder: String,
    size: u64,
    size_label: String,
    modified: String,
    extension: String,
}

pub(super) fn materials_root() -> PathBuf {
    if let Ok(value) = std::env::var("TRONCLASS_MATERIALS_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(release_or_debug) = exe.parent() {
            if let Some(target_dir) = release_or_debug.parent() {
                if target_dir.file_name().and_then(|v| v.to_str()) == Some("target") {
                    if let Some(project_dir) = target_dir.parent() {
                        return project_dir.join("materials");
                    }
                }
            }
            return release_or_debug.join("materials");
        }
    }

    PathBuf::from("materials")
}

/// Keep generated previews outside the Filebrowser root. Preview PDFs can
/// contain sensitive content and must never become ordinary user-visible files.
pub(super) fn materials_preview_cache_root() -> PathBuf {
    if let Ok(value) = std::env::var("TRONCLASS_MATERIALS_CACHE_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from("run/material-preview-cache")
}

fn permission_denied(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message)
}

/// Reject symlinks in every existing component below the materials root.
///
/// Checking only the textual path is not enough: `safe_relative_path` blocks
/// `../`, but an attacker could otherwise upload into or download from a
/// directory symlink that points outside `cloud/`.
fn ensure_no_material_symlinks(root: &Path, rel: &Path) -> std::io::Result<()> {
    let mut current = root.to_path_buf();
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return Err(permission_denied("invalid materials path component"));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(permission_denied("symlinks are not allowed in materials paths"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Resolve an existing file or directory and prove that it remains below the
/// canonical materials root. The returned path contains no symlink component.
pub(super) fn secure_existing_material_path(
    root: &Path,
    rel: &Path,
) -> std::io::Result<PathBuf> {
    if rel.as_os_str().is_empty() {
        return Err(permission_denied("materials root is not an item"));
    }
    let canonical_root = root.canonicalize()?;
    ensure_no_material_symlinks(&canonical_root, rel)?;
    let candidate = canonical_root.join(rel);
    let meta = std::fs::symlink_metadata(&candidate)?;
    if meta.file_type().is_symlink() {
        return Err(permission_denied("symlinks are not allowed in materials paths"));
    }
    let canonical = candidate.canonicalize()?;
    if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
        return Err(permission_denied("materials path escapes its root"));
    }
    Ok(canonical)
}

/// Validate a path that may not exist yet, while ensuring every existing
/// parent is a real directory below the materials root. This is used before
/// `create_dir_all`, which would otherwise follow a directory symlink.
pub(super) fn secure_material_path_for_create(
    root: &Path,
    rel: &Path,
) -> std::io::Result<PathBuf> {
    let canonical_root = root.canonicalize()?;
    ensure_no_material_symlinks(&canonical_root, rel)?;
    let candidate = canonical_root.join(rel);
    let mut current = canonical_root.clone();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(permission_denied("invalid materials path component"));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(permission_denied("symlinks are not allowed in materials paths"));
            }
            Ok(meta) if components.peek().is_some() && !meta.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "materials path component is not a directory",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(candidate)
}

pub(super) fn safe_relative_path(value: &str) -> Option<PathBuf> {
    if value.len() > 1024 {
        return None;
    }
    let mut out = PathBuf::new();
    let normalized = value.replace('\\', "/");
    for part in normalized.split('/') {
        let part = part.trim();
        if part.is_empty() || part == "." {
            continue;
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(component)), None) => {
                let text = component.to_str()?;
                if text.contains('\0')
                    || text == ".."
                    || text.chars().any(|character| character.is_control())
                {
                    return None;
                }
                out.push(text);
            }
            _ => return None,
        }
    }
    Some(out)
}

pub(super) fn safe_file_name(value: &str) -> Option<String> {
    if value.len() > 255 {
        return None;
    }
    let file_name = Path::new(value).file_name()?.to_str()?.trim();
    if file_name.is_empty()
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains('\0')
        || file_name.chars().any(|character| character.is_control())
        || file_name.starts_with('.')
        || file_name == "."
        || file_name == ".."
    {
        None
    } else {
        Some(file_name.to_string())
    }
}

pub(super) fn is_allowed_material_file(file_name: &str) -> bool {
    Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            MATERIAL_ALLOWED_EXTENSIONS
                .iter()
                .any(|allowed| *allowed == ext)
        })
        .unwrap_or(false)
}

pub(super) fn unique_material_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(file_name);
    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|v| v.to_str()).unwrap_or("");
    let suffix = chrono::Local::now().format("%Y%m%d-%H%M%S");
    if ext.is_empty() {
        dir.join(format!("{stem}_{suffix}"))
    } else {
        dir.join(format!("{stem}_{suffix}.{ext}"))
    }
}

pub(super) fn list_material_files(root: &Path) -> std::io::Result<Vec<MaterialFileEntry>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    collect_material_files(root, root, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub(super) fn list_material_tree(
    root: &Path,
) -> std::io::Result<(Vec<MaterialFileEntry>, Vec<String>)> {
    let mut files = Vec::new();
    let mut folders = Vec::new();
    if !root.exists() {
        return Ok((files, folders));
    }
    collect_material_tree(root, root, &mut files, &mut folders)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    folders.sort();
    folders.dedup();
    Ok((files, folders))
}

pub(super) fn collect_material_files(
    root: &Path,
    dir: &Path,
    files: &mut Vec<MaterialFileEntry>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        // Do not follow symlinks while listing. A symlink can otherwise make
        // the dashboard disclose files outside the materials root.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_material_files(root, &path, files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let meta = entry.metadata()?;
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        let folder = rel
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let modified = meta
            .modified()
            .ok()
            .map(|time| {
                chrono::DateTime::<chrono::Local>::from(time)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        let extension = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        files.push(MaterialFileEntry {
            path: rel_path,
            name,
            folder,
            size: meta.len(),
            size_label: format_file_size(meta.len()),
            modified,
            extension,
        });
    }
    Ok(())
}

fn collect_material_tree(
    root: &Path,
    dir: &Path,
    files: &mut Vec<MaterialFileEntry>,
    folders: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        if files.len().saturating_add(folders.len()) >= MATERIALS_MAX_LIST_ENTRIES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("materials tree exceeds {MATERIALS_MAX_LIST_ENTRIES} files and folders"),
            ));
        }
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        // Never follow links while building the visible tree.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_path = rel.to_string_lossy().replace('\\', "/");
            if !rel_path.trim().is_empty() {
                folders.push(rel_path);
            }
            collect_material_tree(root, &path, files, folders)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let meta = entry.metadata()?;
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        let folder = rel
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let modified = meta
            .modified()
            .ok()
            .map(|time| {
                chrono::DateTime::<chrono::Local>::from(time)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "-".to_string());
        let extension = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        files.push(MaterialFileEntry {
            path: rel_path,
            name,
            folder,
            size: meta.len(),
            size_label: format_file_size(meta.len()),
            modified,
            extension,
        });
    }
    Ok(())
}

pub(super) fn list_material_folders(root: &Path) -> std::io::Result<Vec<String>> {
    let mut folders = Vec::new();
    if !root.exists() {
        return Ok(folders);
    }
    collect_material_folders(root, root, &mut folders)?;
    folders.sort();
    folders.dedup();
    Ok(folders)
}

pub(super) fn collect_material_folders(
    root: &Path,
    dir: &Path,
    folders: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_path = rel.to_string_lossy().replace('\\', "/");
        if !rel_path.trim().is_empty() {
            folders.push(rel_path);
        }
        collect_material_folders(root, &path, folders)?;
    }
    Ok(())
}

pub(super) fn format_file_size(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / KB)
    } else {
        format!("{:.1} MB", size as f64 / MB)
    }
}

pub(super) fn material_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        // ODF 家族（odt/ods/odp）與 RTF：前端 office-view.js 本機解析；瀏覽器本身不需要認得。
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "rtf" => "application/rtf",
        // 舊二進位 Office：只供下載／前端顯示提示。
        "doc" => "application/msword",
        "ppt" => "application/vnd.ms-powerpoint",
        "xls" => "application/vnd.ms-excel",
        // URL_SHORTCUT_V1: .url（Internet Shortcut）為純文字，前端讀取後跳轉。
        "url" => "text/x-uri; charset=utf-8",
        "txt" | "text" | "log" => "text/plain; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        // PREVIEW_TEXT_PLAIN_V1
        // HTML / XML 刻意回 text/plain：預覽端點是同源且帶 session cookie 的，
        // 若以 text/html 或 application/xml 串流，使用者上傳的檔案就能在同一個
        // origin 上被當成文件執行（stored XSS）。這裡只把它們當「原始碼」給前端
        // 以 <pre> + esc() 顯示。CSP 之外多一道防線。
        "html" | "htm" | "xml" | "xhtml" => "text/plain; charset=utf-8",
        "csv" | "tsv" => "text/csv; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "yaml" | "yml" | "ini" | "conf" | "cfg" | "toml" | "sql" | "sh" | "rs"
        | "js" | "ts" | "jsx" | "tsx" | "css" | "py" | "java" | "c" | "h" | "cpp"
        | "hpp" | "go" | "php" => "text/plain; charset=utf-8",
        "srt" => "application/x-subrip; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        // PREVIEW_IMAGE_TYPES_V1: 原本這些全部落到 application/octet-stream，
        // 導致 <img>／<audio>／<video> 無法渲染（gif/bmp/webp 的預覽按鈕按下去
        // 只會拿到 400 或空白）。
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        // SVG 只允許被 <img> 載入（img 上下文不會執行 script）；前端不提供
        // 把它當文件渲染的途徑。
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}

pub(super) fn sanitize_header_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' => '_',
            _ => ch,
        })
        .collect()
}

pub(super) async fn run_health_command(program: &str, args: &[&str]) -> String {
    match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(program).args(args).output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if output.status.success() {
                if stdout.is_empty() {
                    "ok".to_string()
                } else {
                    stdout
                }
            } else if stderr.is_empty() {
                format!("exit {}: {}", output.status, stdout)
            } else {
                format!("exit {}: {}", output.status, stderr)
            }
        }
        Ok(Err(e)) => format!("command failed: {e}"),
        Err(_) => "command timeout".to_string(),
    }
}

pub(super) fn read_proc_uptime() -> serde_json::Value {
    let raw = std::fs::read_to_string("/proc/uptime").unwrap_or_default();
    let secs = raw
        .split_whitespace()
        .next()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0) as u64;
    json!({
        "seconds": secs,
        "label": format_duration_label(secs),
    })
}

pub(super) fn read_meminfo_summary() -> serde_json::Value {
    let raw = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get_kb = |name: &str| -> u64 {
        raw.lines()
            .find_map(|line| {
                let (key, rest) = line.split_once(':')?;
                if key != name {
                    return None;
                }
                rest.split_whitespace().next()?.parse::<u64>().ok()
            })
            .unwrap_or(0)
    };
    let mem_total = get_kb("MemTotal") * 1024;
    let mem_avail = get_kb("MemAvailable") * 1024;
    let swap_total = get_kb("SwapTotal") * 1024;
    let swap_free = get_kb("SwapFree") * 1024;
    json!({
        "mem_total": mem_total,
        "mem_available": mem_avail,
        "mem_used": mem_total.saturating_sub(mem_avail),
        "mem_total_label": format_bytes_label(mem_total),
        "mem_available_label": format_bytes_label(mem_avail),
        "mem_used_label": format_bytes_label(mem_total.saturating_sub(mem_avail)),
        "swap_total": swap_total,
        "swap_free": swap_free,
        "swap_used": swap_total.saturating_sub(swap_free),
        "swap_total_label": format_bytes_label(swap_total),
        "swap_free_label": format_bytes_label(swap_free),
        "swap_used_label": format_bytes_label(swap_total.saturating_sub(swap_free)),
    })
}

pub(super) fn format_duration_label(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小時 {mins} 分")
    } else if hours > 0 {
        format!("{hours} 小時 {mins} 分")
    } else {
        format!("{mins} 分")
    }
}

pub(super) fn format_bytes_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / GB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Total storage limit for all teaching materials: 10GB.
pub(super) const MATERIALS_MAX_TOTAL_SIZE: usize = 10 * 1024 * 1024 * 1024;

pub(super) fn material_total_size(root: &Path) -> std::io::Result<u64> {
    fn walk(path: &Path, total: &mut u64) -> std::io::Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                walk(&entry.path(), total)?;
            } else if file_type.is_file() {
                *total = total.saturating_add(entry.metadata()?.len());
            }
        }
        Ok(())
    }
    let mut total = 0u64;
    walk(root, &mut total)?;
    Ok(total)
}
