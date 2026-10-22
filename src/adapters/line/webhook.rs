//! LINE webhook HTTP entry point and LINE-to-core event conversion.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use miette::{IntoDiagnostic, Result, WrapErr};
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};

use crate::adapters::admin_web::{self, AdminWebState};
use crate::adapters::requests::{self, AdapterRequest, RequestContent, RequestState};
use crate::adapters::scanner::{
    tool_security_headers_middleware, NumberCodeSubmission, QrScannerRegistry,
    RadarPointSubmission, ScannerStatusQuery, ScannerSubmission,
};

use super::client::LineBotClient;
use super::types::{Event, LineMessage, WebhookPayload};

const SIGNATURE_HEADER: &str = "x-line-signature";
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

#[derive(Clone)]
pub struct LineWebhookState {
    pub bot: Arc<LineBotClient>,
    pub requests: RequestState,
    pub scanner: Option<Arc<QrScannerRegistry>>,
    pub admin_web: Option<AdminWebState>,
}

impl LineWebhookState {
    pub fn new(
        bot: Arc<LineBotClient>,
        requests: RequestState,
        scanner: Option<Arc<QrScannerRegistry>>,
    ) -> Self {
        Self {
            bot,
            requests,
            scanner,
            admin_web: None,
        }
    }

    pub fn with_admin_web(mut self, admin_web: Option<AdminWebState>) -> Self {
        self.admin_web = admin_web;
        self
    }
}

pub fn build_router(state: LineWebhookState, webhook_path: &str) -> Router {
    let admin_web = state.admin_web.clone();
    let mut app = Router::new()
        .route(webhook_path, post(webhook_handler))
        .route("/scanner", get(scanner_page_handler))
        .route("/scanner/jsqr.js", get(scanner_jsqr_handler))
        // QR_ZXING_V1（fix86）：zxing-wasm（主要解碼器）與它的 wasm 檔（見 scanner.rs 的說明）。
        .route("/scanner/zxing.js", get(scanner_zxing_js_handler))
        .route("/scanner/zxing_reader.wasm", get(scanner_zxing_wasm_handler))
        // QR_WECHAT_V1（fix87）：微信 QRCode 引擎（見 scanner.rs 的說明）。
        .route("/scanner/wechat/index.mjs", get(scanner_wechat_index_handler))
        .route("/scanner/wechat/wasm.mjs", get(scanner_wechat_wasm_handler))
        .route("/scanner/sr/worker.mjs", get(scanner_sr_worker_handler))
        .route("/scanner/sr/ort.mjs", get(scanner_sr_ort_handler))
        .route("/scanner/sr/ort-wasm-simd.wasm", get(scanner_sr_wasm_handler))
        .route("/scanner/sr/model.onnx", get(scanner_sr_model_handler))
        .route("/scanner/status", get(scanner_status_handler))
        .route("/scanner/submit", post(scanner_submit_handler))
        .route("/scanner/resume", post(scanner_resume_handler))
        .route("/number", get(number_page_handler))
        .route("/number/status", get(number_status_handler))
        .route("/number/submit", post(number_submit_handler))
        // RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁（與 scanner 伺服器同一個頁面）。
        .route("/radar", get(radar_page_handler))
        .route("/radar/status", get(radar_status_handler))
        .route("/radar/submit", post(radar_submit_handler))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(tool_security_headers_middleware))
        // TOOL_RATE_LIMIT_V1（見 `scanner.rs` 同名常數/中介層的說明）：這個
        // server 跟 admin_web 的 WAF 是分開的中介層堆疊，補上最基本的洪水防護。
        // 刻意放最外層，真正洪水等級的流量在其他處理之前就擋掉。
        .layer(axum::middleware::from_fn(
            crate::adapters::scanner::tool_rate_limit_middleware,
        ))
        .with_state(state);

    if let Some(admin_web) = admin_web {
        app = app.merge(admin_web::build_admin_router(admin_web));
    }

    app
}

