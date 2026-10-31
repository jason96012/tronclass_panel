//! LINE Messaging API client.

use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{Local, TimeZone};
use hmac::{Hmac, Mac};
use miette::{IntoDiagnostic, Result, WrapErr};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::Sha256;
use tracing::{debug, error, warn};

use crate::adapters::events::{AdapterMessenger, MonitorStatus, OutboundMessage, StatusMessage};
use crate::config::LineBotConfig;

use super::types::{
    LineApiError, PushMessageRequest, ReplyMessageRequest, SendFlexMessage, SendMessage,
};

const LINE_REPLY_API: &str = "https://api.line.me/v2/bot/message/reply";
const LINE_PUSH_API: &str = "https://api.line.me/v2/bot/message/push";

/// LINE Bot API client.
#[derive(Clone)]
pub struct LineBotClient {
    http: Client,
    channel_access_token: String,
    channel_secret: String,
    admin_user_id: String,
}

impl LineBotClient {
    pub fn new(config: &LineBotConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .use_rustls_tls()
            .build()
            .into_diagnostic()
            .wrap_err("Failed to build Line Bot HTTP client")?;

        Ok(Self {
            http,
            channel_access_token: config.channel_access_token.clone(),
            channel_secret: config.channel_secret.clone(),
            admin_user_id: config.admin_user_id.clone(),
        })
    }

    pub fn verify_signature(&self, body: &[u8], signature: &str) -> bool {
        let sig_bytes = match BASE64.decode(signature) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(error = %e, "X-Line-Signature Base64 decode 失敗");
                return false;
            }
        };

        let mut mac = match Hmac::<Sha256>::new_from_slice(self.channel_secret.as_bytes()) {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, "HMAC 初始化失敗（channel_secret 可能無效）");
                return false;
            }
        };

        mac.update(body);
        let computed = mac.finalize().into_bytes();

        constant_time_compare(&computed, &sig_bytes)
    }

    #[allow(dead_code)]
    pub async fn reply_text(&self, reply_token: &str, text: &str) -> Result<()> {
        let req = ReplyMessageRequest::text(reply_token, text);

        debug!(
            // REDIRECT_UTF8_SLICE_V1：token 雖然實務上是 ASCII，仍統一改用
            // 字元截斷，避免未來內容改變時留下 panic 隱患。
            reply_token = %crate::text_utils::truncate_chars(&reply_token, 10),
            text_len = text.len(),
            "發送 Reply 訊息"
        );

        self.send_reply(req).await
    }

    async fn send_reply(&self, req: ReplyMessageRequest) -> Result<()> {
        let resp = self
            .http
            .post(LINE_REPLY_API)
            .bearer_auth(&self.channel_access_token)
            .json(&req)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("Failed to send Line reply")?;

        handle_line_api_response(resp, "reply").await
    }

    #[allow(dead_code)]
    pub async fn push_message(&self, to: &str, text: &str) -> Result<()> {
        let req = PushMessageRequest::text(to, text);

        debug!(
            // REDIRECT_UTF8_SLICE_V1：統一改用字元截斷。
            to = %crate::text_utils::truncate_chars(to, 10),
            text_len = text.len(),
            "發送 Push 訊息"
        );

        self.send_push(req).await
    }

    async fn send_push(&self, req: PushMessageRequest) -> Result<()> {
        let resp = self
            .http
            .post(LINE_PUSH_API)
            .bearer_auth(&self.channel_access_token)
            .json(&req)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("Failed to send Line push message")?;

        handle_line_api_response(resp, "push").await
    }

    pub fn admin_user_id(&self) -> &str {
        &self.admin_user_id
    }
}

