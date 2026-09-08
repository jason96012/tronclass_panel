//! ASSET_VERSION_AUTO_V1：靜態資產版號自動化。
//!
//! `/admin/static/*` 以 `Cache-Control: max-age=604800, immutable` 快取，
//! 因此 URL 必須隨內容變更（過去靠手動 bump `?v=dashmod4`／`?v=mnav8`，
//! 忘記就會讓瀏覽器繼續跑舊 JS——本專案已因此踩過兩次坑）。
//!
//! 改為 runtime 統一改寫：輸出 HTML／CSS／JS 時，把每個
//! `/admin/static/...` 參照（含現有 `?v=...`）一律改寫為
//! `?sv=<build id>`；build id = 所有內嵌資產內容的 SHA-256 前 6 bytes。
//!
//! 為何是「全域」id 而非 per-asset 內容雜湊：資產會互相引用
//! （mobile-nav.js 注入 `mobile-navigation.css?v=...` 連結）。若 A（js）
//! 沒改而 B（css）改了，per-asset 版號下 A 的 URL 不變、瀏覽器繼續用
//! immutable 快取的 A，A 內部寫死的 B 舊版號就永遠拿不到新 CSS。
//! 全域 id 讓任何資產變更都推進所有 URL，巢狀引用必然新鮮。
//!
//! 手動版號從此只需留著當文件（改寫發生在送出前），不必再 bump。

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::borrow::Cow;

/// 參與 build id 的資產（URL path → 檔案內容）。涵蓋所有 /admin/static/
/// 內嵌檔案；清單若未來有遺漏，遺漏者只是不會推進 build id（其 URL 仍被
/// 改寫，行為與手動版號現狀相同，不會更糟）。
const ASSETS: &[(&str, &str)] = &[
    // CSS
    ("/admin/static/theme.css", include_str!("static/theme.css")),
    (
        "/admin/static/maintenance-components.css",
        include_str!("static/css/maintenance-components.css"),
    ),
    (
        "/admin/static/account-components.css",
        include_str!("static/css/account-components.css"),
    ),
    (
        "/admin/static/mobile-timetable.css",
        include_str!("static/css/mobile-timetable.css"),
    ),
    (
        "/admin/static/mobile-navigation.css",
        include_str!("static/css/mobile-navigation.css"),
    ),
    (
        "/admin/static/dashboard.css",
        include_str!("static/css/dashboard.css"),
    ),
    (
        "/admin/static/dashboard-late-fixes.css",
        include_str!("static/css/dashboard-late-fixes.css"),
    ),
    (
        "/admin/static/dashboard-mobile.css",
        include_str!("static/css/dashboard-mobile.css"),
    ),
    (
        "/admin/static/dashboard-components.css",
        include_str!("static/css/dashboard-components.css"),
    ),
    (
        "/admin/static/dashboard-access.css",
        include_str!("static/css/dashboard-access.css"),
    ),
    // 根目錄 JS
    (
        "/admin/static/session-security.js",
        include_str!("static/js/session-security.js"),
    ),
    (
        "/admin/static/app-settings.js",
        include_str!("static/js/app-settings.js"),
    ),
    (
        "/admin/static/notifications-page.js",
        include_str!("static/js/notifications-page.js"),
    ),
    (
        "/admin/static/core-api.js",
        include_str!("static/js/core-api.js"),
    ),
    (
        "/admin/static/web-push.js",
        include_str!("static/js/web-push.js"),
    ),
    // DASHBOARD_LIVE_EVENTS_V1
    (
        "/admin/static/live-events.js",
        include_str!("static/js/live-events.js"),
    ),
    (
        "/admin/static/accounts.js",
        include_str!("static/js/accounts.js"),
    ),
    (
        "/admin/static/mock-rollcall.js",
        include_str!("static/js/mock-rollcall.js"),
    ),
    (
        "/admin/static/timetable.js",
        include_str!("static/js/timetable.js"),
    ),
    (
        "/admin/static/maintenance-implementation.js",
        include_str!("static/js/maintenance-implementation.js"),
    ),
    (
        "/admin/static/maintenance.js",
        include_str!("static/js/maintenance.js"),
    ),
    (
        "/admin/static/dashboard-runtime.js",
        include_str!("static/js/dashboard-runtime.js"),
    ),
    (
        "/admin/static/dashboard-modules.js",
        include_str!("static/js/dashboard-modules.js"),
    ),
    // js/ 底下的 JS
    (
        "/admin/static/js/dompurify.min.js",
        include_str!("static/js/dompurify.min.js"),
    ),
    (
        "/admin/static/js/trusted-types-init.js",
        include_str!("static/js/trusted-types-init.js"),
    ),
    (
        "/admin/static/js/jszip.min.js",
        include_str!("static/js/jszip.min.js"),
    ),
    (
        "/admin/static/js/office-view.js",
        include_str!("static/js/office-view.js"),
    ),
    (
        "/admin/static/js/qrcode.min.js",
        include_str!("static/js/qrcode.min.js"),
    ),
    (
        "/admin/static/js/shares-admin.js",
        include_str!("static/js/shares-admin.js"),
    ),
    (
        "/admin/static/js/dashboard-particles.js",
        include_str!("static/js/dashboard-particles.js"),
    ),
    (
        "/admin/static/js/dashboard-login-card.js",
        include_str!("static/js/dashboard-login-card.js"),
    ),
    (
        "/admin/static/js/dashboard-live-alerts.js",
        include_str!("static/js/dashboard-live-alerts.js"),
    ),
    (
        "/admin/static/js/dashboard-permissions.js",
        include_str!("static/js/dashboard-permissions.js"),
    ),
    (
        "/admin/static/js/dashboard-cookie-badge.js",
        include_str!("static/js/dashboard-cookie-badge.js"),
    ),
    (
        "/admin/static/js/dashboard-modal.js",
        include_str!("static/js/dashboard-modal.js"),
    ),
    (
        "/admin/static/js/dashboard-header-more.js",
        include_str!("static/js/dashboard-header-more.js"),
    ),
    (
        "/admin/static/js/dashboard-more-buttons.js",
        include_str!("static/js/dashboard-more-buttons.js"),
    ),
    (
        "/admin/static/js/dashboard-more-links.js",
        include_str!("static/js/dashboard-more-links.js"),
    ),
    (
        "/admin/static/js/dashboard-more-pages.js",
        include_str!("static/js/dashboard-more-pages.js"),
    ),
    (
        "/admin/static/js/dashboard-notify-purge.js",
        include_str!("static/js/dashboard-notify-purge.js"),
    ),
    (
        "/admin/static/js/dashboard-desktop-size.js",
        include_str!("static/js/dashboard-desktop-size.js"),
    ),
    (
        "/admin/static/js/dashboard-header-spacious.js",
        include_str!("static/js/dashboard-header-spacious.js"),
    ),
    (
        "/admin/static/js/dashboard-more-actions.js",
        include_str!("static/js/dashboard-more-actions.js"),
    ),
    (
        "/admin/static/js/mobile-nav.js",
        include_str!("static/js/mobile-nav.js"),
    ),
    // ONBOARDING_GUIDE_V1
    (
        "/admin/static/js/dashboard-onboarding.js",
        include_str!("static/js/dashboard-onboarding.js"),
    ),
    (
        "/admin/static/js/pdf.min.js",
        include_str!("static/js/pdf.min.js"),
    ),
    (
        "/admin/static/js/pdf.worker.min.js",
        include_str!("static/js/pdf.worker.min.js"),
    ),
    // 分享頁資源（參與 build id；URL 改寫與否無妨）
    ("/pub/share.css", include_str!("static/css/share.css")),
    (
        "/pub/share-preview.js",
        include_str!("static/js/share-preview.js"),
    ),
];

