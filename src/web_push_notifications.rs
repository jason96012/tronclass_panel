//! Account-scoped background Web Push notifications.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use miette::Result;
use once_cell::sync::{Lazy, OnceCell};
use serde_json::json;
use tracing::{debug, warn};
use web_push::{
    request_builder, ContentEncoding, SubscriptionInfo, VapidSignatureBuilder,
    WebPushMessageBuilder,
};

use crate::db::{AccountsDb, WebPushSubscriptionRow};

const VAPID_PRIVATE_KEY_SETTING: &str = "vapid_private_key_v1";
const MAX_DEDUPE_ENTRIES: usize = 4096;

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
    let private_key = match db.get_web_push_setting(VAPID_PRIVATE_KEY_SETTING).await? {
        Some(value) if valid_private_key(&value) => value,
        _ => {
            let value = generate_private_key()?;
            db.set_web_push_setting(VAPID_PRIVATE_KEY_SETTING, &value)
                .await?;
            value
        }
    };
    let partial = VapidSignatureBuilder::from_base64_no_sub(&private_key)
        .map_err(|error| miette::miette!("建立 VAPID key 失敗：{error}"))?;
    let public_key = URL_SAFE_NO_PAD.encode(partial.get_public_key());
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
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
    for subscription in subscriptions {
        service.send_one(&subscription, &payload).await?;
    }
    Ok(total)
}

/// Queue one account-owner notification. Repeated monitor passes for the same
/// rollcall are suppressed for two hours.
pub fn notify_rollcall(
    account_id: &str,
    rollcall_id: u64,
    course_name: &str,
    attendance_type: &str,
) {
    let Some(service) = SERVICE.get().cloned() else {
        return;
    };
    let key = format!("{account_id}:{rollcall_id}");
    {
        let mut recent = RECENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        recent.retain(|_, at| now.duration_since(*at) < Duration::from_secs(2 * 60 * 60));
        if recent.contains_key(&key) {
            return;
        }
        if recent.len() >= MAX_DEDUPE_ENTRIES {
            recent.clear();
        }
        recent.insert(key, now);
    }

    let account_id = account_id.to_string();
    let course_name = course_name.to_string();
    let attendance_type = attendance_type.to_string();
    tokio::spawn(async move {
        if let Err(error) = service
            .send_rollcall(&account_id, rollcall_id, &course_name, &attendance_type)
            .await
        {
            warn!(account = %account_id, rollcall_id, error = %error, "Web Push 點名通知失敗");
        }
    });
}

impl WebPushService {
    async fn send_rollcall(
        &self,
        account_id: &str,
        rollcall_id: u64,
        course_name: &str,
        attendance_type: &str,
    ) -> Result<()> {
        let subscriptions = self.db.list_web_push_subscriptions(account_id).await?;
        if subscriptions.is_empty() {
            return Ok(());
        }
        let payload = serde_json::to_vec(&json!({
            "title": format!("偵測到點名：{course_name}"),
            "body": format!("{attendance_type}｜帳號 {account_id}，點一下立即查看"),
            "url": "/admin/dashboard#rollcall",
            "tag": format!("tronclass-rollcall-{account_id}-{rollcall_id}"),
            "rollcall_id": rollcall_id,
        }))
        .map_err(|error| miette::miette!("序列化 Web Push payload 失敗：{error}"))?;

        for subscription in subscriptions {
            if let Err(error) = self.send_one(&subscription, &payload).await {
                warn!(account = %account_id, endpoint = %subscription.endpoint, error = %error, "Web Push 單一裝置發送失敗");
            }
        }
        Ok(())
    }

    async fn send_one(&self, subscription: &WebPushSubscriptionRow, payload: &[u8]) -> Result<()> {
        let info = SubscriptionInfo::new(
            subscription.endpoint.clone(),
            subscription.p256dh.clone(),
            subscription.auth.clone(),
        );
        let mut signature = VapidSignatureBuilder::from_base64(&self.private_key, &info)
            .map_err(|error| miette::miette!("建立 VAPID signature 失敗：{error}"))?;
        signature.add_claim("sub", "https://tronclass.local");
        let signature = signature
            .build()
            .map_err(|error| miette::miette!("簽署 VAPID 失敗：{error}"))?;
        let mut builder = WebPushMessageBuilder::new(&info);
        builder.set_payload(ContentEncoding::Aes128Gcm, payload);
        builder.set_vapid_signature(signature);
        let message = builder
            .build()
            .map_err(|error| miette::miette!("建立 Web Push 訊息失敗：{error}"))?;
        let request = request_builder::build_request::<Vec<u8>>(message);
        let mut outgoing = self
            .http
            .request(
                reqwest::Method::from_bytes(request.method().as_str().as_bytes())
                    .map_err(|error| miette::miette!("Web Push HTTP method 無效：{error}"))?,
                request.uri().to_string(),
            )
            .body(request.body().clone());
        for (name, value) in request.headers() {
            if let Ok(value) = value.to_str() {
                outgoing = outgoing.header(name.as_str(), value);
            }
        }
        let response = outgoing
            .send()
            .await
            .map_err(|error| miette::miette!("Web Push HTTP 發送失敗：{error}"))?;
        if response.status().is_success() {
            debug!(account = %subscription.account_id, "Web Push 已送達推播服務");
            return Ok(());
        }
        if matches!(response.status().as_u16(), 404 | 410) {
            let _ = self
                .db
                .delete_web_push_subscription(&subscription.account_id, &subscription.endpoint)
                .await;
        }
        Err(miette::miette!(
            "推播服務回應 HTTP {}",
            response.status().as_u16()
        ))
    }
}

fn valid_private_key(value: &str) -> bool {
    VapidSignatureBuilder::from_base64_no_sub(value).is_ok()
}

fn generate_private_key() -> Result<String> {
    for _ in 0..16 {
        let bytes = rand::random::<[u8; 32]>();
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        if valid_private_key(&encoded) {
            return Ok(encoded);
        }
    }
    Err(miette::miette!("無法產生有效的 VAPID private key"))
}