#[async_trait]
impl AdapterMessenger for LineBotClient {
    fn adapter_name(&self) -> &'static str {
        "line"
    }

    fn admin_user_id(&self) -> &str {
        self.admin_user_id()
    }

    async fn reply(&self, reply_token: &str, message: &OutboundMessage) -> Result<()> {
        let messages = render_line_messages(message);
        let req = ReplyMessageRequest::messages(reply_token, messages);
        self.send_reply(req).await
    }

    async fn push(&self, to: &str, message: &OutboundMessage) -> Result<()> {
        let messages = render_line_messages(message);
        let req = PushMessageRequest::messages(to, messages);
        self.send_push(req).await
    }
}

fn render_line_messages(message: &OutboundMessage) -> Vec<SendMessage> {
    match message {
        OutboundMessage::SystemStarted(event) => vec![flex_bubble(
            "Tronclass Rollcall 已啟動",
            "Tronclass Rollcall 已啟動",
            "#16A34A",
            vec![
                text_row("帳號", &event.account),
                text_row("使用者", &event.user_name),
                text_row("輪詢間隔", format!("{} 秒", event.poll_interval_secs)),
                text_row("Adapter", &event.adapter_name),
            ],
            vec![
                postback_action("查看狀態", "/status"),
                postback_action("立即檢查", "/force"),
                postback_action("說明", "/help"),
            ],
        )],
        OutboundMessage::RollcallDetected(event) => vec![flex_bubble(
            format!("偵測到新簽到：{}", event.course_name),
            "偵測到新簽到",
            "#2563EB",
            vec![
                text_row("課程", &event.course_name),
                text_row("教師", &event.teacher_name),
                text_row("類型", &event.attendance_type),
                text_row("點名 ID", event.rollcall_id.to_string()),
                text_row("帳號", &event.account),
            ],
            vec![postback_action("查看狀態", "/status")],
        )],
        OutboundMessage::QrCodeRequested(request) => vec![flex_bubble(
            format!("QR Code 簽到：{}", request.course_name),
            "需要 QR Code 簽到",
            "#F59E0B",
            vec![
                text_row("課程", &request.course_name),
                text_row("教師", &request.teacher_name),
                text_row("點名 ID", request.rollcall_id.to_string()),
                text_row("帳號", &request.account),
                text_row("期限", format!("{} 秒內回覆", request.timeout_secs)),
            ],
            vec![
                uri_action("開啟掃碼頁", &request.scan_url),
                postback_action("查看狀態", "/status"),
            ],
        )],
        OutboundMessage::RollcallFinished(event) => {
            let (header, color) = if event.success {
                ("簽到成功", "#16A34A")
            } else {
                ("簽到未成功", "#DC2626")
            };
            vec![flex_bubble(
                format!("{header}：{}", event.course_name),
                header,
                color,
                vec![
                    text_row("課程", &event.course_name),
                    text_row("類型", &event.attendance_type),
                    text_row("點名 ID", event.rollcall_id.to_string()),
                    text_row("帳號", &event.account),
                    text_row("結果", &event.result),
                    text_row("耗時", format!("{} ms", event.elapsed_ms)),
                ],
                vec![postback_action("查看狀態", "/status")],
            )]
        }
        OutboundMessage::Status(status) => render_status_messages(status),
        OutboundMessage::Help => vec![help_flex("使用說明", "Tronclass Rollcall 指令")],
        OutboundMessage::Welcome => vec![help_flex("歡迎使用", "Tronclass Rollcall")],
        _ => vec![SendMessage::text(render_message(message))],
    }
}

