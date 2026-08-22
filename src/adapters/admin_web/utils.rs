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
    "pdf", "docx", "pptx", "xlsx", "txt", "md", "jpg", "jpeg", "png", "zip", "mp4",
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
                if text.contains('\0') || text == ".." {
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
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            collect_material_files(root, &path, files)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
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
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_path = rel.to_string_lossy().replace('\\', "/");
            if !rel_path.trim().is_empty() {
                folders.push(rel_path);
            }
            collect_material_tree(root, &path, files, folders)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
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
        let path = entry.path();
        let meta = entry.metadata()?;
        if !meta.is_dir() {
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
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
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
            let meta = entry.metadata()?;
            if meta.is_dir() {
                walk(&entry.path(), total)?;
            } else if meta.is_file() {
                *total = total.saturating_add(meta.len());
            }
        }
        Ok(())
    }
    let mut total = 0u64;
    walk(root, &mut total)?;
    Ok(total)
}
