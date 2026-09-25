//! Account-scoped background Web Push notifications.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes_gcm::{aead::Aead, Aes128Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hkdf::Hkdf;
use miette::Result;
use once_cell::sync::{Lazy, OnceCell};
use p256::{
    ecdh::diffie_hellman,
    ecdsa::{signature::Signer, Signature, SigningKey},
    elliptic_curve::sec1::ToEncodedPoint,
    PublicKey, SecretKey,
};
use serde_json::json;
use sha2::Sha256;
use tracing::{debug, warn};

use crate::adapters::events::{
    AdapterAccountTarget, AdapterMessenger, OutboundMessage, PushSubscriptionExpiredEvent,
};
use crate::db::{AccountsDb, WebPushSubscriptionRow};

const VAPID_PRIVATE_KEY_SETTING: &str = "vapid_private_key_v1";
const MAX_DEDUPE_ENTRIES: usize = 4096;
/// Maximum number of Web Push HTTP sends in flight at once. Prevents a burst
/// of notifications (or many subscriptions) from spawning unbounded concurrent
/// tasks / connections against the push service.
const MAX_CONCURRENT_SENDS: usize = 8;

/// Global concurrency gate for outbound Web Push sends.
static SEND_SEMAPHORE: Lazy<tokio::sync::Semaphore> =
    Lazy::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_SENDS));

/// Dedup window for notifications.
const DEDUPE_WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// PUSH_STALE_CLEANUP_V1：同一端點連續失敗達此次數即視為失效並移除。
/// 404/410（Gone）不等待、立即移除；其他狀態（400/401/403 金鑰／簽章問題、
/// 5xx／網路錯誤）需連續失敗到門檻，避免推播服務短暫故障就把訂閱清掉。
const STALE_SUBSCRIPTION_FAILURE_THRESHOLD: i64 = 5;
/// 同一帳號的「訂閱失效」提醒最短間隔（避免多裝置／多次通知同日重複打擾）。
const EXPIRED_NOTICE_COOLDOWN: Duration = Duration::from_secs(12 * 60 * 60);

/// PUSH_STALE_CLEANUP_V1：送出「推播失效」提醒的 Discord／LINE messenger（由 main 注入）。
static EXPIRED_NOTICE_MESSENGER: OnceCell<Arc<dyn AdapterMessenger>> = OnceCell::new();
static EXPIRED_NOTICE_RECENT: Lazy<Mutex<HashMap<String, Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 由 main 在 adapter 建好後呼叫；未呼叫時失效訂閱仍會移除，只是不發提醒。
pub fn set_expired_notice_messenger(messenger: Arc<dyn AdapterMessenger>) {
    let _ = EXPIRED_NOTICE_MESSENGER.set(messenger);
}

/// PUSH_STALE_CLEANUP_V1：單一裝置的發送結果（供 send_to_many 判斷是否要通知）。
enum SendOutcome {
    Delivered,
    /// 暫時性失敗：計數 +1 但尚未達門檻。
    Failed(String),
    /// 已判定失效並從資料庫移除。
    Removed(String),
}

/// Check-and-record a notification key in the dedupe map.
///
/// Returns `true` if the notification is new (should be sent), `false` if it
/// was already sent within the window (suppress).
///
/// This is the "lazy cleanup" version: it does **not** scan the whole map on
/// every call. A single key is O(1); only when the map reaches its cap do we
/// pay an O(n) prune of expired entries, so the hot path stays cheap and does
/// not hold the lock while scanning 4096 rows repeatedly.
fn dedupe_should_send(recent: &mut HashMap<String, Instant>, key: &str) -> bool {
    let now = Instant::now();
    let window = DEDUPE_WINDOW;

    if let Some(&at) = recent.get(key) {
        if now.duration_since(at) < window {
            // Already sent within the window → suppress.
            return false;
        }
        // Entry exists but expired → replace it below.
    }

    if recent.len() >= MAX_DEDUPE_ENTRIES {
        // Rare: only when the map is at capacity, prune all expired entries.
        recent.retain(|_, at| now.duration_since(*at) < window);
    }

    recent.insert(key.to_string(), now);
    true
}

#[derive(Clone)]
struct WebPushService {
    db: AccountsDb,
    http: reqwest::Client,
    private_key: String,
    public_key: String,
}

static SERVICE: OnceCell<Arc<WebPushService>> = OnceCell::new();
static RECENT: Lazy<Mutex<HashMap<String, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub async fn initialize(db: AccountsDb) -> Result<()> {
    if SERVICE.get().is_some() {
        return Ok(());
    }
    // Older clients silently rebound the browser's one PushSubscription to
    // whichever account happened to log in next. Those rows cannot be trusted
    // for owner-only delivery, so clear them once and require an explicit tap.
    const EXPLICIT_BINDING_MIGRATION: &str = "web_push_explicit_binding_v2";
    if db
        .get_web_push_setting(EXPLICIT_BINDING_MIGRATION)
        .await?
        .as_deref()
        != Some("1")
    {
        let removed = db.clear_all_web_push_subscriptions().await?;
        db.set_web_push_setting(EXPLICIT_BINDING_MIGRATION, "1")
            .await?;
        warn!(removed, "已清除舊版可能錯綁帳號的 Web Push 訂閱；需由手機重新明確啟用");
    }
    let private_key = match db.get_web_push_setting(VAPID_PRIVATE_KEY_SETTING).await? {
        Some(value) if valid_private_key(&value) => value,
        _ => {
            let value = generate_private_key()?;
            db.set_web_push_setting(VAPID_PRIVATE_KEY_SETTING, &value)
                .await?;
            value
        }
    };
    let signing_key = decode_signing_key(&private_key)?;
    let public_key = URL_SAFE_NO_PAD.encode(
        signing_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes(),
    );
    let http = reqwest::Client::builder()
        // Keep synchronous test requests below common panel/proxy timeouts so
        // the UI receives a useful JSON error instead of a generic HTTP 502.
        .timeout(Duration::from_secs(8))
        .use_rustls_tls()
        .build()
        .map_err(|error| miette::miette!("建立 Web Push HTTP client 失敗：{error}"))?;
    let _ = SERVICE.set(Arc::new(WebPushService {
        db,
        http,
        private_key,
        public_key,
    }));
    Ok(())
}

