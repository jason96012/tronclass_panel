//! AI 後端（Turn 8 追加後）：`/admin/ai` 頁面的靜態資源，以及公告 AI 摘要。
//!
//! - 一般聊天、串流、Agent 工具、對話串與金鑰設定路由已移除；AI 頁面只用 opencode（見 `opencode.rs`）。
//! - 公告摘要（`summarize_handler`）建立背景 job，前端用 `chat_status_handler` 輪詢。
//!   反向代理（Cloudflare Tunnel）會對長時間沒有回應的請求回空 502，所以只回 202。
//! - 摘要與通知預覽都呼叫 `opencode::summary_complete`，使用 opencode 設定的 NVIDIA 金鑰。

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    admin_login_redirect, html_page_response, must_change_password_redirect, session_token,
    unauthorized_response, AdminQuery, AdminWebState,
};

const PAGE_HTML: &str = include_str!("static/ai.html");

/// AI_CHAT_RATE_LIMIT_V1（fix121）：每帳號每分鐘可建立的 AI 工作數上限。
/// 公告摘要會呼叫 NVIDIA（共用 opencode 的金鑰）。沒有這條，任一 session 外洩或
/// 不當狂刷，都能瞬間燒光上游 API 額度（財務型 DoS）。
const AI_CHAT_MAX_PER_MINUTE: u32 = 12;

/// 完成的 job 保留多久（供前端最後一次輪詢取結果）。
const AI_JOB_TTL_SECS: u64 = 600;
/// 同時保留的 job 數上限。
const AI_JOB_MAX: usize = 64;

/// BULLETIN_AI_SUMMARY_V1：把單則學校公告濃縮成「字數精簡但重點不漏」的摘要（條列）。
const BULLETIN_SUMMARY_MAX_TOKENS: u32 = 1600;
/// BULLETIN_NOTIFY_SUMMARY_V2：通知預覽用的摘要。要比條列版更短 ——
/// 像手機／Discord 通知預覽那樣，「一到兩句」就讓人看懂重點。
const BULLETIN_PREVIEW_MAX_TOKENS: u32 = 900;
/// 送進摘要模型的上游來源文字上限（位元組）。公告多半不長，但設上限以免錯字溢出。
const BULLETIN_SUMMARY_MAX_SOURCE: usize = 40_000;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 背景 job（公告摘要）：生成可能要幾十秒，若讓 HTTP 請求一直掛著，
/// Cloudflare Tunnel 會直接回一個空的 502，所以改用背景 job + 輪詢。
struct ChatJob {
    account: String,
    thread_id: String,
    /// 目前已生成的文字（前端每次輪詢都拿全文，直接覆蓋渲染）。
    text: String,
    done: bool,
    ok: bool,
    /// 保留欄位（停止功能已移除），恆為 false。
    cancel: bool,
    error: String,
    code: String,
    key_name: String,
    model: String,
    /// 保留欄位（自動選模型已移除），恆為空。
    auto_reason: String,
    /// 保留欄位（快取已移除），恆為 false。
    cached: bool,
    /// 保留欄位（快取已移除），恆為空。
    cache_key: String,
    /// 輪替過程中每把密鑰的結果，失敗時攤開給使用者看。
    attempts: Vec<Value>,
    /// 保留欄位（Agent 已移除），恆為空。
    steps: Vec<Value>,
    created_at: u64,
    updated_at: u64,
}

impl ChatJob {
    fn to_json(&self) -> Value {
        json!({
            "state": if !self.done { "running" } else if self.ok { "done" } else { "failed" },
            "done": self.done,
            "ok": self.ok,
            "text": self.text,
            "thread_id": self.thread_id,
            "key_name": self.key_name,
            "model": self.model,
            "auto_reason": self.auto_reason,
            "cached": self.cached,
            "error": self.error,
            "code": self.code,
            "attempts": self.attempts,
            "steps": self.steps,
            "elapsed_secs": now_secs().saturating_sub(self.created_at),
        })
    }
}

static CHAT_JOBS: once_cell::sync::Lazy<
    std::sync::Mutex<std::collections::HashMap<String, ChatJob>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn job_insert(job_id: &str, job: ChatJob) {
    if let Ok(mut map) = CHAT_JOBS.lock() {
        let now = now_secs();
        map.retain(|_, j| now.saturating_sub(j.updated_at) <= AI_JOB_TTL_SECS);
        while map.len() >= AI_JOB_MAX {
            let oldest = map
                .iter()
                .min_by_key(|(_, j)| j.updated_at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    map.remove(&key);
                }
                None => break,
            }
        }
        map.insert(job_id.to_string(), job);
    }
}

