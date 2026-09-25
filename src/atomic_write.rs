//! ATOMIC_WRITE_V1：小型設定／狀態檔的原子寫入。
//!
//! 專案裡多個 `config/*.json`（ai_chat、digest、bulletin_reads、share_links…）
//! 原本直接 `std::fs::write` 覆蓋。`fs::write` 是「truncate → write」兩步：
//! 若在中間被 OOM-kill、斷電或磁碟滿，檔案會留下 0 byte 或半截 JSON，
//! 下次啟動 `serde_json::from_str` 失敗 → 靜默退回預設值（AI 密鑰、分享連結、
//! 已讀狀態全部消失）。
//!
//! 這裡統一改成 **write-to-temp → fsync → rename**：rename 在同一檔案系統上
//! 是原子操作，任何時間點檔案不是完整的舊版就是完整的新版。
//!
//! 使用規則：
//! - temp 檔一律建在目標檔同目錄（跨檔案系統 rename 不是原子）。
//! - 失敗時盡力清掉 temp 檔，不留垃圾。
//! - 需要限制權限的檔（密鑰）用 [`write_atomic_with_mode`]：權限在 rename
//!   **之前** 就套在 temp 檔上，不會出現「已可讀但還沒 chmod」的空窗。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 同一程序內的遞增序號，搭配 pid 讓 temp 檔名不會撞到（多執行緒同時寫同一檔）。
static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path_for(target: &Path) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(".{file_name}.{}.{seq}.tmp", std::process::id());
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

/// 以原子方式把 `contents` 寫到 `path`（會自動建立父目錄）。
pub(crate) fn write_atomic(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    write_atomic_inner(path.as_ref(), contents.as_ref(), None)
}

/// 同 [`write_atomic`]，但在 rename 前先把檔案權限設為 `mode`（僅 Unix 生效；
/// 其他平台忽略 `mode`）。用於含密鑰的檔案，例如 `0o600`。
pub(crate) fn write_atomic_with_mode(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
    mode: u32,
) -> io::Result<()> {
    write_atomic_inner(path.as_ref(), contents.as_ref(), Some(mode))
}

fn write_atomic_inner(target: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = temp_path_for(target);
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        file.write_all(contents)?;
        // 內容先落盤，rename 之後才不會出現「檔名已換、內容還在 page cache」的狀況。
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)?;
        // 目錄項目也同步一次（best-effort；某些檔案系統／平台不支援 fsync 目錄）。
        #[cfg(unix)]
        if let Some(parent) = target.parent() {
            let dir = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            if let Ok(handle) = fs::File::open(dir) {
                let _ = handle.sync_all();
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");
        write_atomic(&path, b"{\"v\":1}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"v\":1}");
        write_atomic(&path, b"{\"v\":2}").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"v\":2}");
        // 沒有殘留 temp 檔
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp 檔應在 rename 後消失");
    }

    #[cfg(unix)]
    #[test]
    fn mode_is_applied_before_rename() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic_with_mode(&path, b"s3cret", 0o600).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn temp_names_are_unique() {
        let a = temp_path_for(Path::new("config/x.json"));
        let b = temp_path_for(Path::new("config/x.json"));
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(Path::new("config")));
    }
}