pub fn public_key() -> Option<String> {
    SERVICE.get().map(|service| service.public_key.clone())
}

pub async fn send_test(account_id: &str) -> Result<usize> {
    let service = SERVICE
        .get()
        .ok_or_else(|| miette::miette!("Web Push 尚未初始化"))?;
    let subscriptions = service.db.list_web_push_subscriptions(account_id).await?;
    let payload = serde_json::to_vec(&json!({
        "title": "Tronclass 手機通知測試",
        "body": "背景推播已啟用；偵測到點名時會通知此帳號。",
        "url": "/admin/dashboard#rollcall",
        "tag": format!("tronclass-push-test-{account_id}"),
    }))
    .map_err(|error| miette::miette!("序列化 Web Push 測試 payload 失敗：{error}"))?;
    let total = subscriptions.len();
    let (ok, _failed) = service.send_to_many(subscriptions, &payload).await;
    if total > 0 && ok == 0 {
        return Err(miette::miette!("Web Push 測試全部發送失敗"));
    }
    Ok(total)
}

/// Send a homework-deadline test push immediately.
///
/// HOMEWORK_TEST_NOTIFICATION_V1
/// Unlike [`notify_homework`], this bypasses the two-hour dedupe cache so the
/// user can press the test button repeatedly, and it reports delivery errors to
/// the caller instead of only logging them.
pub async fn send_homework_test(
    account_id: &str,
    course_name: &str,
    title: &str,
    stage_badge: &str,
) -> Result<usize> {
    let service = SERVICE
        .get()
        .ok_or_else(|| miette::miette!("Web Push 尚未初始化"))?;
    let subscriptions = service.db.list_web_push_subscriptions(account_id).await?;
    let payload = serde_json::to_vec(&json!({
        "title": format!("作業截止提醒｜{course_name}"),
        "body": format!("{stage_badge}：{title}"),
        "url": "/admin/homework",
        "tag": format!("tronclass-homework-test-{account_id}"),
    }))
    .map_err(|error| miette::miette!("序列化 Web Push 測試 payload 失敗：{error}"))?;
    let total = subscriptions.len();
    let (ok, _failed) = service.send_to_many(subscriptions, &payload).await;
    if total > 0 && ok == 0 {
        return Err(miette::miette!("Web Push 作業測試全部發送失敗"));
    }
    Ok(total)
}

/// MORNING_DIGEST_V1：發送晨間摘要給帳號已訂閱推播的裝置。
/// tag 以「帳號＋日期」為後綴——同日重發會覆蓋同一則通知，不會洗版。
pub async fn send_digest(account_id: &str, title: &str, body: &str) -> Result<usize> {
    let service = SERVICE
        .get()
        .ok_or_else(|| miette::miette!("Web Push 尚未初始化"))?;
    let subscriptions = service.db.list_web_push_subscriptions(account_id).await?;
    let payload = serde_json::to_vec(&json!({
        "title": title,
        "body": body,
        "url": "/admin/dashboard#overview",
        "tag": format!(
            "tronclass-digest-{account_id}-{}",
            chrono::Local::now().format("%Y%m%d")
        ),
    }))
    .map_err(|error| miette::miette!("序列化晨間摘要 payload 失敗：{error}"))?;
    let total = subscriptions.len();
    let (ok, _failed) = service.send_to_many(subscriptions, &payload).await;
    if total > 0 && ok == 0 {
        return Err(miette::miette!("晨間摘要全部發送失敗"));
    }
    Ok(total)
}

/// Queue one account-owner notification. Repeated monitor passes for the same
/// rollcall are suppressed for two hours.
/// PUSH_CHANNEL_PREFS_V1：以 channel="push" 查詢該帳號是否啟用此類推播。
/// 查無設定或資料庫錯誤時預設允許（與 Discord 端 `discord_notification_enabled` 一致），
/// 讓使用者能在通知設定頁逐類關閉手機推播。
async fn push_preference_enabled(service: &WebPushService, account_id: &str, key: &str) -> bool {
    match service
        .db
        .notification_preference_enabled(account_id, "push", key)
        .await
    {
        Ok(enabled) => enabled,
        Err(error) => {
            warn!(
                account = %account_id,
                key = key,
                error = %error,
                "讀取 Web Push 通知設定失敗，預設允許通知"
            );
            true
        }
    }
}

pub fn notify_rollcall(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
) {
    notify_rollcall_with_url(account_id, rollcall_id, course_name, attendance_type, None);
}