fn render_message(message: &OutboundMessage) -> String {
    match message {
        OutboundMessage::Text(text) => text.clone(),
        OutboundMessage::SecurityAlert(event) => format!(
            "⚠️ Dashboard 安全警示\n\n連續密碼錯誤\n帳號：{}\n來源 IP：{}\n帳號失敗次數：{}\nIP 失敗次數：{}\n時間：{}\n{}請由 Dashboard 黑名單頁面封鎖此 IP：\nhttps://dashboard.jasoncloudserver.me/admin/blacklist",
            event.username,
            event.ip,
            event.user_failures,
            event.ip_failures,
            event.timestamp,
            // LOGIN_TIERED_LOCKOUT_V1：LINE 沒有互動按鈕，只補一行狀態說明。
            if event.ip_is_blacklisted {
                "此 IP 已自動加入黑名單。\n"
            } else if event.login_locked {
                "此帳號的密碼登入已暫時鎖定 10 分鐘（通行密鑰不受影響）。\n"
            } else {
                ""
            },
        ),
        // ADMIN_GEO_RESTRICT_V1：這則一律只送管理員 Discord ID，LINE 端理論上
        // 不會真的送達（hub 會對每個 adapter 都呼叫一次 push，LINE 收到不屬於
        // 自己的 user id 會送失敗，由呼叫端吞掉錯誤）；仍補上文字版避免 match
        // 漏 arm，且 LINE 沒有互動按鈕，只能請管理員改用 Discord 或網頁處理。
        OutboundMessage::GeoAccessRequest(event) => format!(
            "⚠️ 國外 IP 嘗試存取管理後台\n\n來源 IP：{}\n位置：{}\n時間：{}\n\n請改用 Discord 或 /admin/blacklist 頁面處理（暫時放行／忽略）。",
            event.ip, event.location, event.timestamp,
        ),
        OutboundMessage::SystemStarted(event) => format!(
            "🚀 Tronclass Rollcall 已啟動\n\
             帳號：{} / {}\n\
             輪詢間隔：{} 秒\n\
             Adapter：{}\n\n\
             輸入 /help 查看可用指令",
            event.account, event.user_name, event.poll_interval_secs, event.adapter_name,
        ),
        OutboundMessage::RollcallDetected(event) => format!(
            "📋 偵測到新簽到\n\
             帳號：{}\n\
             課程：{}\n\
             教師：{}\n\
             類型：{}\n\
             點名 ID：{}\n\
             開始自動簽到...",
            event.account,
            event.course_name,
            event.teacher_name,
            event.attendance_type,
            event.rollcall_id,
        ),
        OutboundMessage::NumberCodeRequested(request) => {
            let page = if request.input_url.trim().is_empty() {
                "".to_string()
            } else {
                format!("\n數字點名頁：{}", request.input_url)
            };
            format!(
                "🔢 需要數字點名碼\n\
                 課程：{}\n\
                 帳號：{}\n\
                 教師：{}\n\
                 點名 ID：{}\n\
                 \n\
                 預設不會自動讀取代碼。\n\
                 請在 {} 秒內回覆 4 位數字，或開啟數字點名頁。{}",
                request.course_name,
                request.account,
                request.teacher_name,
                request.rollcall_id,
                request.timeout_secs,
                page,
            )
        }
        OutboundMessage::QrCodeRequested(request) => format!(
            "📷 需要 QR Code 簽到\n\
             課程：{}\n\
             帳號：{}\n\
             教師：{}\n\
             點名 ID：{}\n\
             \n\
             請到以下連結掃描 QR Code，\n\
             然後將掃描結果（URL）傳送到此對話：\n\
             {}\n\
             \n\
             ⏰ 請在 {} 秒內回覆，否則簽到逾時",
            request.course_name,
            request.account,
            request.teacher_name,
            request.rollcall_id,
            request.scan_url,
            request.timeout_secs,
        ),
        OutboundMessage::RollcallFinished(event) => {
            let emoji = if event.success { "✅" } else { "❌" };
            let title = if event.success {
                "你已完成點名"
            } else {
                "點名尚未完成"
            };
            format!(
                "{emoji} {title}\n\
                 帳號：{}\n\
                 課程：{}\n\
                 類型：{}\n\
                 點名 ID：{}\n\
                 結果：{}\n\
                 耗時：{}ms",
                event.account,
                event.course_name,
                event.attendance_type,
                event.rollcall_id,
                event.result,
                event.elapsed_ms,
            )
        }
        OutboundMessage::Help => help_text().to_string(),
        OutboundMessage::Welcome => format!(
            "👋 歡迎使用 Tronclass Rollcall！\n\n我會自動幫你完成 Tronclass 簽到。\n\n{}",
            help_text()
        ),
        OutboundMessage::UnsupportedMedia => "⚠️ 不支援媒體訊息，請傳送文字指令。".to_string(),
        OutboundMessage::LocationReceived {
            latitude,
            longitude,
        } => format!(
            "📍 收到位置訊息：\n緯度：{latitude:.6}\n經度：{longitude:.6}\n\n（位置功能尚未實作）"
        ),
        OutboundMessage::Status(status) => render_status_message(status),
        OutboundMessage::NotAuthorized => {
            "⚠️ 你可以查詢自己的帳號狀態，但此操作僅限管理員使用。".to_string()
        }
        OutboundMessage::MonitorPaused => {
            "⏸️ 簽到監控已暫停。\n輸入 /start 或 啟動 來恢復。".to_string()
        }
        OutboundMessage::MonitorResumed => "▶️ 簽到監控已恢復，立即執行一次簽到檢查...".to_string(),
        OutboundMessage::ForcePollTriggered => "🔄 已觸發立即簽到檢查，請稍候...".to_string(),
        OutboundMessage::ReauthTriggered => "🔐 已觸發重新認證，請稍候...".to_string(),
        OutboundMessage::QrAccepted => "✅ 已收到 QR Code，正在嘗試簽到，請稍候...".to_string(),
        OutboundMessage::QrAmbiguousTarget => {
            "⚠️ 多帳號模式下 QR Code 目標不明，請由綁定該帳號的使用者傳送 QR Code。".to_string()
        }
        OutboundMessage::QrNoBoundAccount => {
            "⚠️ 找不到與你的帳號綁定。請確認帳號資料中的 user_id 是否正確。".to_string()
        }
        OutboundMessage::QrNoPendingRequest => {
            "⚠️ 無法傳送 QR Code：目前沒有等待 QR Code 的簽到，或簽到已逾時。".to_string()
        }
        OutboundMessage::HomeworkDeadline(event) => event.render(),
        OutboundMessage::Bulletin(event) => event.render(),
        OutboundMessage::PushSubscriptionExpired(event) => event.render(),
        OutboundMessage::RollcallInputReminder(event) => event.render(),
        // DISCORD_NOTIFY_UNIFY_V1：LINE 維持純文字，用事件自帶的 render()。
        OutboundMessage::MonitorEvent(event) => event.render(),
        OutboundMessage::AdminAlert(event) => event.render(),
        OutboundMessage::ClassReminder(event) => event.render(),
        OutboundMessage::TimetableChanged(event) => event.render(),
        OutboundMessage::UnknownCommand { text } => format!(
            "❓ 不認識的指令：{}\n\n輸入 /help 或 幫助 查看可用指令。",
            // REDIRECT_UTF8_SLICE_V1：這裡截的是使用者輸入的文字，長中文訊息
            // （>50 bytes ≈ 16 個中文字）在舊寫法下會直接 panic；改字元截斷。
            crate::text_utils::truncate_chars(text, 50)
        ),
    }
}

