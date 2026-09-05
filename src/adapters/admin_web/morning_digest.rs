//! MORNING_DIGEST_V1：晨間摘要推送。
//!
//! 每天（預設 08:00，可在設定頁調整）為每個啟用帳號推送一則 Web Push：
//! 今日課表＋48 小時內截止的未繳作業＋最新公告。資料全部取自既有
//! 快取／DB（timetable_entries、homework_snapshot、公告輪詢的最新結果），
//! **不對學校端發任何額外請求**。
//!
//! 設定持久化於 `config/digest.json`（與 bulletin_notified.json 同目錄），
//! 重啟不流失；時間以伺服器本地時間為準。發送窗＝目標時間起 30 分鐘內
//! （多帳號逐一手動別送、每帳號間隔 2 秒），每帳號每天至多一則
//! （以日期守門，重啟不會重複）。

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Timelike, Utc, Weekday};
use once_cell::sync::Lazy;
use tokio::sync::RwLock;

use super::AdminWebState;

/// 發送窗寬度（分鐘）：目標時間後這麼久內都允許補發（涵蓋重啟／忙碌延遲）。
const FIRE_WINDOW_MINUTES: u32 = 30;
/// 每帳號發送間隔（秒），平滑 Web Push 流量。
const DIGEST_ACCOUNT_GAP_SECS: u64 = 2;
/// 排程器 tick（秒）。
const TICK_SECS: u64 = 30;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct DigestSettings {
    pub enabled: bool,
    /// HH:MM（伺服器本地時間）
    pub time: String,
}

impl Default for DigestSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            time: "08:00".to_string(),
        }
    }
}

fn digest_file() -> PathBuf {
    std::path::Path::new("config/digest.json").to_path_buf()
}

static DIGEST: Lazy<RwLock<DigestSettings>> = Lazy::new(|| {
    // 啟動時載入持久化設定；缺檔／格式錯誤回預設（08:00、啟用）。
    let loaded = std::fs::read_to_string(digest_file())
        .ok()
        .and_then(|text| serde_json::from_str::<DigestSettings>(&text).ok())
        .filter(|s| crate::homework_reminders::parse_hh_mm(&s.time).is_some())
        .unwrap_or_default();
    RwLock::new(loaded)
});

/// /admin/api/status 用的摘要設定快照。
#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct DigestStatusInfo {
    pub enabled: bool,
    pub time: String,
}

pub(super) fn digest_status() -> DigestStatusInfo {
    let settings = digest_settings();
    DigestStatusInfo {
        enabled: settings.enabled,
        time: settings.time,
    }
}

