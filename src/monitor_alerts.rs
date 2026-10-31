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
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::adapters::events::{AdminAlertNotice, AdapterMessenger, OutboundMessage};
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

/// FAILURE_DIAGNOSIS_V1：輪詢／登入失敗的分類結果，同時給管理員通知與
/// 儀表板（帳號列、未監控帳號區塊）顯示「原因 + 下一步」。
///
/// 所有文字不含表情符號；`message` 已截斷且不含憑證（呼叫端傳入的錯誤鏈
/// 本來就不含密碼，這裡再多做一次長度限制）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDiagnosis {
    /// 機器可讀代碼：auth / password_blocked / captcha / network / server /
    /// rate_limited / parse / unknown。
    pub code: String,
    /// 人看的分類名稱。
    pub label: String,
    /// 給帳號持有人的自助排除建議。
    pub advice: String,
    /// 截斷後的原始錯誤訊息。
    pub message: String,
    /// 發生時間（Unix 秒）。
    pub at: i64,
}

/// FAILURE_DIAGNOSIS_V1：關鍵字分類表（依優先順序）。
///
/// 回傳 (code, label, advice)。`describe_cause` 與 [`diagnose`] 共用，
/// 確保通知與儀表板講的是同一套詞。
fn classify_cause(error_text: &str, is_auth_error: bool) -> (&'static str, &'static str, &'static str) {
    let lower = error_text.to_ascii_lowercase();
    // 儲存密碼解密失敗／被阻止送出：比 auth 更前面，因為這種情況重登也沒用。
    if lower.contains("已阻止送出登入請求")
        || lower.contains("無法解密")
        || lower.contains("沒有可用的登入密碼")
    {
        return (
            "password_blocked",
            "儲存密碼無法使用",
            "此帳號的密碼在資料庫中無法解密或為空，自動重登不會成功。請管理員確認 TRONCLASS_MASTER_KEY 設定，或在儀表板重新儲存此帳號的 Tronclass 密碼。",
        );
    }
    if is_auth_error {
        return (
            "auth",
            "登入憑證失效（session 過期或密碼變更）",
            "系統會自動重新登入。若持續失敗，多半是 Tronclass 密碼已更改：請到儀表板更新此帳號密碼後按「重登」。",
        );
    }
    if lower.contains("captcha") || lower.contains("驗證碼") {
        return (
            "captcha",
            "登入驗證碼辨識失敗",
            "學校登入頁的驗證碼辨識暫時失敗，系統會自動重試；若連續失敗超過 30 分鐘，請按一次「重登」。",
        );
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("逾時")
        || lower.contains("connect")
        || lower.contains("dns")
        || lower.contains("connection reset")
        || lower.contains("network")
    {
        return (
            "network",
            "網路連線問題",
            "本機到學校系統的連線暫時不通（學校主機維護、網路波動）。系統會以逐步拉長的間隔自動重試，不需要操作。",
        );
    }
    if lower.contains("狀態 5")
        || lower.contains("status 5")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
    {
        return (
            "server",
            "Tronclass 伺服器回應錯誤",
            "學校 Tronclass 伺服器回傳錯誤（5xx），通常是對方維護或過載。系統會自動重試；若超過一天未恢復請回報管理員。",
        );
    }
    if lower.contains("429") || lower.contains("too many") {
        return (
            "rate_limited",
            "請求過於頻繁（被限流）",
            "學校系統暫時限制了請求頻率。系統已自動拉長重試間隔；請不要連續按「立即檢查」，等待 10 到 15 分鐘後會自行恢復。",
        );
    }
    if lower.contains("json") || lower.contains("解析") {
        return (
            "parse",
            "回應格式異常",
            "學校系統回傳的內容與預期不符，可能正在改版或維護。系統會自動重試；若持續超過一天請回報管理員。",
        );
    }
    (
        "unknown",
        "未分類錯誤",
        "請把下方錯誤訊息回報給管理員，並可先按一次「重登」看是否恢復。",
    )
}

/// 把錯誤字串粗略歸類成人看得懂的原因，附在通知裡。
pub fn describe_cause(error_text: &str, is_auth_error: bool) -> &'static str {
    classify_cause(error_text, is_auth_error).1
}

/// FAILURE_DIAGNOSIS_V1：產生一筆完整診斷（分類 + 建議 + 截斷訊息 + 時間）。
pub fn diagnose(error_text: &str, is_auth_error: bool) -> FailureDiagnosis {
    let (code, label, advice) = classify_cause(error_text, is_auth_error);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    FailureDiagnosis {
        code: code.to_string(),
        label: label.to_string(),
        advice: advice.to_string(),
        message: crate::text_utils::truncate_chars(error_text.trim(), ERROR_SNIPPET_CHARS).to_string(),
        at,
    }
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
) -> (
    // (Discord user id, 對應的帳號 id)：DC_CHANNEL_PREFS_V1 逐位管理員檢查通知偏好用；
    // messenger admin 無對應帳號時為 None（不檢查、維持送達）。
    Vec<(String, Option<String>)>,
    Vec<String>,
) {
    let mut discord_recipients: Vec<(String, Option<String>)> = Vec::new();
    let mut account_ids: Vec<String> = Vec::new();
    if let Some(messenger) = messenger {
        let admin = messenger.admin_user_id().trim();
        if !admin.is_empty() {
            discord_recipients.push((admin.to_string(), None));
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
                if !id.is_empty()
                    && !discord_recipients
                        .iter()
                        .any(|(existing, _)| existing == id)
                {
                    discord_recipients.push((id.to_string(), Some(account.id.clone())));
                }
            }
        }
        Err(error) => warn!(error = %error, "監控警示：讀取管理員清單失敗"),
    }
    (discord_recipients, account_ids)
}

#[allow(clippy::too_many_arguments)]
async fn broadcast_to_admins(
    db: &AccountsDb,
    messenger: Option<&dyn AdapterMessenger>,
    account_id: &str,
    grade_label: &str,
    is_recovery: bool,
    title: String,
    body: String,
    tag: String,
) {
    let (discord_recipients, account_ids) = admin_recipients(db, messenger).await;
    if let Some(messenger) = messenger {
        // DISCORD_NOTIFY_UNIFY_V1：結構化事件，Discord 端依等級上色。
        let message = OutboundMessage::AdminAlert(AdminAlertNotice {
            account: account_id.to_string(),
            grade: grade_label.to_string(),
            is_recovery,
            title: title.clone(),
            body: body.clone(),
        });
        for (to, pref_account) in &discord_recipients {
            // DC_CHANNEL_PREFS_V1：管理員可在通知設定頁關閉監控警示的 Discord 通知。
            if let Some(pref_account_id) = pref_account {
                match db
                    .notification_preference_enabled(pref_account_id, "discord", "monitor_alert")
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(
                            account = %pref_account_id,
                            "監控警示 Discord 通知已由使用者關閉"
                        );
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            account = %pref_account_id,
                            error = %error,
                            "讀取監控警示 Discord 通知設定失敗，仍發送"
                        );
                    }
                }
            }
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
    broadcast_to_admins(db, messenger, account_id, grade.label(), false, title, body, tag).await;
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
    broadcast_to_admins(db, messenger, account_id, "已恢復", true, title, body, tag).await;
}