/// PUSH_DEEP_LINK_V1：數字／QR 點名在註冊好輸入頁之後才發推播，把該帳號專屬的
/// 輸入頁／掃碼頁完整網址塞進 payload，使用者點通知直接進到輸入畫面，
/// 不用再從儀表板找。`target_url` 為 None 或空字串時退回儀表板點名分頁。
/// 去重 key 與 `notify_rollcall` 共用，所以同一筆點名只會送一次。
pub fn notify_rollcall_with_url(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
    target_url: Option<&str>,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let key = format!("{account_id}:{rollcall_id}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !dedupe_should_send(&mut recent, &key) {
            return;
        }
    }

    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let attendance_type = attendance_type.to_string();
    let target_url = target_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string);
    tokio::spawn(async move {
        // PUSH_CHANNEL_PREFS_V1：點名偵測推播可在通知設定頁關閉。
        if !push_preference_enabled(&service, &account_id, "rollcall_detected").await {
            return;
        }
        if let Err(error) = service
            .send_rollcall(
                &account_id,
                rollcall_id,
                &course_name,
                &attendance_type,
                target_url.as_deref(),
            )
            .await
        {
            warn!(account = %account_id, rollcall_id, error = %error, "Web Push 點名通知失敗");
        }
    });
}

/// ROLLCALL_INPUT_REMINDER_V1：等了 N 秒還沒有輸入，再推一次（同一筆點名只催一次）。
/// tag 沿用原本的點名通知 tag，裝置上會把原通知換成催促內容（renotify 會再響一次），
/// 不會堆成兩則。
pub fn notify_rollcall_reminder(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
    target_url: Option<&str>,
    remaining_secs: u64,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let key = format!("remind:{account_id}:{rollcall_id}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !dedupe_should_send(&mut recent, &key) {
            return;
        }
    }
    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let attendance_type = attendance_type.to_string();
    let target_url = target_url
        .map(str::trim)
        .filter(|url| url.starts_with("https://") || url.starts_with('/'))
        .map(str::to_string);
    tokio::spawn(async move {
        // PUSH_CHANNEL_PREFS_V1：催促跟著數字／QR 點名的開關走（與 Discord 端一致）。
        let reminder_key = if attendance_type.contains("數字") {
            "number_code"
        } else {
            "qr_code"
        };
        if !push_preference_enabled(&service, &account_id, reminder_key).await {
            return;
        }
        let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(account = %account_id, error = %error, "Web Push 點名催促讀取訂閱失敗");
                return;
            }
        };
        if subscriptions.is_empty() {
            return;
        }
        let url = target_url
            .clone()
            .unwrap_or_else(|| "/admin/dashboard#rollcall".to_string());
        let payload = match serde_json::to_vec(&json!({
            "title": format!("還沒送出：{course_name}"),
            "body": format!(
                "{attendance_type}｜帳號 {account_id}，還剩 {remaining_secs} 秒就會逾時，點一下立即輸入"
            ),
            "url": url,
            "tag": format!("tronclass-rollcall-{account_id}-{rollcall_id}"),
            "rollcall_id": rollcall_id,
            "deep_link": target_url.is_some(),
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化 Web Push 催促 payload 失敗");
                return;
            }
        };
        service.send_to_many(subscriptions, &payload).await;
    });
}

/// ROLLCALL_FINISH_PUSH_V1：點名嘗試結束後的結果推播（成功／失敗），
/// 與 Discord 的 RollcallFinished 對應；偏好鍵 `rollcall_finished` 可在通知設定頁關閉。
pub fn notify_rollcall_finished(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
    success: bool,
    result_text: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let attendance_type = attendance_type.to_string();
    let result_text = result_text.to_string();
    tokio::spawn(async move {
        // PUSH_CHANNEL_PREFS_V1：點名完成結果推播可在通知設定頁關閉。
        if !push_preference_enabled(&service, &account_id, "rollcall_finished").await {
            return;
        }
        let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(account = %account_id, error = %error, "Web Push 點名結果讀取訂閱失敗");
                return;
            }
        };
        if subscriptions.is_empty() {
            return;
        }
        let headline = if success { "點名完成" } else { "點名未完成" };
        let payload = match serde_json::to_vec(&json!({
            "title": format!("{headline}：{course_name}"),
            "body": format!("{attendance_type}｜帳號 {account_id}｜{result_text}"),
            "url": "/admin/dashboard#rollcall",
            "tag": format!("tronclass-rollcall-finished-{account_id}-{rollcall_id}"),
            "rollcall_id": rollcall_id,
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化 Web Push 點名結果 payload 失敗");
                return;
            }
        };
        service.send_to_many(subscriptions, &payload).await;
    });
}

/// ROLLCALL_SUBMITTED_NOTICE_V1（fix32）：點名送出成功後、二次確認完成前的
/// 即時推播。確認最多需要約 9 秒（3 次×3 秒），這段空窗讓使用者知道點名
/// 「已送出、正在確認」而不是卡住。偏好鍵沿用 rollcall_finished（設定頁
/// 不變）；tag 與最終結果通知相同——瀏覽器會用最終結果直接「替換」這則
/// 通知，不會堆兩則。
pub fn notify_rollcall_submitted(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let attendance_type = attendance_type.to_string();
    tokio::spawn(async move {
        if !push_preference_enabled(&service, &account_id, "rollcall_finished").await {
            return;
        }
        let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(account = %account_id, error = %error, "Web Push 點名已送出通知讀取訂閱失敗");
                return;
            }
        };
        if subscriptions.is_empty() {
            return;
        }
        let payload = match serde_json::to_vec(&json!({
            "title": format!("點名已送出：{course_name}"),
            "body": format!("{attendance_type}｜帳號 {account_id}｜已送出 iClass，正在二次確認…"),
            "url": "/admin/dashboard#rollcall",
            "tag": format!("tronclass-rollcall-finished-{account_id}-{rollcall_id}"),
            "rollcall_id": rollcall_id,
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化 Web Push 點名已送出 payload 失敗");
                return;
            }
        };
        service.send_to_many(subscriptions, &payload).await;
    });
}

