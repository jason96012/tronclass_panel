//! MONITOR_ALERT_GRADING_V1：監控錯誤分級與管理員警示。
//!
//! 監控迴圈每次輪詢失敗都會呼叫 [`classify`] 取得等級：
//!
//! | 等級 | 條件 | 行為 |
//! |------|------|------|
//! | 輕微 (Transient) | 連續失敗 < 3 次且非認證錯誤 | 只寫 log，不打擾任何人 |
//! | 警告 (Warning)   | 連續失敗 ≥ 3 次，或偵測到認證錯誤 | 通知所有管理員（Discord DM + 手機推播）一次 |
//! | 嚴重 (Critical)  | 自動重新認證失敗，或連續失敗 ≥ 2×max_failures | 立刻通知管理員；持續未恢復時每 30 分鐘再提醒 |
//!
//! 曾送出警示的帳號恢復成功輪詢時，會再送一則「已恢復」訊息，並附上中斷時長。
//! 帳號擁有者本身的重新認證訊息維持原本流程（monitor.rs），這裡只負責管理員。
//! 所有文字不含表情符號。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tracing::{debug, info, warn};

use crate::adapters::events::{AdapterMessenger, OutboundMessage};
use crate::db::AccountsDb;

/// 連續失敗達到這個次數就升為「警告」。
pub const WARNING_AFTER_FAILURES: u32 = 3;
/// 「嚴重」等級持續未恢復時的重複提醒間隔。
const CRITICAL_REPEAT_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// 錯誤訊息在通知中最多保留的字元數。
const ERROR_SNIPPET_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorGrade {
    Transient,
    Warning,
    Critical,
}

impl ErrorGrade {
    pub fn label(self) -> &'static str {
        match self {
            ErrorGrade::Transient => "輕微",
            ErrorGrade::Warning => "警告",
            ErrorGrade::Critical => "嚴重",
        }
    }
}

/// 依錯誤性質與連續失敗次數決定等級。
pub fn classify(
    is_auth_error: bool,
    consecutive_failures: u32,
    max_failures_before_reauth: u32,
    reauth_failed: bool,
) -> ErrorGrade {
    if reauth_failed {
        return ErrorGrade::Critical;
    }
    let critical_after = max_failures_before_reauth
        .max(WARNING_AFTER_FAILURES)
        .saturating_mul(2);
    if consecutive_failures >= critical_after {
        return ErrorGrade::Critical;
    }
    if is_auth_error || consecutive_failures >= WARNING_AFTER_FAILURES {
        return ErrorGrade::Warning;
    }
    ErrorGrade::Transient
}

/// 把錯誤字串粗略歸類成人看得懂的原因，附在通知裡。
pub fn describe_cause(error_text: &str, is_auth_error: bool) -> &'static str {
    let lower = error_text.to_ascii_lowercase();
    if is_auth_error {
        return "登入憑證失效（session 過期或密碼變更）";
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("逾時")
        || lower.contains("connect")
        || lower.contains("dns")
        || lower.contains("connection reset")
        || lower.contains("network")
    {
        return "網路連線問題";
    }
    if lower.contains("狀態 5")
        || lower.contains("status 5")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
    {
        return "Tronclass 伺服器回應錯誤";
    }
    if lower.contains("429") || lower.contains("too many") {
        return "請求過於頻繁（被限流）";
    }
    if lower.contains("json") || lower.contains("解析") {
        return "回應格式異常";
    }
    "未分類錯誤"
}

fn advice_for(grade: ErrorGrade, cause: &str) -> &'static str {
    match grade {
        ErrorGrade::Critical => {
            if cause.starts_with("登入憑證") {
                "自動重新認證已失敗，請到儀表板確認帳號密碼，或請使用者更新 manual_cookie。"
            } else {
                "系統會持續重試；若長時間未恢復，請檢查伺服器網路與 Tronclass 服務狀態。"
            }
        }
        ErrorGrade::Warning => {
            "系統將自動重新認證並繼續重試，暫時不需要人工介入；恢復後會另行通知。"
        }
        ErrorGrade::Transient => "",
    }
}