fn render_status_messages(message: &StatusMessage) -> Vec<SendMessage> {
    match message {
        StatusMessage::NoAccounts => vec![SendMessage::text(render_status_message(message))],
        StatusMessage::Single(status) => vec![status_flex(
            "系統狀態",
            None,
            status,
            vec![
                postback_action("立即檢查", "/force"),
                postback_action("重新認證", "/reauth"),
            ],
        )],
        StatusMessage::UserAccount { account_id, status } => vec![status_flex(
            "你的帳號狀態",
            Some(account_id),
            status,
            vec![postback_action("立即檢查", "/force")],
        )],
        StatusMessage::AdminAccounts(_) => vec![SendMessage::text(render_status_message(message))],
    }
}

fn render_status_message(message: &StatusMessage) -> String {
    match message {
        StatusMessage::NoAccounts => "📊 系統狀態\n目前沒有可查詢的 Tronclass 帳號。".to_string(),
        StatusMessage::Single(status) => render_monitor_status(status),
        StatusMessage::UserAccount { account_id, status } => format!(
            "📊 你的 Tronclass 帳號狀態\n帳號 ID：{}\n{}",
            account_id,
            render_monitor_status(status)
        ),
        StatusMessage::AdminAccounts(accounts) => {
            let mut lines = vec![format!("📊 系統狀態（{} 個帳號）", accounts.len())];
            for account in accounts {
                let status = &account.status;
                let running = if status.is_running {
                    "運行中"
                } else {
                    "已暫停"
                };
                let last_poll = status
                    .last_poll_timestamp
                    .map(format_unix_time)
                    .unwrap_or_else(|| "尚未輪詢".to_string());
                let last_success = status.last_success_course.as_deref().unwrap_or("無");
                lines.push(format!(
                    "\n帳號：{}\n使用者：{}\n狀態：{}\n最後輪詢：{}\n最後成功：{}\n連續失敗：{} 次",
                    account.account_id,
                    status.user_name,
                    running,
                    last_poll,
                    last_success,
                    status.consecutive_failures,
                ));
            }
            lines.join("\n")
        }
    }
}