/// CLASS_REMINDER_PUSH_V1：上課前提醒的手機推播（與 Discord DM 對應）。
/// 偏好走 class_reminder_preferences.push_enabled（上課提醒卡開關），不走 notification_preferences。
pub fn notify_class_reminder(
    account_id: &str,
    course_name: &str,
    headline: &str,
    body_text: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let headline = headline.to_string();
    let body_text = body_text.to_string();
    tokio::spawn(async move {
        let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
            Ok(rows) => rows,
            Err(error) => {
                warn!(account = %account_id, error = %error, "Web Push 上課提醒讀取訂閱失敗");
                return;
            }
        };
        if subscriptions.is_empty() {
            return;
        }
        let payload = match serde_json::to_vec(&json!({
            "title": format!("上課提醒｜{course_name}"),
            "body": format!("{headline}｜{body_text}"),
            "url": "/admin/dashboard#timetable",
            "tag": format!("tronclass-class-reminder-{account_id}"),
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化 Web Push 上課提醒 payload 失敗");
                return;
            }
        };
        service.send_to_many(subscriptions, &payload).await;
    });
}

/// RADAR_SCHOOL_BLOCK_V1（fix77）：雷達送出被學校端擋下（429／403）時的推播。
///
/// 與 [`notify_admin_alert`] 的差別：雷達流程是在背景跑（手上沒有 `AccountsDb`），
/// 所以這裡自己從服務的資料庫挑出管理員帳號，再走同一條推播路徑（含
/// `notification_preferences` 的 monitor_alert 開關）。
pub fn notify_admins_school_block(title: String, body: String, tag: String) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    tokio::spawn(async move {
        let admins: Vec<String> = match service.db.list().await {
            Ok(accounts) => accounts
                .into_iter()
                .filter(|account| account.is_admin)
                .map(|account| account.id)
                .collect(),
            Err(error) => {
                warn!(error = %error, "學校端擋下告警：讀取管理員清單失敗");
                return;
            }
        };
        if admins.is_empty() {
            return;
        }
        warn!(admins = admins.len(), tag = %tag, "學校端擋下：已推播管理員");
        notify_admin_alert(admins, title, body, tag);
    });
}

/// MONITOR_ALERT_GRADING_V1：把監控警示／恢復訊息推到所有管理員帳號的裝置。
/// tag 以帳號為後綴，同一帳號的警示→恢復會覆蓋同一則通知，不會洗版。
/// 呼叫端已做節流，這裡不再去重。
pub fn notify_admin_alert(admin_account_ids: Vec<String>, title: String, body: String, tag: String) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    tokio::spawn(async move {
        let payload = match serde_json::to_vec(&json!({
            "title": title,
            "body": body,
            "url": "/admin/dashboard#maintenance",
            "tag": tag,
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化監控警示 payload 失敗");
                return;
            }
        };
        for account_id in admin_account_ids {
            // PUSH_CHANNEL_PREFS_V1：監控警示推播可在通知設定頁關閉（管理員各自設定）。
            if !push_preference_enabled(&service, &account_id, "monitor_alert").await {
                continue;
            }
            let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
                Ok(rows) => rows,
                Err(error) => {
                    warn!(account = %account_id, error = %error, "監控警示讀取推播訂閱失敗");
                    continue;
                }
            };
            if subscriptions.is_empty() {
                continue;
            }
            service.send_to_many(subscriptions, &payload).await;
        }
    });
}

/// LOGIN_LOCATION_ALERT_V1：安全類推播（登入異常地點）。與 `notify_admin_alert`
/// 的差別只有落地頁：點開直接進「登入裝置管理」，方便立刻踢掉可疑 session。
pub fn notify_security_alert(account_ids: Vec<String>, title: String, body: String, tag: String) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    tokio::spawn(async move {
        let payload = match serde_json::to_vec(&json!({
            "title": title,
            "body": body,
            "url": "/admin/devices",
            "tag": tag,
        })) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(error = %error, "序列化安全警示 payload 失敗");
                return;
            }
        };
        for account_id in account_ids {
            // PUSH_CHANNEL_PREFS_V1：安全警示推播可在通知設定頁關閉。
            if !push_preference_enabled(&service, &account_id, "security_alert").await {
                continue;
            }
            let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
                Ok(rows) => rows,
                Err(error) => {
                    warn!(account = %account_id, error = %error, "安全警示讀取推播訂閱失敗");
                    continue;
                }
            };
            if subscriptions.is_empty() {
                continue;
            }
            service.send_to_many(subscriptions, &payload).await;
        }
    });
}

/// TRAFFIC_ALERT_PUSH_V1：異常流量警示推播（60 秒內嘗試超過上限、已暫時封鎖）。
/// 落地頁是登入審計（/admin/activity）；偏好鍵 `traffic_alert` 可在通知設定頁關閉。
pub fn notify_traffic_alert(admin_account_ids: Vec<String>, title: String, body: String) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    tokio::spawn(async move {
        for account_id in admin_account_ids {
            // PUSH_CHANNEL_PREFS_V1：異常流量推播可在通知設定頁關閉（管理員各自設定）。
            if !push_preference_enabled(&service, &account_id, "traffic_alert").await {
                continue;
            }
            let subscriptions = match service.db.list_web_push_subscriptions(&account_id).await {
                Ok(rows) => rows,
                Err(error) => {
                    warn!(account = %account_id, error = %error, "異常流量警示讀取推播訂閱失敗");
                    continue;
                }
            };
            if subscriptions.is_empty() {
                continue;
            }
            let payload = match serde_json::to_vec(&json!({
                "title": title,
                "body": body,
                "url": "/admin/activity",
                "tag": "tronclass-traffic-alert",
            })) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!(error = %error, "序列化異常流量 payload 失敗");
                    return;
                }
            };
            service.send_to_many(subscriptions, &payload).await;
        }
    });
}