#[derive(Debug)]
struct AlertState {
    grade: ErrorGrade,
    first_failed_at: Instant,
    last_sent_at: Instant,
    sent_count: u32,
}

/// 只記錄「已經送出警示」的帳號；恢復時據此決定要不要送恢復訊息。
static ALERTS: Lazy<Mutex<HashMap<String, AlertState>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 判斷這次是否要送出通知：首次達到警告以上、等級升高、或嚴重等級超過重複間隔。
fn should_send(account_id: &str, grade: ErrorGrade) -> bool {
    if grade == ErrorGrade::Transient {
        return false;
    }
    let now = Instant::now();
    let mut alerts = ALERTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match alerts.get_mut(account_id) {
        None => {
            alerts.insert(
                account_id.to_string(),
                AlertState {
                    grade,
                    first_failed_at: now,
                    last_sent_at: now,
                    sent_count: 1,
                },
            );
            true
        }
        Some(state) => {
            let escalated = grade > state.grade;
            let repeat_due = grade == ErrorGrade::Critical
                && state.grade == ErrorGrade::Critical
                && now.duration_since(state.last_sent_at) >= CRITICAL_REPEAT_INTERVAL;
            if escalated || repeat_due {
                state.grade = grade;
                state.last_sent_at = now;
                state.sent_count = state.sent_count.saturating_add(1);
                true
            } else {
                false
            }
        }
    }
}

/// 帳號恢復：若先前送過警示就移除並回傳（先前等級, 中斷時長, 送出次數）。
fn take_recovered(account_id: &str) -> Option<(ErrorGrade, Duration, u32)> {
    let mut alerts = ALERTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    alerts
        .remove(account_id)
        .map(|state| (state.grade, state.first_failed_at.elapsed(), state.sent_count))
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs} 秒")
    } else if secs < 3600 {
        format!("{} 分鐘", secs / 60)
    } else {
        format!("{} 小時 {} 分鐘", secs / 3600, (secs % 3600) / 60)
    }
}

/// 管理員收件人：messenger 設定的 admin_user_id + 資料庫中 is_admin 帳號的 Discord ID。
/// 同時回傳 is_admin 帳號 ID 清單（供手機推播）。
async fn admin_recipients(
    db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
) -> (Vec<String>, Vec<String>) {
    let mut discord_ids: Vec<String> = Vec::new();
    let mut account_ids: Vec<String> = Vec::new();
    if let Some(messenger) = messenger {
        let admin = messenger.admin_user_id().trim();
        if !admin.is_empty() {
            discord_ids.push(admin.to_string());
        }
    }
    match db.list().await {
        Ok(accounts) => {
            for account in accounts {
                if !account.is_admin {
                    continue;
                }
                account_ids.push(account.id.clone());
                let id = account.discord_user_id.trim();
                if !id.is_empty() && !discord_ids.iter().any(|existing| existing == id) {
                    discord_ids.push(id.to_string());
                }
            }
        }
        Err(error) => warn!(error = %error, "監控警示：讀取管理員清單失敗"),
    }
    (discord_ids, account_ids)
}

async fn broadcast_to_admins(
    db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    title: String,
    body: String,
    tag: String,
) {
    let (discord_ids, account_ids) = admin_recipients(db, messenger).await;
    if let Some(messenger) = messenger {
        let message = OutboundMessage::Text(format!("{title}\n\n{body}"));
        for to in &discord_ids {
            if let Err(error) = messenger.push(to, &message).await {
                warn!(to = %to, error = %error, "監控警示 Discord 推送失敗");
            }
        }
    }
    if !account_ids.is_empty() {
        crate::web_push_notifications::notify_admin_alert(account_ids, title, body, tag);
    }
}

