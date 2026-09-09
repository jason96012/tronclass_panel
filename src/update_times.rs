//! UPDATE_TIMES_PAGE_V1：各項背景資料「最後更新時間」的集中登記簿。
//!
//! 目的：讓「設定 → 其他設定 → 資料更新時間」頁能一眼看到公告、作業、課表、
//! 晨間摘要、各排程器最後一次成功／失敗是什麼時候，不必翻 log。
//!
//! 設計：
//! * 純記憶體 `Mutex<Store>` + 落地到 `config/update_times.json`（原子寫入），
//!   重啟後仍看得到上一次的時間；`started_at` 永遠是「本次程序」的啟動時間。
//! * 每個項目分別保留「最後成功」與「最後失敗」——失敗不會蓋掉成功時間，
//!   前端可同時顯示「上次成功 12 分鐘前」與「最近一次失敗：…」。
//! * 落地節流：最多每 `PERSIST_MIN_GAP_SECS` 秒寫一次檔（上課前提醒每 60 秒記一筆，
//!   不必每筆都 fsync）；程序若在間隔內結束，最多遺失一分鐘內的紀錄，可接受。
//! * 呼叫端全是 fire-and-forget（同步、不回傳錯誤），永遠不影響主流程。
//!
//! 項目名稱（`kind`）由呼叫端自訂字串，前端以固定清單對應中文標籤；
//! 未知 kind 會以原字串顯示，不會壞頁。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// 全域項目（不分帳號）。
pub const KIND_BULLETIN_POLL: &str = "bulletin_poll";
pub const KIND_HOMEWORK_PREFETCH: &str = "homework_prefetch";
pub const KIND_HOMEWORK_REMINDER_CHECK: &str = "homework_reminder_check";
pub const KIND_TIMETABLE_SYNC_CHECK: &str = "timetable_sync_check";
pub const KIND_CLASS_REMINDER_CHECK: &str = "class_reminder_check";
/// 帳號項目。
pub const KIND_HOMEWORK: &str = "homework";
pub const KIND_BULLETINS: &str = "bulletins";
pub const KIND_MORNING_DIGEST: &str = "morning_digest";

const PERSIST_MIN_GAP_SECS: i64 = 60;
/// 帳號登記上限：帳號被刪除後其紀錄仍留在檔內，超過上限時丟掉最久沒更新的。
const MAX_ACCOUNT_ENTRIES: usize = 256;
const MAX_DETAIL_CHARS: usize = 200;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStamp {
    /// 最後一次成功的 unix 秒；0 = 從未成功。
    #[serde(default)]
    pub last_ok_at: i64,
    /// 成功時的簡短說明（例如「12 則」「3 帳號更新」）。
    #[serde(default)]
    pub last_ok_detail: String,
    /// 最後一次失敗的 unix 秒；0 = 從未失敗。
    #[serde(default)]
    pub last_error_at: i64,
    /// 最後一次失敗的訊息（已截斷）。
    #[serde(default)]
    pub last_error: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    global: HashMap<String, UpdateStamp>,
    #[serde(default)]
    accounts: HashMap<String, HashMap<String, UpdateStamp>>,
}

struct Registry {
    store: Store,
    loaded: bool,
    dirty: bool,
    last_persist_at: i64,
}

static REGISTRY: Lazy<Mutex<Registry>> = Lazy::new(|| {
    Mutex::new(Registry {
        store: Store::default(),
        loaded: false,
        dirty: false,
        last_persist_at: 0,
    })
});

/// 本次程序啟動時間（第一次被讀到時定格；`init()` 在 main 早期呼叫確保準確）。
static STARTED_AT: Lazy<i64> = Lazy::new(now_unix);

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn store_file() -> PathBuf {
    Path::new("config/update_times.json").to_path_buf()
}

/// 在 main 早期呼叫：定格啟動時間並把上次的檔案讀進記憶體。
pub fn init() {
    let _ = *STARTED_AT;
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    ensure_loaded(&mut reg);
}

pub fn started_at() -> i64 {
    *STARTED_AT
}

fn ensure_loaded(reg: &mut Registry) {
    if reg.loaded {
        return;
    }
    reg.loaded = true;
    if let Ok(text) = std::fs::read_to_string(store_file()) {
        if let Ok(parsed) = serde_json::from_str::<Store>(&text) {
            reg.store = parsed;
        }
    }
}

fn truncate_detail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
    out.push('…');
    out
}

fn apply(stamp: &mut UpdateStamp, ok: bool, detail: &str, now: i64) {
    if ok {
        stamp.last_ok_at = now;
        stamp.last_ok_detail = truncate_detail(detail);
    } else {
        stamp.last_error_at = now;
        stamp.last_error = truncate_detail(detail);
    }
}