pub async fn start_webhook_server(
    state: LineWebhookState,
    bind_addr: &str,
    port: u16,
    webhook_path: &str,
) -> Result<()> {
    let app = build_router(state, webhook_path);
    let ip = bind_addr
        .trim()
        .parse::<std::net::IpAddr>()
        .into_diagnostic()
        .wrap_err_with(|| format!("無效的 Webhook bind address: `{bind_addr}`"))?;
    let addr = std::net::SocketAddr::new(ip, port);

    info!(bind_addr = %bind_addr, port = port, path = %webhook_path, "Line Bot Webhook 伺服器啟動：http://{}{}", addr, webhook_path);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to bind to port {port}"))?;

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .into_diagnostic()
    .wrap_err("Webhook server error")
}

async fn webhook_handler(
    State(state): State<LineWebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY_SIZE {
        warn!(size = body.len(), "Webhook body 過大，拒絕處理");
        return (StatusCode::PAYLOAD_TOO_LARGE, "Request body too large").into_response();
    }

    let signature = match headers.get(SIGNATURE_HEADER).and_then(|v| v.to_str().ok()) {
        Some(sig) => sig.to_string(),
        None => {
            warn!("缺少 X-Line-Signature Header");
            return (StatusCode::BAD_REQUEST, "Missing X-Line-Signature header").into_response();
        }
    };

    if !state.bot.verify_signature(&body, &signature) {
        // REDIRECT_UTF8_SLICE_V1：統一改用字元截斷。
        warn!(signature = %crate::text_utils::truncate_chars(&signature, 20), "X-Line-Signature 驗證失敗");
        return (StatusCode::BAD_REQUEST, "Invalid signature").into_response();
    }

    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "Webhook payload 反序列化失敗");
            return (StatusCode::OK, "").into_response();
        }
    };

    debug!(
        destination = %payload.destination,
        event_count = payload.events.len(),
        "收到 Webhook Payload"
    );

    for event in &payload.events {
        match adapter_request_from_line(event) {
            Some(request) => {
                if let Err(e) = requests::handle_request(request, &state.requests).await {
                    error!(error = %e, "處理 adapter request 失敗");
                }
            }
            None => debug!("LINE 事件無法轉換或不需處理，忽略"),
        }
    }

    (StatusCode::OK, "").into_response()
}

async fn scanner_jsqr_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("jsQR.min.js"),
    )
        .into_response()
}

/// QR_ZXING_V1（fix86）：zxing-wasm 的 JS 與 wasm（本機供應，不連 CDN）。
/// 這條路徑（webhook 伺服器）跟 scanner.rs 那條會各自供應一份，兩邊都要有，
/// 否則在 webhook 部署模式下頁面會載不到解碼器、只能退回 jsQR。
async fn scanner_zxing_js_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("zxing.js"),
    )
        .into_response()
}

/// QR_WECHAT_V1（fix87）：微信 QRCode 引擎（本機供應；webhook 模式也要有，否則載不到）。
async fn scanner_wechat_index_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("wechat/index.mjs"),
    )
        .into_response()
}

async fn scanner_wechat_wasm_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("wechat/wasm.mjs"),
    )
        .into_response()
}

/// QR_AI_SR_V1（fix93）：AI 超分救援的資產（只在使用者「深度掃描一直解不到」時才會下載）。
///
/// 為什麼要加：實測（見 fix93 報告）「差一點點」的案例（QR 每模組約 3.6px，三個引擎原本全 ❌），
///   用 Swin2SR-lightweight ×2 超分之後 zxing 與微信**都解得出來**；但實拍投影幕照片
///   （訊號已低於可解極限）救不回 —— 所以設計成「最後一擊」而不是每帧跑。
/// 四個檔案都由本機供應（不連 CDN）：
///   `sr/worker.mjs`            —— worker 橋接（onnxruntime-web 推論，背景執行緒）
///   `sr/ort.mjs`               —— onnxruntime-web 1.17.3 的 WASM-only ESM 建置（單執行緒、
///                                      不需要 SharedArrayBuffer，部署端不必加 COOP/COEP）
///   `sr/ort-wasm-simd.wasm`    —— ORT 的 WASM 執行檔（約 10MB）
///   `sr/model.onnx`            —— Swin2SR-lightweight ×2（uint8 量化，約 6.8MB；Apache-2.0）
async fn scanner_sr_worker_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("sr/worker.mjs"),
    )
        .into_response()
}