fn job_update<T>(job_id: &str, apply: impl FnOnce(&mut ChatJob) -> T) -> Option<T> {
    let mut map = CHAT_JOBS.lock().ok()?;
    let job = map.get_mut(job_id)?;
    let out = apply(job);
    job.updated_at = now_secs();
    Some(out)
}

/// 摘要用的 system 提示：只保留重點、不臆測、條列、精簡。
fn bulletin_summary_instruction() -> String {
    "你是學校公告的摘要助手。請把使用者給的公告濃縮成重點摘要。\
     要求：1) 用繁體中文。2) 用條列式，每點一行，最多 5 點。\
     3) 務必保留關鍵資訊：日期與截止期限、地點、需要採取的行動、重要規定、政策變更、\
     需回覆或參與的項目。4) 不要加入原文沒有的資訊或推測。\
     5) 整份精簡（約 100–180 字），但重點不可遺漏。".to_string()
}

/// 通知預覽用的摘要指令：像手機／Discord 的通知預覽一樣，
/// 只要一到兩句、抓到最關鍵的重點即可，不要條列。
fn bulletin_preview_instruction() -> String {
    "你是學校公告的摘要助手。請把使用者給的公告寫成「通知預覽」一句話。\
     要求：1) 用繁體中文。2) 一到兩句（約 20–45 字），不要列點、不要換行。\
     3) 只抓最關鍵的訊息：日期與截止期限、需要採取的行動、地點、或重要變更。\
     4) 不要加入原文沒有的資訊或推測。\
     5) 讀起來要像手機推播或 Discord 通知預覽那樣一目了然。".to_string()
}

async fn run_bulletin_summary_job(job_id: String, text: String) {
    let messages = vec![
        json!({ "role": "system", "content": bulletin_summary_instruction() }),
        json!({ "role": "user", "content": text }),
    ];
    let result = super::opencode::summary_complete(messages, BULLETIN_SUMMARY_MAX_TOKENS).await;
    let _ = job_update(&job_id, |job| {
        job.done = true;
        match result {
            Ok((summary, model)) => {
                job.ok = true;
                job.text = summary;
                job.key_name = super::opencode::summary_label().to_string();
                job.model = model;
            }
            Err(e) => {
                job.ok = false;
                job.code = "AI_SUMMARY_FAILED".to_string();
                job.error = e;
            }
        }
    });
}

/// POST /admin/api/ai/summarize 的請求本體。
#[derive(Debug, Deserialize)]
pub(super) struct SummarizeRequest {
    #[serde(default)]
    token: String,
    /// 要摘要的公告全文（通常含標題與正文）。由前端先轉成純文字再送。
    text: String,
}

/// 送出公告 → 建立背景摘要 job → 立刻回 202 與 job_id（前端再輪詢 chat/status）。
pub(super) async fn summarize_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Json(body): Json<SummarizeRequest>,
) -> Response {
    let token = session_token(&headers, &body.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    // AI_CHAT_RATE_LIMIT_V1（fix121）：每帳號每分鐘摘要工作數上限，
    // 防共用的 NVIDIA 金鑰被狂刷燒額度（財務型 DoS）。
    if !state
        .rate_limiter
        .check_limit_memory(
            &format!("ai-chat:{}", session.account_id),
            AI_CHAT_MAX_PER_MINUTE,
        )
        .await
    {
        tracing::warn!(account = %session.account_id, "AI summarize 觸發每帳號限流");
        return super::rate_limited_response();
    }

    let text = body.text.trim().to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "沒有可摘要的內容", "code": "AI_EMPTY_CONTENT" })),
        )
            .into_response();
    }
    if text.len() > BULLETIN_SUMMARY_MAX_SOURCE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("公告太長（上限 {} 位元組），無法摘要", BULLETIN_SUMMARY_MAX_SOURCE),
                "code": "AI_SUMMARY_TOO_LONG",
            })),
        )
            .into_response();
    }

    // 先確認有 NVIDIA 金鑰（與 opencode 共用），才建 job。
    if !super::opencode::summary_key_configured() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "尚未設定 NVIDIA 金鑰。請到 AI 頁面的 opencode 分頁填入 API Key，公告摘要會共用這把金鑰。",
                "code": "NO_KEY_AVAILABLE",
            })),
        )
            .into_response();
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    job_insert(
        &job_id,
        ChatJob {
            account: session.account_id,
            thread_id: String::new(),
            text: String::new(),
            done: false,
            ok: false,
            cancel: false,
            error: String::new(),
            code: String::new(),
            key_name: String::new(),
            model: String::new(),
            auto_reason: String::new(),
            cached: false,
            cache_key: String::new(),
            attempts: Vec::new(),
            steps: Vec::new(),
            created_at: now_secs(),
            updated_at: now_secs(),
        },
    );

    let job_id2 = job_id.clone();
    tokio::spawn(async move {
        run_bulletin_summary_job(job_id2, text).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "job_id": job_id,
            "state": "running",
            "poll_path": "/admin/api/ai/chat/status",
            "poll_ms": 600,
        })),
    )
        .into_response()
}