/// 目前生效的摘要設定（同步讀取）。
pub(super) fn digest_settings() -> DigestSettings {
    DIGEST
        .try_read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// 更新設定（驗證 HH:MM）→ 寫檔 → 更新記憶體。回傳正規化後的時間。
pub(super) async fn set_digest_settings(enabled: bool, time: String) -> miette::Result<String> {
    let time = time.trim().to_string();
    let normalized = crate::homework_reminders::parse_hh_mm(&time)
        .ok_or_else(|| miette::miette!("時間格式錯誤：{time:?}（應為 HH:MM，例 08:00）"))?;
    let (h, m) = (normalized / 60, normalized % 60);
    let normalized_text = format!("{h:02}:{m:02}");
    let settings = DigestSettings {
        enabled,
        time: normalized_text.clone(),
    };
    let text = serde_json::to_string_pretty(&settings)
        .map_err(|e| miette::miette!("序列化摘要設定失敗：{e}"))?;
    std::fs::write(digest_file(), text)
        .map_err(|e| miette::miette!("寫入 config/digest.json 失敗：{e}"))?;
    *DIGEST.write().await = settings;
    Ok(normalized_text)
}

/// 啟動晨間摘要排程器。
pub(super) fn start(state: AdminWebState) {
    tokio::spawn(async move {
        let settings = digest_settings();
        tracing::info!(
            enabled = settings.enabled,
            time = %settings.time,
            "晨間摘要排程器已啟動"
        );
        // 每帳號「今天已送」守門（記憶體即可：重啟後日期守門仍防重複，
        // 唯一風險是重啟跨目標分鐘時多送一輪——發送窗 30 分鐘內才會發生，
        // 且 Web Push tag 以日期為後綴，裝置端會覆蓋而非洗版）。
        let mut last_sent: HashMap<String, NaiveDate> = HashMap::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(TICK_SECS)).await;
            let settings = digest_settings();
            if !settings.enabled {
                continue;
            }
            let Some(target) = crate::homework_reminders::parse_hh_mm(&settings.time) else {
                continue;
            };
            let now = Local::now();
            let now_min = now.hour() * 60 + now.minute();
            if !in_fire_window(target, now_min) {
                continue;
            }
            let today = now.date_naive();
            let accounts = match state.accounts_db.list().await {
                Ok(list) => list,
                Err(_) => continue,
            };
            for raw in accounts {
                if !raw.enabled || last_sent.get(&raw.id) == Some(&today) {
                    continue;
                }
                match build_and_send(&state, &raw.id, &raw.username).await {
                    Ok(delivered) => {
                        if delivered > 0 {
                            tracing::info!(
                                account = %raw.id,
                                devices = delivered,
                                "晨間摘要已推送"
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(account = %raw.id, %error, "晨間摘要推送失敗");
                    }
                }
                last_sent.insert(raw.id.clone(), today);
                tokio::time::sleep(std::time::Duration::from_secs(DIGEST_ACCOUNT_GAP_SECS)).await;
            }
        }
    });
}

/// now 是否落在目標時間的發送窗內（target ≤ now < target+30）。
fn in_fire_window(target_min: u32, now_min: u32) -> bool {
    now_min >= target_min && now_min < target_min.saturating_add(FIRE_WINDOW_MINUTES)
}

/// chrono Weekday → timetable_entries 的 weekday 鍵（mon..sun）。
fn weekday_key(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "mon",
        Weekday::Tue => "tue",
        Weekday::Wed => "wed",
        Weekday::Thu => "thu",
        Weekday::Fri => "fri",
        Weekday::Sat => "sat",
        Weekday::Sun => "sun",
    }
}

/// 組裝並推送單一帳號的摘要。回傳成功送達的裝置數。
async fn build_and_send(state: &AdminWebState, account_id: &str, username: &str) -> miette::Result<usize> {
    let now_local = Local::now();
    let today_key = weekday_key(now_local.weekday());

    // 一、今日課表（DB timetable_entries；不發上游請求）
    let mut class_lines: Vec<String> = Vec::new();
    if let Ok(rows) = state.accounts_db.list_timetable_entries_for_account(account_id).await {
        let mut today: Vec<_> = rows
            .into_iter()
            .filter(|row| row.weekday == today_key)
            .collect();
        today.sort_by(|a, b| a.start_time.cmp(&b.start_time));
        for row in today.iter().take(8) {
            let place = if row.classroom.trim().is_empty() {
                String::new()
            } else {
                format!(" @{}", row.classroom.trim())
            };
            class_lines.push(format!("{} {}{}", row.start_time, row.course_name, place));
        }
        if today.len() > 8 {
            class_lines.push(format!("…另有 {} 節", today.len() - 8));
        }
    }

    // 二、48 小時內截止的未繳作業（homework_snapshot）
    let cutoff = Utc::now() + ChronoDuration::hours(48);
    let mut homework_lines: Vec<String> = Vec::new();
    if let Ok(Some((items_json, _))) = state.accounts_db.get_homework_snapshot(account_id).await {
        if let Ok(items) = serde_json::from_str::<Vec<crate::api::todos::TronclassTodoItem>>(&items_json) {
            let mut due: Vec<_> = items
                .into_iter()
                .filter(|item| {
                    !item.is_submitted
                        && item
                            .end_time
                            .map(|end| end > Utc::now() && end <= cutoff)
                            .unwrap_or(false)
                })
                .collect();
            due.sort_by_key(|item| item.end_time);
            for item in due.iter().take(5) {
                let end = item.end_time.expect("filtered above");
                let when = {
                    let local_end = end.with_timezone(&Local);
                    let hours = (end - Utc::now()).num_hours();
                    if hours < 24 {
                        format!("{} 小時後截止", hours.max(1))
                    } else {
                        format!("{} 截止", local_end.format("%m/%d %H:%M"))
                    }
                };
                let course = item.course_name.as_deref().unwrap_or("課程");
                homework_lines.push(format!("{course}：{}（{when}）", item.title));
            }
            if due.len() > 5 {
                homework_lines.push(format!("…另有 {} 項", due.len() - 5));
            }
        }
    }

    // 三、最新公告（公告輪詢的最新一輪結果；不發上游請求）
    let bulletin_lines = super::bulletins::latest_bulletin_titles(account_id, 3);

    let body = render_digest_body(&class_lines, &homework_lines, &bulletin_lines);
    let title = format!("🌅 晨間摘要｜{username}");
    crate::web_push_notifications::send_digest(account_id, &title, &body).await
}