async fn scanner_sr_ort_handler() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_str!("sr/ort.mjs"),
    )
        .into_response()
}

async fn scanner_sr_wasm_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("sr/ort-wasm-simd.wasm").as_slice(),
    )
        .into_response()
}

async fn scanner_sr_model_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("sr/model.onnx").as_slice(),
    )
        .into_response()
}

async fn scanner_zxing_wasm_handler() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        include_bytes!("zxing_reader.wasm").as_slice(),
    )
        .into_response()
}

async fn scanner_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("scanner.html")),
    )
        .into_response()
}

async fn number_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("number.html")),
    )
        .into_response()
}

/// RADAR_INPUT_PAGE_V1（fix71）：雷達點名輸入頁。
async fn radar_page_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(include_str!("radar.html")),
    )
        .into_response()
}

async fn scanner_status_handler(
    State(state): State<LineWebhookState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.status(query).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string(), "code": e.code() })),
        )
            .into_response(),
    }
}

async fn number_status_handler(
    State(state): State<LineWebhookState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.number_status(query).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn number_submit_handler(
    State(state): State<LineWebhookState>,
    Json(submission): Json<NumberCodeSubmission>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.submit_number(submission).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn radar_status_handler(
    State(state): State<LineWebhookState>,
    Query(query): Query<ScannerStatusQuery>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.radar_status(query).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn radar_submit_handler(
    State(state): State<LineWebhookState>,
    Json(submission): Json<RadarPointSubmission>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.submit_radar(submission).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn scanner_resume_handler(
    State(state): State<LineWebhookState>,
    Json(query): Json<ScannerStatusQuery>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "scanner is not enabled"}))).into_response();
    };
    match scanner.request_qr_resume(query).await {
        Ok(result) => (StatusCode::ACCEPTED, Json(serde_json::json!(result))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": e.to_string(), "code": e.code() }))).into_response(),
    }
}

async fn scanner_submit_handler(
    State(state): State<LineWebhookState>,
    Json(submission): Json<ScannerSubmission>,
) -> Response {
    let Some(scanner) = state.scanner.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "scanner is not enabled" })),
        )
            .into_response();
    };

    match scanner.submit(submission).await {
        Ok(result) => (StatusCode::OK, Json(json!(result))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string(), "code": e.code() })),
        )
            .into_response(),
    }
}

