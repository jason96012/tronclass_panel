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
        "/admin/static/subpage-fetch.js",
        include_str!("static/js/subpage-fetch.js"),
    ),
    (
        "/admin/static/client-log.js",
        include_str!("static/js/client-log.js"),
    ),
    // CREDITS_ASSET_VERSION_FIX（2026-09-13）：credits.js 原先漏在 ASSETS 清單外，
    // 導致改版後 build id 不變、瀏覽器持續使用 immutable 快取的舊 JS。
    (
        "/admin/static/credits.js",
        include_str!("static/js/credits.js"),
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
        "/admin/static/js/suspensions-admin.js",
        include_str!("static/js/suspensions-admin.js"),
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
    // URL_SAFETY_SHARED_V1 / ROLLCALL_DETECT_SHARED_V1 / ROLLCALL_PENDING_FAB_V1
    // （fix-asset-version-gap）：rollcall-pending-fab.js 原本漏掉沒登記進 build id——
    // `rewrite_asset_versions()` 會把 HTML 裡這支的 URL 一律改寫成 `?sv=<build id>`
    // （連同本來手動維護的 `?v=fab2` 一起蓋掉，手動版號從此只是文件，不是真的在
    // 生效），但因為沒登記在這份清單，改這支檔案的內容不會讓 build id 變動，URL
    // 就不變；而這些都是用 `Cache-Control: immutable, max-age=604800` 送出的，
    // URL 不變，瀏覽器完全不會重新驗證，等於每次部署新版都可能有使用者卡在舊版
    // 最長 7 天——這正是這份清單檔頭註解講的「已經踩過兩次坑」的那種問題，只是
    // 這次是漏登記、不是手動忘記 bump。這裡把 url-safety.js／rollcall-detect.js
    // 一開始就登記進來，不再重蹈覆轍。
    (
        "/admin/static/url-safety.js",
        include_str!("static/js/url-safety.js"),
    ),
    (
        "/admin/static/rollcall-detect.js",
        include_str!("static/js/rollcall-detect.js"),
    ),
    (
        "/admin/static/rollcall-pending-fab.js",
        include_str!("static/js/rollcall-pending-fab.js"),
    ),
    // 分享頁資源（參與 build id；URL 改寫與否無妨——這兩個路徑字串只是拿來給
    // 下面 BUILD_ID 雜湊當 key，從不會被 rewrite_asset_versions() 實際改寫，因為
    // 它的 regex 只比對 `/admin/static/...`，不碰 `/pub/...`）。
    // ASSET_VERSION_LABEL_FIX_V1：原本這兩個字串寫的是 "/pub/share.css" /
    // "/pub/share-preview.js"，但 mod.rs 實際註冊的路由其實是
    // "/pub/static/css/share.css" / "/pub/static/js/share-preview.js"（多了
    // "static/" 這一段）——純屬命名打錯，從未對應到真正存在的路由。因為這兩個
    // 字串只被當 HashMap 的 key 用（雜湊同時吃 key 與 value），不影響內容本身
    // 有沒有被算進 build id，所以不是功能缺陷；改成跟真正路由一致純粹是讓這份
    // 清單能被信任「路徑欄＝真實路由」，日後要用路徑反查資產時才不會查錯。
    (
        "/pub/static/css/share.css",
        include_str!("static/css/share.css"),
    ),
    (
        "/pub/static/js/share-preview.js",
        include_str!("static/js/share-preview.js"),
    ),
    // AI_ASSET_VERSION_FIX_V1（2026-11-02）：AI 對話頁的 CSS 與模組原本沒登記在這份清單，
    // 改它們不會讓 build id 變動，ai.html 的 ?sv= 也不會變，手機（PWA）會以 immutable 快取
    // 繼續跑舊 CSS／舊模組，最多 7 天。這裡全部登記。
    (
        "/admin/static/ai/opencode.css",
        include_str!("static/opencode.css"),
    ),
    (
        "/admin/static/ai/js/state.js",
        include_str!("static/js/state.js"),
    ),
    (
        "/admin/static/ai/js/api.js",
        include_str!("static/js/api.js"),
    ),
    (
        "/admin/static/ai/js/markdown.js",
        include_str!("static/js/markdown.js"),
    ),
    (
        "/admin/static/ai/js/composer.js",
        include_str!("static/js/composer.js"),
    ),
    (
        "/admin/static/ai/js/timeline.js",
        include_str!("static/js/timeline.js"),
    ),
    (
        "/admin/static/ai/js/panels.js",
        include_str!("static/js/panels.js"),
    ),
    (
        "/admin/static/ai/js/command-palette.js",
        include_str!("static/js/command-palette.js"),
    ),
    (
        "/admin/static/ai/js/keyboard.js",
        include_str!("static/js/keyboard.js"),
    ),
    (
        "/admin/static/ai/js/opencode.js",
        include_str!("static/js/opencode.js"),
    ),
    (
        "/admin/static/ai/js/icons.js",
        include_str!("static/js/icons.js"),
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
    // 對 /admin/static/ 與 /pub/static/ 路徑（含現有 ?v=… 或 ?sv=…，故可重複套用）做全域改寫；路徑字元集不含
    // `?`，貪婪匹配自然停在 query 之前，引號／括號外的文字不受影響。
    Regex::new(r"(/(?:admin|pub)/static/[A-Za-z0-9._\-/]+)(\?(?:v|sv)=[A-Za-z0-9._\-]+)?")
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
/// AI_MODULE_VERSION_V1：ES module 的同目錄相對 import（`from './panels.js'`）不帶版號，
/// 會被 immutable 快取跨版本共用，造成「新 entry + 舊模組」。這裡把它改寫成 `?sv=<build id>`，
/// 與 HTML 裡 entry 的版號一致，所以整組模組會一起換。
static MODULE_IMPORT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"((?:from|import)\s*['"]\./[A-Za-z0-9._\-]+\.js)(['"])"#)
        .expect("AI_MODULE_VERSION_V1 regex must compile")
});

/// 改寫 JS 模組內的相對 import 版號（只給 AI 頁面的模組用）。
pub(super) fn rewrite_module_imports(input: &str) -> Cow<'_, str> {
    if !input.contains("./") {
        return Cow::Borrowed(input);
    }
    let replacement = format!("${{1}}?sv={}${{2}}", *BUILD_ID);
    MODULE_IMPORT_RE.replace_all(input, replacement.as_str())
}

pub(super) fn rewrite_asset_versions(input: &str) -> Cow<'_, str> {
    // 快速路徑：兩種前綴都不存在時零配置回傳（/pub/static 也必須改寫，見 ASSET_VERSION_AUTO_V1）。
    if !input.contains("/admin/static/") && !input.contains("/pub/static/") {
        return Cow::Borrowed(input);
    }
    let replacement = format!("$1?sv={}", *BUILD_ID);
    ASSET_URL_RE.replace_all(input, replacement)
}