/// 輪詢失敗時呼叫。輕微等級只寫 log；警告／嚴重會依節流規則通知管理員。
pub async fn report_failure(
    db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    account_id: &str,
    grade: ErrorGrade,
    consecutive_failures: u32,
    is_auth_error: bool,
    error_text: &str,
) {
    let cause = describe_cause(error_text, is_auth_error);
    if grade == ErrorGrade::Transient {
        debug!(
            account = %account_id,
            consecutive_failures,
            cause,
            "輪詢失敗（輕微），不通知管理員"
        );
        return;
    }
    if !should_send(account_id, grade) {
        debug!(account = %account_id, grade = grade.label(), "監控警示已在節流期間內，略過");
        return;
    }
    info!(
        account = %account_id,
        grade = grade.label(),
        consecutive_failures,
        cause,
        "發送監控警示給管理員"
    );
    let snippet = crate::text_utils::truncate_chars(error_text.trim(), ERROR_SNIPPET_CHARS);
    let title = format!("監控警示（{}）：{account_id}", grade.label());
    let body = format!(
        "帳號：{account_id}\n連續失敗：{consecutive_failures} 次\n原因：{cause}\n錯誤：{snippet}\n\n{}",
        advice_for(grade, cause)
    );
    let tag = format!("tronclass-monitor-alert-{account_id}");
    broadcast_to_admins(db, messenger, title, body, tag).await;
}

/// 輪詢成功（或重新認證成功）時呼叫；只有先前送過警示的帳號才會送恢復訊息。
pub async fn report_recovery(
    db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    account_id: &str,
) {
    let Some((grade, outage, sent_count)) = take_recovered(account_id) else {
        return;
    };
    info!(
        account = %account_id,
        previous_grade = grade.label(),
        outage_secs = outage.as_secs(),
        "帳號監控已恢復，通知管理員"
    );
    let title = format!("監控已恢復：{account_id}");
    let body = format!(
        "帳號：{account_id}\n先前等級：{}\n中斷時長：約 {}\n期間警示次數：{sent_count}\n\n輪詢已恢復正常，不需要進一步處理。",
        grade.label(),
        format_duration(outage)
    );
    let tag = format!("tronclass-monitor-alert-{account_id}");
    broadcast_to_admins(db, messenger, title, body, tag).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_grades_by_failures_and_auth() {
        assert_eq!(classify(false, 1, 5, false), ErrorGrade::Transient);
        assert_eq!(classify(false, 2, 5, false), ErrorGrade::Transient);
        assert_eq!(classify(false, 3, 5, false), ErrorGrade::Warning);
        assert_eq!(classify(true, 1, 5, false), ErrorGrade::Warning);
        assert_eq!(classify(false, 10, 5, false), ErrorGrade::Critical);
        assert_eq!(classify(false, 1, 5, true), ErrorGrade::Critical);
        // max_failures 很小時，嚴重門檻至少是 2×3。
        assert_eq!(classify(false, 5, 1, false), ErrorGrade::Warning);
        assert_eq!(classify(false, 6, 1, false), ErrorGrade::Critical);
    }

    #[test]
    fn should_send_throttles_same_grade_and_allows_escalation() {
        let id = "test-throttle-account";
        let _ = take_recovered(id);
        assert!(!should_send(id, ErrorGrade::Transient));
        assert!(should_send(id, ErrorGrade::Warning));
        assert!(!should_send(id, ErrorGrade::Warning));
        assert!(should_send(id, ErrorGrade::Critical));
        assert!(!should_send(id, ErrorGrade::Critical));
        let recovered = take_recovered(id).expect("alert state should exist");
        assert_eq!(recovered.0, ErrorGrade::Critical);
        assert_eq!(recovered.2, 2);
        assert!(take_recovered(id).is_none());
    }

    #[test]
    fn describe_cause_recognises_common_errors() {
        assert_eq!(describe_cause("operation timed out", false), "網路連線問題");
        assert_eq!(
            describe_cause("API 返回錯誤狀態 503：bad gateway", false),
            "Tronclass 伺服器回應錯誤"
        );
        assert!(describe_cause("whatever", true).starts_with("登入憑證"));
        assert_eq!(describe_cause("???", false), "未分類錯誤");
    }

    #[test]
    fn format_duration_is_human_readable() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45 秒");
        assert_eq!(format_duration(Duration::from_secs(600)), "10 分鐘");
        assert_eq!(format_duration(Duration::from_secs(3900)), "1 小時 5 分鐘");
    }
}