/// Queue one account-owner homework deadline notification.
pub fn notify_homework(
    account_id: &str,
    homework_id: u64,
    course_name: &str,
    title: &str,
    stage_badge: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let key = format!("hw:{account_id}:{homework_id}:{stage_badge}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !dedupe_should_send(&mut recent, &key) {
            return;
        }
    }

    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let title = title.to_string();
    let stage_badge = stage_badge.to_string();
    tokio::spawn(async move {
        // PUSH_CHANNEL_PREFS_V1：作業截止推播可在通知設定頁關閉。
        if !push_preference_enabled(&service, &account_id, "homework_deadline").await {
            return;
        }
        if let Err(error) = service
            .send_homework(&account_id, &course_name, &title, &stage_badge, false)
            .await
        {
            warn!(account = %account_id, error = %error, "Web Push 作業截止通知失敗");
        }
    });
}

/// HOMEWORK_NEW_NOTIFY_V1：排入一則「新上架作業」推播。
///
/// 與 [`notify_homework`] 共用通知偏好開關（`homework_deadline`：作業提醒），
/// 差別在標題（🆕 新作業）與點擊後前往的路徑（作業頁，而不是總覽）。
pub fn notify_homework_new(
    account_id: &str,
    homework_id: u64,
    course_name: &str,
    title: &str,
    stage_badge: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    // 去重鍵含徽章：同一份作業的「新作業」與「24 小時」是兩則不同通知，
    // 但同一則不會因為同一輪被評估兩次而重複發送。
    let key = format!("hwnew:{account_id}:{homework_id}:{stage_badge}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !dedupe_should_send(&mut recent, &key) {
            return;
        }
    }

    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let title = title.to_string();
    let stage_badge = stage_badge.to_string();
    tokio::spawn(async move {
        if !push_preference_enabled(&service, &account_id, "homework_deadline").await {
            return;
        }
        if let Err(error) = service
            .send_homework(&account_id, &course_name, &title, &stage_badge, true)
            .await
        {
            warn!(account = %account_id, error = %error, "Web Push 新作業通知失敗");
        }
    });
}

/// BULLETIN_PUSH_V1: 排入一則課程公告推播（去重後非同步發送）。
pub fn notify_bulletin(
    account_id: &str,
    bulletin_key: &str,
    course_name: &str,
    title: &str,
    summary: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let key = format!("bulletin:{account_id}:{bulletin_key}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !dedupe_should_send(&mut recent, &key) {
            return;
        }
    }

    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let title = title.to_string();
    let summary = summary.to_string();
    tokio::spawn(async move {
        // PUSH_CHANNEL_PREFS_V1：公告推播（課程＋管理員）可在通知設定頁關閉。
        if !push_preference_enabled(&service, &account_id, "bulletin").await {
            return;
        }
        if let Err(error) = service
            .send_bulletin(&account_id, &course_name, &title, &summary)
            .await
        {
            warn!(account = %account_id, error = %error, "Web Push 公告通知失敗");
        }
    });
}

impl WebPushService {
    /// `is_new`＝HOMEWORK_NEW_NOTIFY_V1 的新作業通知（標題與落地頁不同）。
    async fn send_homework(
        &self,
        account_id: &str,
        course_name: &str,
        title: &str,
        stage_badge: &str,
        is_new: bool,
    ) -> Result<()> {
        let subscriptions = self.db.list_web_push_subscriptions(account_id).await?;
        if subscriptions.is_empty() {
            return Ok(());
        }
        let headline = if is_new { "🆕 新作業" } else { "作業截止提醒" };
        let url = if is_new { "/admin/homework" } else { "/admin/dashboard" };
        let payload = serde_json::to_vec(&json!({
            "title": format!("{headline}｜{course_name}"),
            "body": format!("{stage_badge}：{title}"),
            "url": url,
            "tag": format!("tronclass-homework-{account_id}"),
        }))
        .map_err(|error| miette::miette!("序列化 Web Push 作業 payload 失敗：{error}"))?;

        self.send_to_many(subscriptions, &payload).await;
        Ok(())
    }

    async fn send_bulletin(
        &self,
        account_id: &str,
        course_name: &str,
        title: &str,
        summary: &str,
    ) -> Result<()> {
        let subscriptions = self.db.list_web_push_subscriptions(account_id).await?;
        if subscriptions.is_empty() {
            return Ok(());
        }
        // BULLETIN_NOTIFY_SUMMARY_V1：通知主體優先放 AI 重點摘要（精簡但保留截止、
        // 地點、待辦等關鍵資訊）；摘要為空時退回標題。避免手機彈出一長串。
        let body = if summary.trim().is_empty() {
            title.to_string()
        } else {
            summary.to_string()
        };
        let payload = serde_json::to_vec(&json!({
            "title": format!("新公告｜{course_name}"),
            "body": body,
            "url": "/admin/bulletins",
            "tag": format!("tronclass-bulletin-{account_id}"),
        }))
        .map_err(|error| miette::miette!("序列化 Web Push 公告 payload 失敗：{error}"))?;

        self.send_to_many(subscriptions, &payload).await;
        Ok(())
    }