fn persist_if_due(reg: &mut Registry, now: i64) {
    if !reg.dirty || now - reg.last_persist_at < PERSIST_MIN_GAP_SECS {
        return;
    }
    if let Ok(text) = serde_json::to_string(&reg.store) {
        match crate::atomic_write::write_atomic(store_file(), text) {
            Ok(()) => {
                reg.dirty = false;
                reg.last_persist_at = now;
            }
            Err(error) => {
                tracing::debug!(%error, "寫入 config/update_times.json 失敗（下次再試）");
                // 仍更新時間，避免磁碟持續出錯時每筆紀錄都嘗試寫檔。
                reg.last_persist_at = now;
            }
        }
    }
}

fn prune_accounts(store: &mut Store) {
    if store.accounts.len() <= MAX_ACCOUNT_ENTRIES {
        return;
    }
    let mut by_recent: Vec<(String, i64)> = store
        .accounts
        .iter()
        .map(|(id, kinds)| {
            let latest = kinds
                .values()
                .map(|s| s.last_ok_at.max(s.last_error_at))
                .max()
                .unwrap_or(0);
            (id.clone(), latest)
        })
        .collect();
    by_recent.sort_by_key(|(_, latest)| *latest);
    let overflow = store.accounts.len() - MAX_ACCOUNT_ENTRIES;
    for (id, _) in by_recent.into_iter().take(overflow) {
        store.accounts.remove(&id);
    }
}

/// 記錄全域（不分帳號）項目的一次結果。
pub fn record_global(kind: &str, ok: bool, detail: impl AsRef<str>) {
    let now = now_unix();
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    ensure_loaded(&mut reg);
    let stamp = reg.store.global.entry(kind.to_string()).or_default();
    apply(stamp, ok, detail.as_ref(), now);
    reg.dirty = true;
    persist_if_due(&mut reg, now);
}

/// 記錄某帳號某項目的一次結果。
pub fn record_account(kind: &str, account_id: &str, ok: bool, detail: impl AsRef<str>) {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return;
    }
    let now = now_unix();
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    ensure_loaded(&mut reg);
    let stamp = reg
        .store
        .accounts
        .entry(account_id.to_string())
        .or_default()
        .entry(kind.to_string())
        .or_default();
    apply(stamp, ok, detail.as_ref(), now);
    prune_accounts(&mut reg.store);
    reg.dirty = true;
    persist_if_due(&mut reg, now);
}

/// 目前所有紀錄的複本（供 API 組回應）。
pub fn snapshot() -> (
    HashMap<String, UpdateStamp>,
    HashMap<String, HashMap<String, UpdateStamp>>,
) {
    let mut reg = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    ensure_loaded(&mut reg);
    (reg.store.global.clone(), reg.store.accounts.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_does_not_overwrite_success() {
        let mut stamp = UpdateStamp::default();
        apply(&mut stamp, true, "12 則", 100);
        apply(&mut stamp, false, "逾時", 160);
        assert_eq!(stamp.last_ok_at, 100);
        assert_eq!(stamp.last_ok_detail, "12 則");
        assert_eq!(stamp.last_error_at, 160);
        assert_eq!(stamp.last_error, "逾時");
        apply(&mut stamp, true, "13 則", 200);
        assert_eq!(stamp.last_ok_at, 200);
        assert_eq!(stamp.last_error_at, 160, "失敗紀錄保留，由前端比較先後");
    }

    #[test]
    fn detail_is_truncated() {
        let long = "字".repeat(MAX_DETAIL_CHARS + 50);
        let out = truncate_detail(&long);
        assert_eq!(out.chars().count(), MAX_DETAIL_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn prune_keeps_most_recent_accounts() {
        let mut store = Store::default();
        for i in 0..(MAX_ACCOUNT_ENTRIES + 3) {
            let mut kinds = HashMap::new();
            kinds.insert(
                KIND_HOMEWORK.to_string(),
                UpdateStamp {
                    last_ok_at: i as i64,
                    ..Default::default()
                },
            );
            store.accounts.insert(format!("acc{i}"), kinds);
        }
        prune_accounts(&mut store);
        assert_eq!(store.accounts.len(), MAX_ACCOUNT_ENTRIES);
        assert!(!store.accounts.contains_key("acc0"));
        assert!(!store.accounts.contains_key("acc2"));
        assert!(store.accounts.contains_key(&format!("acc{}", MAX_ACCOUNT_ENTRIES + 2)));
    }
}