fn flex_bubble(
    alt_text: impl Into<String>,
    header: impl Into<String>,
    status_color: &str,
    rows: Vec<Value>,
    actions: Vec<Value>,
) -> SendMessage {
    let mut contents = json!({
        "type": "bubble",
        "size": "mega",
        "header": {
            "type": "box",
            "layout": "vertical",
            "backgroundColor": status_color,
            "paddingAll": "16px",
            "contents": [
                {
                    "type": "text",
                    "text": header.into(),
                    "weight": "bold",
                    "size": "lg",
                    "color": "#FFFFFF",
                    "wrap": true
                }
            ]
        },
        "body": {
            "type": "box",
            "layout": "vertical",
            "spacing": "md",
            "paddingAll": "18px",
            "contents": rows
        }
    });

    if !actions.is_empty() {
        contents["footer"] = json!({
            "type": "box",
            "layout": "vertical",
            "spacing": "sm",
            "paddingAll": "14px",
            "contents": actions
        });
    }

    SendMessage::Flex(SendFlexMessage {
        alt_text: alt_text.into(),
        contents,
    })
}

fn text_row(label: impl Into<String>, value: impl Into<String>) -> Value {
    json!({
        "type": "box",
        "layout": "baseline",
        "spacing": "sm",
        "contents": [
            {
                "type": "text",
                "text": label.into(),
                "size": "sm",
                "color": "#6B7280",
                "flex": 2
            },
            {
                "type": "text",
                "text": value.into(),
                "size": "sm",
                "color": "#111827",
                "wrap": true,
                "flex": 5
            }
        ]
    })
}

fn uri_action(label: impl Into<String>, uri: impl Into<String>) -> Value {
    json!({
        "type": "button",
        "style": "primary",
        "height": "sm",
        "color": "#2563EB",
        "action": {
            "type": "uri",
            "label": label.into(),
            "uri": uri.into()
        }
    })
}

fn postback_action(label: impl Into<String>, data: impl Into<String>) -> Value {
    let data = data.into();
    json!({
        "type": "button",
        "style": "secondary",
        "height": "sm",
        "action": {
            "type": "postback",
            "label": label.into(),
            "data": data.clone(),
            "displayText": data
        }
    })
}