    async fn send_rollcall(
        &self,
        account_id: &str,
        rollcall_id: u64,
        course_name: &str,
        attendance_type: &str,
        target_url: Option<&str>,
    ) -> Result<()> {
        let subscriptions = self.db.list_web_push_subscriptions(account_id).await?;
        if subscriptions.is_empty() {
            return Ok(());
        }
        // PUSH_DEEP_LINK_V1：只接受 https:// 絕對網址或站內相對路徑，其餘退回儀表板。
        let deep_link = target_url
            .filter(|url| url.starts_with("https://") || url.starts_with('/'))
            .unwrap_or("/admin/dashboard#rollcall");
        let has_deep_link = deep_link != "/admin/dashboard#rollcall";
        let body = if has_deep_link {
            format!("{attendance_type}｜帳號 {account_id}，點一下直接開啟輸入頁")
        } else {
            format!("{attendance_type}｜帳號 {account_id}，點一下立即查看")
        };
        let payload = serde_json::to_vec(&json!({
            "title": format!("偵測到點名：{course_name}"),
            "body": body,
            "url": deep_link,
            "tag": format!("tronclass-rollcall-{account_id}-{rollcall_id}"),
            "rollcall_id": rollcall_id,
            "deep_link": has_deep_link,
        }))
        .map_err(|error| miette::miette!("序列化 Web Push payload 失敗：{error}"))?;

        self.send_to_many(subscriptions, &payload).await;
        Ok(())
    }

    /// 單一裝置發送。成功回 Ok(())；失敗回 Err，並由 PUSH_STALE_CLEANUP_V1 的計數決定
    /// 是否移除訂閱（見 `send_one_tracked`）。
    async fn send_one(&self, subscription: &WebPushSubscriptionRow, payload: &[u8]) -> Result<()> {
        let endpoint = reqwest::Url::parse(&subscription.endpoint)
            .map_err(|error| miette::miette!("Web Push endpoint 無效：{error}"))?;
        let encrypted = encrypt_payload(
            payload,
            &subscription.p256dh,
            &subscription.auth,
        )?;
        let authorization = vapid_authorization(&self.private_key, &self.public_key, &endpoint)?;
        let response = self
            .http
            .post(endpoint)
            .header("TTL", "300")
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("Authorization", authorization)
            .body(encrypted)
            .send()
            .await
            .map_err(|error| miette::miette!("Web Push HTTP 發送失敗：{error}"))?;
        let status = response.status();
        if status.is_success() {
            debug!(account = %subscription.account_id, "Web Push 已送達推播服務");
            return Ok(());
        }
        let detail = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect::<String>();
        if detail.trim().is_empty() {
            Err(miette::miette!("Apple 推播服務回應 HTTP {}", status.as_u16()))
        } else {
            Err(miette::miette!(
                "Apple 推播服務回應 HTTP {}：{}",
                status.as_u16(),
                detail.trim()
            ))
        }
    }

    /// PUSH_STALE_CLEANUP_V1：包住 send_one，維護每端點的連續失敗計數並在
    /// 永久性錯誤（404/410）或達門檻時移除訂閱。
    ///
    /// 判定規則：
    /// - 成功 → 計數歸零（原本非 0 才寫 DB）。
    /// - HTTP 404 / 410 → 端點已不存在，立即移除。
    /// - 其他失敗（含 400/401/403、5xx、網路錯誤、解密／簽章錯誤）→ 計數 +1，
    ///   達 `STALE_SUBSCRIPTION_FAILURE_THRESHOLD` 才移除。這樣 VAPID 金鑰輪替、
    ///   推播服務短暫故障都能被涵蓋，又不會一次抖動就清掉訂閱。
    async fn send_one_tracked(
        &self,
        subscription: &WebPushSubscriptionRow,
        payload: &[u8],
    ) -> SendOutcome {
        match self.send_one(subscription, payload).await {
            Ok(()) => {
                if subscription.consecutive_failures != 0 {
                    let _ = self
                        .db
                        .reset_web_push_failures(&subscription.account_id, &subscription.endpoint)
                        .await;
                }
                SendOutcome::Delivered
            }
            Err(error) => {
                let message = format!("{error}");
                let gone = http_status_of(&message)
                    .map(|code| matches!(code, 404 | 410))
                    .unwrap_or(false);
                if gone {
                    let _ = self
                        .db
                        .delete_web_push_subscription(&subscription.account_id, &subscription.endpoint)
                        .await;
                    warn!(
                        account = %subscription.account_id,
                        error = %message,
                        "Web Push 端點已不存在，訂閱已移除"
                    );
                    return SendOutcome::Removed(short_reason(&message));
                }
                let failures = match self
                    .db
                    .bump_web_push_failures(&subscription.account_id, &subscription.endpoint)
                    .await
                {
                    Ok(Some(count)) => count,
                    // 端點已被別的流程移除或 DB 錯誤：以記憶體中的值 +1 估算，不阻擋流程。
                    _ => subscription.consecutive_failures + 1,
                };
                if failures >= STALE_SUBSCRIPTION_FAILURE_THRESHOLD {
                    let _ = self
                        .db
                        .delete_web_push_subscription(&subscription.account_id, &subscription.endpoint)
                        .await;
                    warn!(
                        account = %subscription.account_id,
                        failures,
                        error = %message,
                        "Web Push 連續失敗達門檻，訂閱已移除"
                    );
                    return SendOutcome::Removed(format!(
                        "連續 {failures} 次失敗（{}）",
                        short_reason(&message)
                    ));
                }
                SendOutcome::Failed(message)
            }
        }
    }