static BUILD_ID: Lazy<String> = Lazy::new(|| {
    let mut hasher = Sha256::new();
    for (path, content) in ASSETS {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(content.as_bytes());
    }
    let digest = hasher.finalize();
    format!("b{}", hex::encode(&digest[..6]))
});

static ASSET_URL_RE: Lazy<Regex> = Lazy::new(|| {
    // 對 /admin/static/ 路徑（含現有 ?v=…）做全域改寫；路徑字元集不含
    // `?`，貪婪匹配自然停在 query 之前，引號／括號外的文字不受影響。
    Regex::new(r"(/admin/static/[A-Za-z0-9._\-/]+)(\?v=[A-Za-z0-9._\-]+)?")
        .expect("ASSET_VERSION_AUTO_V1 regex must compile")
});

/// DASHBOARD_LIVE_EVENTS_V1：對外提供 build id（SSE `version` 事件／自動重載偵測）。
/// 由於 build id 是所有內嵌資產內容的雜湊，任何前端檔案變更重新部署後都會不同；
/// 同一個 binary 重啟則維持相同（不會誤觸發重載）。
pub(super) fn build_id() -> &'static str {
    BUILD_ID.as_str()
}

/// 把輸入中所有 `/admin/static/...` 參照統一改寫為 `?sv=<build id>`。
/// 沒有任何參照時回傳借用（零配置）；`?sv=` 前綴與舊 `?v=` 區別，
/// 看回應就能確認改寫已生效。
pub(super) fn rewrite_asset_versions(input: &str) -> Cow<'_, str> {
    if !input.contains("/admin/static/") {
        return Cow::Borrowed(input);
    }
    let replacement = format!("$1?sv={}", *BUILD_ID);
    ASSET_URL_RE.replace_all(input, replacement)
}

#[cfg(test)]
mod tests {
    /// UNWRAP_SWEEP_V1：強制初始化 ASSET_URL_RE。
    #[test]
    fn asset_url_regex_compiles() {
        let _ = &*super::ASSET_URL_RE;
        let out = super::rewrite_asset_versions("<a href=\"/admin/static/x.js?v=1\">");
        assert!(out.contains("/admin/static/x.js?sv="));
    }
}
