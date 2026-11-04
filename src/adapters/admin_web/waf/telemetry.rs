//! 即時遙測計數器與統計 API。

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

// WAF_AUTO_BLOCK_NOTIFY_V2：WAF 新建立自動封鎖時，透過 notify_waf_auto_block 只發 Discord 給
// admin_user_id（同一 IP 24 小時去重），附「解除封鎖」按鈕。不發 LINE、不發 Web Push。
use crate::adapters::admin_web::AdminWebState;

/// WAF 即時遙測計數器。
pub struct WafTelemetry {
    pub total_requests: AtomicU64,
    pub total_blocked: AtomicU64,
    pub honeypot_probes: AtomicU64,
    pub sql_injections: AtomicU64,
    pub rce_attempts: AtomicU64,
    pub xss_attempts: AtomicU64,
    pub path_traversals: AtomicU64,
    pub bot_scanners: AtomicU64,
    pub ssrf_attempts: AtomicU64,
    pub ssti_attempts: AtomicU64,
    pub proto_pollution: AtomicU64,
    pub deserialization_attempts: AtomicU64,
    pub host_poisoning: AtomicU64,
    pub smuggling_attempts: AtomicU64,
    pub webshell_attempts: AtomicU64,
    pub rate_limited: AtomicU64,
    pub anomaly_blocked: AtomicU64,
    pub fingerprint_blocked: AtomicU64,
    pub whitelist_bypass: AtomicU64,
}

impl Default for WafTelemetry {
    fn default() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            total_blocked: AtomicU64::new(0),
            honeypot_probes: AtomicU64::new(0),
            sql_injections: AtomicU64::new(0),
            rce_attempts: AtomicU64::new(0),
            xss_attempts: AtomicU64::new(0),
            path_traversals: AtomicU64::new(0),
            bot_scanners: AtomicU64::new(0),
            ssrf_attempts: AtomicU64::new(0),
            ssti_attempts: AtomicU64::new(0),
            proto_pollution: AtomicU64::new(0),
            deserialization_attempts: AtomicU64::new(0),
            host_poisoning: AtomicU64::new(0),
            smuggling_attempts: AtomicU64::new(0),
            webshell_attempts: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            anomaly_blocked: AtomicU64::new(0),
            fingerprint_blocked: AtomicU64::new(0),
            whitelist_bypass: AtomicU64::new(0),
        }
    }
}

/// WAF 即時遙測統計 API：回傳各類攻擊阻斷計數與防禦指標。
pub async fn waf_stats_handler(
    State(state): State<AdminWebState>,
) -> Response {
    let stats = serde_json::json!({
        "total_requests": state.waf.telemetry.total_requests.load(Ordering::Relaxed),
        "total_blocked": state.waf.telemetry.total_blocked.load(Ordering::Relaxed),
        "honeypot_probes": state.waf.telemetry.honeypot_probes.load(Ordering::Relaxed),
        "sql_injections": state.waf.telemetry.sql_injections.load(Ordering::Relaxed),
        "rce_attempts": state.waf.telemetry.rce_attempts.load(Ordering::Relaxed),
        "xss_attempts": state.waf.telemetry.xss_attempts.load(Ordering::Relaxed),
        "path_traversals": state.waf.telemetry.path_traversals.load(Ordering::Relaxed),
        "bot_scanners": state.waf.telemetry.bot_scanners.load(Ordering::Relaxed),
        "ssrf_attempts": state.waf.telemetry.ssrf_attempts.load(Ordering::Relaxed),
        "ssti_attempts": state.waf.telemetry.ssti_attempts.load(Ordering::Relaxed),
        "proto_pollution": state.waf.telemetry.proto_pollution.load(Ordering::Relaxed),
        "deserialization_attempts": state.waf.telemetry.deserialization_attempts.load(Ordering::Relaxed),
        "host_poisoning": state.waf.telemetry.host_poisoning.load(Ordering::Relaxed),
        "smuggling_attempts": state.waf.telemetry.smuggling_attempts.load(Ordering::Relaxed),
        "webshell_attempts": state.waf.telemetry.webshell_attempts.load(Ordering::Relaxed),
        "rate_limited": state.waf.telemetry.rate_limited.load(Ordering::Relaxed),
        "anomaly_blocked": state.waf.telemetry.anomaly_blocked.load(Ordering::Relaxed),
        "fingerprint_blocked": state.waf.telemetry.fingerprint_blocked.load(Ordering::Relaxed),
        "whitelist_bypass": state.waf.telemetry.whitelist_bypass.load(Ordering::Relaxed),
        "rules_version": state.waf.rules.version,
        "rules_source": state.waf.rules.source_label(),
    });

    (
        StatusCode::OK,
        Json(stats),
    )
        .into_response()
}