    /// Send a payload to every subscription, bounding concurrency with the
    /// global semaphore so a burst of notifications or many devices cannot
    /// flood the push service with parallel connections. Returns (success,
    /// failed) counts.
    async fn send_to_many(
        &self,
        subscriptions: Vec<WebPushSubscriptionRow>,
        payload: &[u8],
    ) -> (usize, usize) {
        // PUSH_HEALTH_V1: 同一批訂閱屬於同一個帳號，發送後記錄結果供管理員查看推播是否失效。
        let account_id = subscriptions.first().map(|sub| sub.account_id.clone());
        let total = subscriptions.len();
        // Fan out in parallel, each send holding one permit from SEND_SEMAPHORE.
        let futures = subscriptions.into_iter().map(|sub| {
            let payload = payload.to_vec();
            async move {
                let _permit = SEND_SEMAPHORE.acquire().await;
                self.send_one_tracked(&sub, &payload).await
            }
        });
        let results = futures::future::join_all(futures).await;
        let mut ok = 0usize;
        let mut failed = 0usize;
        let mut removed = 0usize;
        let mut first_error: Option<String> = None;
        let mut removal_reason: Option<String> = None;
        for result in results {
            match result {
                SendOutcome::Delivered => ok += 1,
                SendOutcome::Failed(error) => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(error.clone());
                    }
                    warn!(error = %error, "Web Push 單一裝置發送失敗");
                }
                SendOutcome::Removed(reason) => {
                    failed += 1;
                    removed += 1;
                    if first_error.is_none() {
                        first_error = Some(reason.clone());
                    }
                    if removal_reason.is_none() {
                        removal_reason = Some(reason);
                    }
                }
            }
        }
        if let Some(account_id) = account_id.as_deref() {
            let _ = self
                .db
                .record_web_push_health(account_id, ok, failed, first_error.as_deref())
                .await;
            // PUSH_STALE_CLEANUP_V1：有訂閱被移除 → 透過 Discord／LINE 提醒該使用者重新啟用。
            if removed > 0 {
                let remaining = total.saturating_sub(removed);
                let reason = removal_reason.unwrap_or_else(|| "推播服務拒絕".to_string());
                // PUSH_GONE_VISIBLE_V1：先寫進健康紀錄（推播狀態頁／管理員總覽會顯示黃色橫幅），
                // 再送 Discord 提醒；提醒有 12 小時冷卻，但狀態頁的標記會一直留到重新啟用。
                if let Err(error) = self
                    .db
                    .record_web_push_removed(account_id, removed, remaining, &reason)
                    .await
                {
                    warn!(account = %account_id, %error, "寫入推播訂閱移除紀錄失敗");
                }
                self.notify_subscription_expired(account_id, removed, remaining, reason)
                    .await;
            }
        }
        (ok, failed)
    }

    /// PUSH_STALE_CLEANUP_V1：以 Discord／LINE DM 通知帳號擁有者推播訂閱已失效。
    /// 走 `push_to_account_or_admin`，所以 Discord 的「其他通知」偏好關閉時不會打擾；
    /// 沒綁定 Discord/LINE 時會退回管理員（讓管理員知道某帳號收不到推播）。
    async fn notify_subscription_expired(
        &self,
        account_id: &str,
        removed_devices: usize,
        remaining_devices: usize,
        reason: String,
    ) {
        let Some(messenger) = EXPIRED_NOTICE_MESSENGER.get() else {
            debug!(account = %account_id, "未設定 messenger，略過推播失效提醒");
            return;
        };
        {
            let mut recent = EXPIRED_NOTICE_RECENT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            if let Some(&at) = recent.get(account_id) {
                if now.duration_since(at) < EXPIRED_NOTICE_COOLDOWN {
                    debug!(account = %account_id, "推播失效提醒在冷卻期內，略過");
                    return;
                }
            }
            if recent.len() >= MAX_DEDUPE_ENTRIES {
                recent.retain(|_, at| now.duration_since(*at) < EXPIRED_NOTICE_COOLDOWN);
            }
            recent.insert(account_id.to_string(), now);
        }
        let account = match self.db.get(account_id).await {
            Ok(Some(account)) => account,
            Ok(None) => {
                debug!(account = %account_id, "帳號不存在，略過推播失效提醒");
                return;
            }
            Err(error) => {
                warn!(account = %account_id, %error, "讀取帳號失敗，略過推播失效提醒");
                return;
            }
        };
        let dashboard_url = crate::config::try_get_config()
            .map(|config| config.adapters.discord.public_base_url.trim().trim_end_matches('/'))
            .filter(|base| !base.is_empty())
            .map(|base| format!("{base}/admin/push-status"))
            .unwrap_or_default();
        let event = OutboundMessage::PushSubscriptionExpired(PushSubscriptionExpiredEvent {
            account: account_id.to_string(),
            removed_devices,
            remaining_devices,
            reason,
            dashboard_url,
        });
        let target = AdapterAccountTarget::new(
            account.id.clone(),
            account.line_user_id.clone(),
            account.discord_user_id.clone(),
        );
        if let Err(error) = messenger.push_to_account_or_admin(&target, &event).await {
            warn!(account = %account_id, %error, "推播失效提醒發送失敗");
        } else {
            warn!(
                account = %account_id,
                removed_devices,
                remaining_devices,
                "已通知使用者手機推播訂閱失效"
            );
        }
    }
}

/// PUSH_STALE_CLEANUP_V1：從 send_one 的錯誤字串抽出 HTTP 狀態碼（格式固定為「HTTP <code>」）。
fn http_status_of(message: &str) -> Option<u16> {
    let idx = message.find("HTTP ")?;
    let digits: String = message[idx + 5..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// PUSH_STALE_CLEANUP_V1：把錯誤訊息縮成適合放進 DM 的一行（去掉回應內文）。
fn short_reason(message: &str) -> String {
    match http_status_of(message) {
        Some(code) => format!("HTTP {code}"),
        None => crate::text_utils::truncate_chars(message, 60).to_string(),
    }
}

fn decode_signing_key(value: &str) -> Result<SigningKey> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value.trim())
        .map_err(|error| miette::miette!("VAPID private key Base64 無效：{error}"))?;
    SigningKey::from_slice(&bytes)
        .map_err(|error| miette::miette!("VAPID private key 無效：{error}"))
}