pub fn adapter_request_from_line(event: &Event) -> Option<AdapterRequest> {
    match event {
        Event::Message(msg_event) => {
            let user_id = msg_event.common.source.user_id().unwrap_or("").to_string();
            let reply_token = msg_event.common.reply_token.clone();
            let is_direct_user = msg_event.common.source.is_user();
            let content = match &msg_event.message {
                LineMessage::Text(text) => RequestContent::Text(text.text.clone()),
                LineMessage::Image(_) | LineMessage::Video(_) | LineMessage::Audio(_) => {
                    RequestContent::Media
                }
                LineMessage::Location(loc) => RequestContent::Location {
                    latitude: loc.latitude,
                    longitude: loc.longitude,
                },
                LineMessage::Sticker(_) => RequestContent::Sticker,
                _ => RequestContent::Unknown,
            };

            Some(AdapterRequest::new(
                user_id,
                reply_token,
                is_direct_user,
                content,
            ))
        }
        Event::Follow(follow_event) => Some(AdapterRequest::new(
            follow_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            follow_event.common.reply_token.clone(),
            follow_event.common.source.is_user(),
            RequestContent::Follow,
        )),
        Event::Unfollow(unfollow_event) => Some(AdapterRequest::new(
            unfollow_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            unfollow_event.common.reply_token.clone(),
            unfollow_event.common.source.is_user(),
            RequestContent::Unfollow,
        )),
        Event::Join(join_event) => Some(AdapterRequest::new(
            join_event.common.source.user_id().unwrap_or("").to_string(),
            join_event.common.reply_token.clone(),
            join_event.common.source.is_user(),
            RequestContent::Join,
        )),
        Event::Leave(leave_event) => Some(AdapterRequest::new(
            leave_event
                .common
                .source
                .user_id()
                .unwrap_or("")
                .to_string(),
            leave_event.common.reply_token.clone(),
            leave_event.common.source.is_user(),
            RequestContent::Leave,
        )),
        Event::Postback(pb_event) => Some(AdapterRequest::new(
            pb_event.common.source.user_id().unwrap_or("").to_string(),
            pb_event.common.reply_token.clone(),
            pb_event.common.source.is_user(),
            RequestContent::Text(pb_event.postback.data.clone()),
        )),
        Event::Unknown => Some(AdapterRequest::new(
            "",
            None,
            false,
            RequestContent::Unknown,
        )),
        _ => Some(AdapterRequest::new(
            "",
            None,
            false,
            RequestContent::Unknown,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    use crate::adapters::line::types;
    use crate::adapters::scanner::QrScannerRegistry;
    use crate::config::LineBotConfig;

    fn test_bot() -> Arc<LineBotClient> {
        Arc::new(
            LineBotClient::new(&LineBotConfig {
                enabled: true,
                channel_secret: "secret".to_string(),
                channel_access_token: "token".to_string(),
                webhook_port: 8080,
                webhook_bind_addr: "127.0.0.1".to_string(),
                webhook_path: "/webhook".to_string(),
                public_base_url: "https://scanner.example.test".to_string(),
                admin_user_id: "Uadmin123".to_string(),
            })
            .unwrap(),
        )
    }

    fn test_state(scanner: Option<Arc<QrScannerRegistry>>) -> LineWebhookState {
        let bot = test_bot();
        let messenger = Arc::clone(&bot) as Arc<dyn crate::adapters::events::AdapterMessenger>;
        let requests = RequestState::new(messenger, requests::AccountRegistry::default());
        LineWebhookState::new(bot, requests, scanner)
    }

    fn make_event_common(source: types::EventSource) -> types::EventCommon {
        types::EventCommon {
            webhook_event_id: "test-event".to_string(),
            reply_token: None,
            timestamp: 0,
            source,
            delivery_context: types::DeliveryContext::default(),
        }
    }

    #[test]
    fn test_non_user_message_event_maps_as_not_direct() {
        let event = Event::Message(types::MessageEvent {
            common: make_event_common(types::EventSource::Group {
                group_id: "Ggroup".to_string(),
                user_id: Some("Uadmin123".to_string()),
            }),
            message: LineMessage::Text(types::TextMessage {
                id: "text-1".to_string(),
                text: "/stop".to_string(),
                emojis: vec![],
                mention: None,
                quoted_message_id: None,
            }),
        });

        let request = adapter_request_from_line(&event).unwrap();
        assert!(!request.is_direct_user);
        assert_eq!(request.user_id, "Uadmin123");
    }

    #[tokio::test]
    async fn scanner_route_serves_html() {
        let app = build_router(
            test_state(Some(Arc::new(QrScannerRegistry::new(
                "https://scanner.example.test",
            )))),
            "/webhook",
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/scanner")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("Tronclass QR Scanner"));
        assert!(html.contains("jsQR"));
    }

    #[tokio::test]
    async fn scanner_submit_route_delivers_qr_data() {
        let scanner = Arc::new(QrScannerRegistry::new("https://scanner.example.test"));
        let (tx, mut rx) = mpsc::channel(1);
        let link = scanner
            .register_pending("fju", 42, "acc1", tx, std::time::Duration::from_secs(60))
            .await
            .unwrap();
        let app = build_router(test_state(Some(scanner)), "/webhook");
        let body = serde_json::json!({
            "provider": "fju",
            "rollcall_id": 42,
            "account_id": "acc1",
            "token": link.token,
            "qr_data": "0~100!3~secret!4~42"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/scanner/submit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(rx.recv().await.unwrap(), "0~100!3~secret!4~42");
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["delivered_count"], 1);
    }

    #[tokio::test]
    async fn scanner_submit_route_returns_error_for_invalid_submit() {
        let app = build_router(
            test_state(Some(Arc::new(QrScannerRegistry::new(
                "https://scanner.example.test",
            )))),
            "/webhook",
        );
        let body = serde_json::json!({
            "provider": "fju",
            "rollcall_id": 42,
            "account_id": "acc1",
            "token": "missing",
            "qr_data": "0~100!3~secret!4~42"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/scanner/submit")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("等待中"));
    }
}