// BULLETIN_NOTIFY_SUMMARY_V1
// 供「公告通知」在背景生成本公告的重點摘要（精簡、保留關鍵資訊），與 summarize_handler 共用
// opencode 的 NVIDIA 金鑰。回傳 None 代表摘要無法生成（無金鑰或上游失敗），呼叫端退回標題。
pub(crate) async fn summarize_bulletin_text(text: String) -> Option<String> {
    let messages = vec![
        json!({ "role": "system", "content": bulletin_preview_instruction() }),
        json!({ "role": "user", "content": text }),
    ];
    // BULLETIN_NOTIFY_SUMMARY_V2：通知預覽用較短的 token 上限，逼模型寫短句。
    match super::opencode::summary_complete(messages, BULLETIN_PREVIEW_MAX_TOKENS).await {
        Ok((summary, _)) if !summary.trim().is_empty() => Some(summary),
        _ => None,
    }
}

pub(super) async fn page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：access 過期但 refresh 有效時由伺服器端輪換，不再閃登入頁。
    let Some(page) = super::pages::resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/ai");
    };
    if page.session.must_change_password {
        return super::pages::attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    super::pages::attach_fresh_cookies(
        html_page_response(&headers, super::pages::inject_auth_attrs(&super::pages::inject_theme(PAGE_HTML), &page.session)),
        &page,
        &headers,
    )
}

/// AI_PAGE_LAYOUT_V2：AI 頁面的獨立樣式與腳本。
///
/// 為什麼不用 inline：CSP 的 `style-src 'self'`／`script-src 'self'` 不接受
/// 任意 inline 樣式；頁面原本的 `<style>` 與 `style="…"` 屬性在嚴格模式下會被擋。
/// 因此檔案全部放在 `/admin/static/ai/…`，走和其他面板資源一樣的快取與
/// 版號改寫，並且 `/admin/static/` 不需登入（與其他面板資源相同）。
const AI_STATIC_OPENCODE_CSS: &str = include_str!("static/opencode.css");
const AI_STATIC_JS: &[(&str, &str)] = &[
    ("state.js", include_str!("static/js/state.js")),
    ("api.js", include_str!("static/js/api.js")),
    ("markdown.js", include_str!("static/js/markdown.js")),
    ("composer.js", include_str!("static/js/composer.js")),
    ("timeline.js", include_str!("static/js/timeline.js")),
    ("panels.js", include_str!("static/js/panels.js")),
    ("command-palette.js", include_str!("static/js/command-palette.js")),
    ("keyboard.js", include_str!("static/js/keyboard.js")),
    ("opencode.js", include_str!("static/js/opencode.js")),
    ("icons.js", include_str!("static/js/icons.js")),
];

/// GET /admin/static/ai/{*path}：只服務上面列出的檔案，其他一律 404。
pub(super) async fn ai_static_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    if path == "opencode.css" {
        return super::static_assets::admin_static_asset(
            "text/css; charset=utf-8",
            AI_STATIC_OPENCODE_CSS,
        );
    }
    if let Some(name) = path.strip_prefix("js/") {
        if let Some((_, body)) = AI_STATIC_JS.iter().find(|(file, _)| *file == name) {
            // AI_MODULE_VERSION_V1：相對 import 也要帶 ?sv=，否則會跨版本吃到舊模組。
            let body = super::asset_versions::rewrite_module_imports(body);
            return super::static_assets::admin_static_asset(
                "application/javascript; charset=utf-8",
                &body,
            );
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

/// 公告摘要 job 輪詢的查詢參數（`?job=`）。
#[derive(Debug, Deserialize)]
pub(super) struct JobQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    job: String,
}

/// 公告摘要 job 的輪詢端點：回傳目前狀態與完成後的結果。
pub(super) async fn chat_status_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<JobQuery>,
) -> Response {
    let token = session_token(&headers, &query.token);
    let Some(session) = state.authorize_any(&token).await else {
        return unauthorized_response();
    };
    if query.job.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "缺少 job 參數", "code": "AI_JOB_MISSING" })),
        )
            .into_response();
    }

    let snapshot = CHAT_JOBS.lock().ok().and_then(|map| {
        map.get(query.job.trim()).map(|job| {
            // 只能看自己的 job。
            if job.account != session.account_id {
                None
            } else {
                Some(job.to_json())
            }
        })
    });

    match snapshot {
        Some(Some(value)) => (StatusCode::OK, Json(value)).into_response(),
        Some(None) => unauthorized_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "找不到這個生成工作（可能已過期）",
                "code": "AI_JOB_NOT_FOUND",
            })),
        )
            .into_response(),
    }
}