fn valid_private_key(value: &str) -> bool {
    decode_signing_key(value).is_ok()
}

fn generate_private_key() -> Result<String> {
    for _ in 0..32 {
        let encoded = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        if valid_private_key(&encoded) {
            return Ok(encoded);
        }
    }
    Err(miette::miette!("無法產生有效的 VAPID private key"))
}

fn vapid_authorization(
    private_key: &str,
    public_key: &str,
    endpoint: &reqwest::Url,
) -> Result<String> {
    let host = endpoint
        .host_str()
        .ok_or_else(|| miette::miette!("Web Push endpoint 缺少 host"))?;
    let audience = match endpoint.port() {
        Some(port) => format!("{}://{host}:{port}", endpoint.scheme()),
        None => format!("{}://{host}", endpoint.scheme()),
    };
    // The decoded VAPID header must be valid JSON. Raw byte strings do not
    // escape their inner quotes; backslashes here make Apple reject every push.
    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "aud": audience,
            "exp": chrono::Utc::now().timestamp() + 12 * 60 * 60,
            "sub": "mailto:admin@example.com"
        }))
        .map_err(|error| miette::miette!("序列化 VAPID claims 失敗：{error}"))?,
    );
    let input = format!("{header}.{claims}");
    let signing_key = decode_signing_key(private_key)?;
    let signature: Signature = signing_key.sign(input.as_bytes());
    let token = format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    Ok(format!("vapid t={token}, k={public_key}"))
}

fn encrypt_payload(payload: &[u8], p256dh: &str, auth: &str) -> Result<Vec<u8>> {
    let receiver_public_bytes = URL_SAFE_NO_PAD
        .decode(p256dh.trim())
        .map_err(|error| miette::miette!("訂閱 p256dh 無效：{error}"))?;
    let receiver_public = PublicKey::from_sec1_bytes(&receiver_public_bytes)
        .map_err(|error| miette::miette!("訂閱 P-256 public key 無效：{error}"))?;
    let auth_secret = URL_SAFE_NO_PAD
        .decode(auth.trim())
        .map_err(|error| miette::miette!("訂閱 auth secret 無效：{error}"))?;

    let sender_secret = loop {
        let candidate = rand::random::<[u8; 32]>();
        if let Ok(secret) = SecretKey::from_slice(&candidate) {
            break secret;
        }
    };
    let sender_public = sender_secret.public_key().to_encoded_point(false);
    let shared = diffie_hellman(
        sender_secret.to_nonzero_scalar(),
        receiver_public.as_affine(),
    );

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&receiver_public_bytes);
    key_info.extend_from_slice(sender_public.as_bytes());
    let auth_hkdf = Hkdf::<Sha256>::new(Some(&auth_secret), shared.raw_secret_bytes());
    let mut ikm = [0u8; 32];
    auth_hkdf
        .expand(&key_info, &mut ikm)
        .map_err(|_| miette::miette!("Web Push IKM HKDF 失敗"))?;

    let salt = rand::random::<[u8; 16]>();
    let content_hkdf = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut content_key = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    content_hkdf
        .expand(b"Content-Encoding: aes128gcm\0", &mut content_key)
        .map_err(|_| miette::miette!("Web Push content key HKDF 失敗"))?;
    content_hkdf
        .expand(b"Content-Encoding: nonce\0", &mut nonce_bytes)
        .map_err(|_| miette::miette!("Web Push nonce HKDF 失敗"))?;

    let mut record = payload.to_vec();
    record.push(2); // RFC 8188 final-record delimiter.
    let cipher = Aes128Gcm::new_from_slice(&content_key)
        .map_err(|error| miette::miette!("建立 Web Push AES-GCM cipher 失敗：{error}"))?;
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce_bytes), record.as_ref())
        .map_err(|error| miette::miette!("加密 Web Push payload 失敗：{error}"))?;

    // RFC 8188 aes128gcm body: salt, record size, key-id length, key-id, record.
    let mut body = Vec::with_capacity(16 + 4 + 1 + sender_public.as_bytes().len() + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(sender_public.as_bytes().len() as u8);
    body.extend_from_slice(sender_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_extraction() {
        assert_eq!(http_status_of("Apple 推播服務回應 HTTP 410：gone"), Some(410));
        assert_eq!(http_status_of("Apple 推播服務回應 HTTP 403"), Some(403));
        assert_eq!(http_status_of("Web Push HTTP 發送失敗：timeout"), None);
        assert_eq!(short_reason("Apple 推播服務回應 HTTP 403：VapidPkHashMismatch"), "HTTP 403");
        assert!(short_reason("Web Push HTTP 發送失敗：連線逾時").chars().count() <= 60);
    }

    #[test]
    fn expired_event_render_plain_text() {
        let event = PushSubscriptionExpiredEvent {
            account: "u1".to_string(),
            removed_devices: 1,
            remaining_devices: 0,
            reason: "HTTP 410".to_string(),
            dashboard_url: String::new(),
        };
        let text = event.render();
        assert!(text.contains("手機推播訂閱已失效"));
        assert!(text.contains("完全收不到"));
        assert!(!text.chars().any(|c| (c as u32) >= 0x1F000), "不含表情符號");
    }
}