/// 组装三段落文字（獨立函數以便單元測試）。
fn render_digest_body(class_lines: &[String], homework_lines: &[String], bulletin_lines: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    sections.push(if class_lines.is_empty() {
        "📅 今日沒課 🎉".to_string()
    } else {
        format!("📅 今日課表（{} 節）\n{}", class_lines.len().min(8), class_lines.join("\n"))
    });
    sections.push(if homework_lines.is_empty() {
        "📌 48 小時內沒有截止作業".to_string()
    } else {
        format!("📌 48 小時內截止\n{}", homework_lines.join("\n"))
    });
    if !bulletin_lines.is_empty() {
        sections.push(format!("📢 最新公告\n{}", bulletin_lines.join("\n")));
    }
    let body = sections.join("\n\n");
    // Web Push body 過長會被平臺截斷，主動控制在上限內（char 邊界安全截斷）。
    if body.chars().count() > 900 {
        let truncated: String = body.chars().take(897).collect();
        format!("{truncated}…")
    } else {
        body
    }
}

/// MORNING_DIGEST_V1：立即為指定帳號組裝並推送一則摘要（設定頁「發送測試」）。
pub(super) async fn send_test_now(
    state: &AdminWebState,
    account_id: &str,
) -> miette::Result<usize> {
    let username = state
        .accounts_db
        .get(account_id)
        .await
        .ok()
        .flatten()
        .map(|account| account.username.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| account_id.to_string());
    build_and_send(state, account_id, &username).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_window_semantics() {
        let target = 8 * 60;
        assert!(!in_fire_window(target, target - 1), "目標前不發");
        assert!(in_fire_window(target, target), "整點即發");
        assert!(in_fire_window(target, target + 29), "窗內補發");
        assert!(!in_fire_window(target, target + 30), "窗關閉");
        // 深夜設定（如 23:30）不會在早上誤發
        let night = 23 * 60 + 30;
        assert!(!in_fire_window(night, 8 * 60));
    }

    #[test]
    fn weekday_key_mapping() {
        assert_eq!(weekday_key(Weekday::Mon), "mon");
        assert_eq!(weekday_key(Weekday::Sun), "sun");
    }

    #[test]
    fn render_body_sections() {
        let empty = render_digest_body(&[], &[], &[]);
        assert!(empty.contains("今日沒課"));
        assert!(empty.contains("沒有截止作業"));
        assert!(!empty.contains("最新公告"), "無公告不出該段");

        let full = render_digest_body(
            &["08:10 微積分 @B101".to_string()],
            &["線性代數：作業三（5 小時後截止）".to_string()],
            &["停課通知".to_string()],
        );
        assert!(full.contains("今日課表（1 節）"));
        assert!(full.contains("線性代數"));
        assert!(full.contains("最新公告"));

        // 超長截斷（char 邊界）
        let long: Vec<String> = (0..200).map(|i| format!("課程{i}xxxxxxxxxxxxxxxxxxxx")).collect();
        let body = render_digest_body(&long, &[], &[]);
        assert!(body.chars().count() <= 900);
        assert!(body.ends_with('…'));
    }
}