fn status_flex(
    header: &str,
    account_id: Option<&str>,
    status: &MonitorStatus,
    actions: Vec<Value>,
) -> SendMessage {
    let running = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };
    let last_poll = status
        .last_poll_timestamp
        .map(format_unix_time)
        .unwrap_or_else(|| "尚未輪詢".to_string());
    let last_success = status.last_success_course.as_deref().unwrap_or("無");
    let color = if status.is_running {
        "#16A34A"
    } else {
        "#6B7280"
    };

    let mut rows = vec![
        text_row("狀態", running),
        text_row("使用者", &status.user_name),
        text_row("最後輪詢", last_poll),
        text_row("最後成功", last_success),
        text_row("連續失敗", format!("{} 次", status.consecutive_failures)),
    ];

    if let Some(account_id) = account_id {
        rows.insert(1, text_row("帳號 ID", account_id));
    }

    flex_bubble(format!("{header}：{running}"), header, color, rows, actions)
}

fn help_flex(alt_text: &str, header: &str) -> SendMessage {
    flex_bubble(
        alt_text,
        header,
        "#2563EB",
        vec![
            text_row("/status", "查看目前監控狀態"),
            text_row("/start", "啟動簽到監控"),
            text_row("/stop", "暫停簽到監控"),
            text_row("/force", "立即觸發一次簽到檢查"),
            text_row("/reauth", "重新登入"),
            text_row("QR Code", "直接貼上掃描到的 URL 或 p 參數"),
        ],
        vec![
            postback_action("查看狀態", "/status"),
            postback_action("立即檢查", "/force"),
            postback_action("說明", "/help"),
        ],
    )
}

fn render_monitor_status(status: &MonitorStatus) -> String {
    let status_emoji = if status.is_running { "✅" } else { "⏸️" };
    let status_text = if status.is_running {
        "運行中"
    } else {
        "已暫停"
    };

    let last_poll = status
        .last_poll_timestamp
        .map(format_unix_time)
        .unwrap_or_else(|| "尚未輪詢".to_string());

    let last_success = status.last_success_course.as_deref().unwrap_or("無");

    format!(
        "📊 系統狀態\n\
         狀態：{status_emoji} {status_text}\n\
         帳號：{}\n\
         最後輪詢：{last_poll}\n\
         最後成功：{last_success}\n\
         連續失敗：{} 次",
        status.user_name, status.consecutive_failures,
    )
}

fn format_unix_time(timestamp: i64) -> String {
    match Local.timestamp_opt(timestamp, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S %Z").to_string(),
        None => format!("{timestamp}"),
    }
}

fn help_text() -> &'static str {
    "📚 可用指令：\n\
     \n\
     /status - 查看目前監控狀態\n\
     /start  - 啟動簽到監控\n\
     /stop   - 暫停簽到監控\n\
     /force  - 立即觸發一次簽到檢查\n\
     /reauth - 重新登錄（Session 過期時使用）\n\
     /help   - 顯示此說明\n\
     \n\
     💡 當有 QR Code 簽到時，\n\
     直接貼上掃描到的 URL 或 p 參數即可"
}

impl std::fmt::Debug for LineBotClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineBotClient")
            .field("admin_user_id", &self.admin_user_id)
            .field("channel_access_token", &"<redacted>")
            .field("channel_secret", &"<redacted>")
            .finish()
    }
}

async fn handle_line_api_response(resp: reqwest::Response, api_name: &str) -> Result<()> {
    let status = resp.status();

    if status.is_success() {
        debug!(api = api_name, status = %status, "Line API 呼叫成功");
        return Ok(());
    }

    let body = resp.text().await.unwrap_or_default();

    if let Ok(err) = serde_json::from_str::<LineApiError>(&body) {
        Err(miette::miette!(
            "Line {} API 失敗（HTTP {}）：{}",
            api_name,
            status,
            err
        ))
    } else {
        Err(miette::miette!(
            "Line {} API 失敗（HTTP {}）：{}",
            api_name,
            status,
            // REDIRECT_UTF8_SLICE_V1：LINE API 錯誤回應常含中文，改用字元截斷。
            crate::text_utils::truncate_chars(&body, 200)
        ))
    }
}

fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        let dummy = &vec![0u8; a.len()][..];
        let _ = constant_time_compare_inner(a, dummy);
        return false;
    }
    constant_time_compare_inner(a, b)
}

fn constant_time_compare_inner(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

