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
        if matches!(status.as_u16(), 404 | 410) {
            let _ = self
                .db
                .delete_web_push_subscription(&subscription.account_id, &subscription.endpoint)
                .await;
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
