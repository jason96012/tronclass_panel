//! HTML templates — Tronclass Admin Web UI.

use std::sync::RwLock;

// Page handlers below reuse the shared auth/cookie/response helpers from the
// parent module, so pull in the parent namespace (same pattern as files.rs).
use super::*;

static RUNTIME_THEME: RwLock<Option<String>> = RwLock::new(None);

pub(super) fn load_runtime_theme() {
    let paths = [
        "src/adapters/admin_web/static/theme.css",
        "theme.css",
        "static/theme.css",
    ];
    for p in &paths {
        if let Ok(css) = std::fs::read_to_string(p) {
            if !css.trim().is_empty() {
                *RUNTIME_THEME.write().unwrap_or_else(|p| p.into_inner()) = Some(css);
                tracing::info!(path = %p, "runtime theme.css loaded");
                return;
            }
        }
    }
}

pub(super) fn theme_css() -> String {
    if let Some(ref css) = *RUNTIME_THEME.read().unwrap_or_else(|p| p.into_inner()) {
        return css.clone();
    }
    include_str!("static/theme.css").to_string()
}

pub(super) const DOMPURIFY_JS: &str = include_str!("static/js/dompurify.min.js");
pub(super) const TRUSTED_TYPES_INIT_JS: &str = include_str!("static/js/trusted-types-init.js");
pub(super) const JSZIP_MIN_JS: &str = include_str!("static/js/jszip.min.js");
pub(super) const OFFICE_VIEW_JS: &str = include_str!("static/js/office-view.js");
pub(super) const QR_CODE_MIN_JS: &str = include_str!("static/js/qrcode.min.js");
// SHARE_ADMIN_V1: 分享連結管理頁（管理員專用）與其前端。
pub(super) const SHARES_HTML: &str = include_str!("static/shares.html");
pub(super) const SHARES_ADMIN_JS: &str = include_str!("static/js/shares-admin.js");
// DASH_ASSET_V1: dashboard 單體 inline JS 資產化（粒子/登入卡/SSE+提示音）。
pub(super) const DASHBOARD_PARTICLES_JS: &str = include_str!("static/js/dashboard-particles.js");
pub(super) const DASHBOARD_LOGIN_CARD_JS: &str = include_str!("static/js/dashboard-login-card.js");
pub(super) const DASHBOARD_LIVE_ALERTS_JS: &str = include_str!("static/js/dashboard-live-alerts.js");
// DASH_ASSET_V2: 第二批（行動端權限／校務 Cookie 徽章／彈窗顯示）。
pub(super) const DASHBOARD_PERMISSIONS_JS: &str = include_str!("static/js/dashboard-permissions.js");
pub(super) const DASHBOARD_COOKIE_BADGE_JS: &str = include_str!("static/js/dashboard-cookie-badge.js");
pub(super) const DASHBOARD_MODAL_JS: &str = include_str!("static/js/dashboard-modal.js");
// DASH_ASSET_V3: 第三批（標頭／MORE 選單系列）。
pub(super) const DASHBOARD_HEADER_MORE_JS: &str = include_str!("static/js/dashboard-header-more.js");
pub(super) const DASHBOARD_MORE_BUTTONS_JS: &str = include_str!("static/js/dashboard-more-buttons.js");
pub(super) const DASHBOARD_MORE_LINKS_JS: &str = include_str!("static/js/dashboard-more-links.js");
pub(super) const DASHBOARD_MORE_PAGES_JS: &str = include_str!("static/js/dashboard-more-pages.js");
pub(super) const DASHBOARD_NOTIFY_PURGE_JS: &str = include_str!("static/js/dashboard-notify-purge.js");
pub(super) const DASHBOARD_DESKTOP_SIZE_JS: &str = include_str!("static/js/dashboard-desktop-size.js");
pub(super) const DASHBOARD_HEADER_SPACIOUS_JS: &str = include_str!("static/js/dashboard-header-spacious.js");
pub(super) const DASHBOARD_MORE_ACTIONS_JS: &str = include_str!("static/js/dashboard-more-actions.js");
// SHARED_MOBILE_NAV_V1: 全頁共用手機導覽列。
pub(super) const SHARED_MOBILE_NAV_JS: &str = include_str!("static/js/mobile-nav.js");
// ONBOARDING_GUIDE_V1: 第一次進入 Dashboard 的新手導覽。
pub(super) const DASHBOARD_ONBOARDING_JS: &str = include_str!("static/js/dashboard-onboarding.js");
// SHARE_V1: 免登入分享預覽頁的外掛 JS/CSS（符合 CSP，不用 inline）。
pub(super) const SHARE_PREVIEW_JS: &str = include_str!("static/js/share-preview.js");
pub(super) const SHARE_CSS: &str = include_str!("static/css/share.css");
// PDFJS_PREVIEW_V1: 套用 Firefox 同款 PDF.js 檢視器（純 JS canvas 渲染，
// 不靠瀏覽器原生外掛，所有瀏覽器一致，解決 iframe 空白）。
pub(super) const PDF_MIN_JS: &str = include_str!("static/js/pdf.min.js");
pub(super) const PDF_WORKER_MIN_JS: &str = include_str!("static/js/pdf.worker.min.js");
pub(super) const SESSION_SECURITY_JS: &str = include_str!("static/js/session-security.js");
pub(super) const CORE_API_JS: &str = include_str!("static/js/core-api.js");
pub(super) const WEB_PUSH_JS: &str = include_str!("static/js/web-push.js");
// DASHBOARD_LIVE_EVENTS_V1
pub(super) const LIVE_EVENTS_JS: &str = include_str!("static/js/live-events.js");
// URL_SAFETY_SHARED_V1：「這個字串是不是可信的 http(s) 網址」共用判斷，排在
// ROLLCALL_DETECT_JS／ROLLCALL_PENDING_FAB_JS 之前（兩者皆會用到它）。
pub(super) const URL_SAFETY_JS: &str = include_str!("static/js/url-safety.js");
// ROLLCALL_DETECT_SHARED_V1：「偵測到點名、分組、算完成/未送出人數」的共用正本，
// 必須排在下面 ROLLCALL_PENDING_FAB_JS 之前載入（inject_theme 會保證順序）。
pub(super) const ROLLCALL_DETECT_JS: &str = include_str!("static/js/rollcall-detect.js");
// ROLLCALL_PENDING_FAB_V1：全站共用、右下角「還有點名尚未送出」捷徑按鈕。
pub(super) const ROLLCALL_PENDING_FAB_JS: &str = include_str!("static/js/rollcall-pending-fab.js");
/// SUBPAGE_FETCH_SHARED_V1：子頁共用的唯一 window.fetch 包裝（同步、在 inline script 之前）。
pub(super) const SUBPAGE_FETCH_JS: &str = include_str!("static/js/subpage-fetch.js");
/// CLIENT_ERROR_REPORT_V1：前端錯誤／看門狗事件回報。
pub(super) const CLIENT_LOG_JS: &str = include_str!("static/js/client-log.js");
pub(super) const ACCOUNTS_MODULE_JS: &str = include_str!("static/js/accounts.js");
pub(super) const MOCK_ROLLCALL_MODULE_JS: &str = include_str!("static/js/mock-rollcall.js");
pub(super) const TIMETABLE_UI_MODULE_JS: &str = include_str!("static/js/timetable.js");
pub(super) const MAINTENANCE_IMPLEMENTATION_JS: &str =
    include_str!("static/js/maintenance-implementation.js");
pub(super) const MAINTENANCE_MODULE_JS: &str = include_str!("static/js/maintenance.js");
// MAINTENANCE_COMPONENTS_CSS_PHASE3B4
pub(super) const MAINTENANCE_COMPONENTS_CSS: &str =
    include_str!("static/css/maintenance-components.css");
// ACCOUNT_COMPONENTS_CSS_PHASE3B3
pub(super) const ACCOUNT_COMPONENTS_CSS: &str = include_str!("static/css/account-components.css");
// MOBILE_TIMETABLE_CSS_PHASE3B2
pub(super) const MOBILE_TIMETABLE_CSS: &str = include_str!("static/css/mobile-timetable.css");
// MOBILE_NAVIGATION_CSS_PHASE3B1
pub(super) const MOBILE_NAVIGATION_CSS: &str = include_str!("static/css/mobile-navigation.css");
// DASHBOARD_MODULE_LOADER_PHASE1
pub(super) const DASHBOARD_MODULE_CSS: &str = include_str!("static/css/dashboard.css");
// DASHBOARD_LATE_FIXES_CSS_PHASE1_V1
pub(super) const DASHBOARD_LATE_FIXES_CSS: &str =
    include_str!("static/css/dashboard-late-fixes.css");
// DASHBOARD_CSS_CLEANUP_PHASE2
pub(super) const DASHBOARD_MOBILE_CSS: &str = include_str!("static/css/dashboard-mobile.css");
pub(super) const DASHBOARD_COMPONENTS_CSS: &str =
    include_str!("static/css/dashboard-components.css");
pub(super) const DASHBOARD_ACCESS_CSS: &str = include_str!("static/css/dashboard-access.css");
pub(super) const DASHBOARD_RUNTIME_JS: &str = include_str!("static/js/dashboard-runtime.js");
pub(super) const DASHBOARD_FEATURES_JS: &str = include_str!("static/js/dashboard-modules.js");
// DASHBOARD_UI_BUNDLE_V1：Dashboard 常駐小模組合併成一支，減少冷啟動請求。
// 順序：原本 body 同步腳本先、再接原本 head defer 的 header/more 系列。
// 執行時機改為單一 defer（parse 完、所有 inline 之後）；各模組等
// `tronclass:dashboard:ready`／DOMContentLoaded，不依賴 parse-time 插入點。
pub(super) const DASHBOARD_UI_JS: &str = concat!(
    include_str!("static/js/dashboard-permissions.js"),
    "\n",
    include_str!("static/js/dashboard-cookie-badge.js"),
    "\n",
    include_str!("static/js/dashboard-more-buttons.js"),
    "\n",
    include_str!("static/js/dashboard-more-links.js"),
    "\n",
    include_str!("static/js/dashboard-modal.js"),
    "\n",
    include_str!("static/js/dashboard-more-pages.js"),
    "\n",
    include_str!("static/js/dashboard-live-alerts.js"),
    "\n",
    include_str!("static/js/dashboard-header-spacious.js"),
    "\n",
    include_str!("static/js/dashboard-header-more.js"),
    "\n",
    include_str!("static/js/dashboard-notify-purge.js"),
    "\n",
    include_str!("static/js/dashboard-more-actions.js"),
    "\n",
    include_str!("static/js/dashboard-desktop-size.js"),
);
// 只合併「dashboard.html 專用、且排在 dashboard.css 之後」的 CSS。
// mobile-navigation.css 給 homework + mobile-nav.js；dashboard.css /
// dashboard-late-fixes.css 給 login.html——不能併進 dashboard-only bundle。
// mobile-timetable / account-components 仍單獨載入（原本在 dashboard.css 之前）。
pub(super) const DASHBOARD_EXTRAS_CSS: &str = concat!(
    include_str!("static/css/dashboard-mobile.css"),
    "\n",
    include_str!("static/css/maintenance-components.css"),
    "\n",
    include_str!("static/css/dashboard-components.css"),
    "\n",
    include_str!("static/css/dashboard-access.css"),
);

pub(super) const ADMIN_LOGS_HTML: &str = include_str!("static/logs.html");
pub(super) const ADMIN_HEALTH_HTML: &str = include_str!("static/health.html");
pub(super) const ADMIN_DASHBOARD_HTML: &str = include_str!("static/dashboard.html");
pub(super) const ADMIN_LOGIN_HTML: &str = include_str!("static/login.html");
pub(super) const ADMIN_ACTIVITY_HTML: &str = include_str!("static/activity.html");
pub(super) const ADMIN_PASSWORD_HTML: &str = include_str!("static/password.html");
pub(super) const ADMIN_PASSKEY_HTML: &str = include_str!("static/passkey.html");
pub(super) const ADMIN_BLACKLIST_HTML: &str = include_str!("static/blacklist.html");
pub(super) const ADMIN_BLOCKED_HTML: &str = include_str!("static/blocked.html");
// ADMIN_GEO_RESTRICT_V1：與 ADMIN_BLOCKED_HTML（IP 黑名單）是兩種不同原因、
// 兩個不同頁面——這個是「國外 IP 被擋」專用，文案不談黑名單，避免使用者誤以為
// 自己的帳號出了問題。
pub(super) const ADMIN_GEO_BLOCKED_HTML: &str = include_str!("static/geo_blocked.html");
pub(super) const ADMIN_HOMEWORK_HTML: &str = include_str!("static/homework.html");
pub(super) const ADMIN_FILES_HTML: &str = include_str!("static/files.html");
pub(super) const ADMIN_SETTINGS_HTML: &str = include_str!("static/settings.html");
pub(super) const ADMIN_NOTIFICATIONS_HTML: &str = include_str!("static/notifications.html");
pub(super) const APP_SETTINGS_JS: &str = include_str!("static/js/app-settings.js");
pub(super) const NOTIFICATIONS_PAGE_JS: &str = include_str!("static/js/notifications-page.js");

/// Add shared frontend assets without copying about 50 KiB of CSS/JavaScript
/// into every HTML response. The security script intentionally remains
/// parser-blocking: it must install its Web Storage guards before legacy body
/// scripts execute. Shared URLs are browser-cacheable by their route handlers.
/// Build-time asset version. Static assets are served with
/// `immutable, max-age=604800`, so a stale `?v=` means the browser will not
/// even revalidate for a week -- edited JS/CSS silently keeps running the old
/// copy. Stamping it from the build makes every rebuild bust the cache
/// automatically instead of relying on hand-edited version strings.
pub(super) fn asset_version() -> &'static str {
    option_env!("TRONCLASS_BUILD_STAMP").unwrap_or("dev")
}

fn stamp_asset_urls(html: &str) -> String {
    // Rewrite every ?v=<token> that belongs to an /admin/static/ URL.
    let stamp: String = asset_version()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if stamp.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("?v=") {
        out.push_str(&rest[..pos]);
        out.push_str("?v=");
        let after = &rest[pos + 3..];
        let end = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(after.len());
        let token = &after[..end];
        // Leave functional markers such as ?v=auth / ?v=logout alone.
        if token.starts_with("2026") {
            out.push_str(&stamp);
        } else {
            out.push_str(token);
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

pub(super) fn inject_theme(html: &str) -> String {
    // FAST_PAGE_LOAD_V1: preload + put CSS at the start of <head> so parsing
    // the large inline dashboard scripts does not delay theme.css.
    //
    // APP_SETTINGS_BOOT_V1: app-settings.js 排在最前面、先於 theme.css 執行，
    // 在 body 解析前把 data-theme / data-accent / data-fontscale / data-motion
    // 掛到 <html>，使用者選的主題不會在載入時閃爍（FOUC）。
    // 外部 <script src> 為同源 'self'，符合 CSP script-src 'self'。
    //
    // APP_SETTINGS_DYNAMIC_CSS_V1: 依 <html data-app-settings> 屬性套用的外觀覆蓋。
    // 排在 theme.css link 之後：兩者皆為 !important 時，同 specificity 由順序決勝；
    // accent 規則再用雙屬性選擇器拉高 specificity，穩定壓過 theme.css 的
    // :root / html[data-theme] 區塊與其手機 media-query 覆蓋。
    // （此 inline <style> 由 html_page_response() 以 sha256 雜湊放行。）
    // 注意：concat!() 只接受字面值（literal），不能展開 const，因此全部內聯在此。
    //
    // PAGE_BOOT_WATCHDOG_V1：放在 <head> 最前面、任何同步外部 <script> 之前的內聯看門狗。
    // 若 20 秒後解析器仍卡在 <head>（readyState 還是 'loading'、<body> 尚未建立），代表某支
    // 同步資產／被暫停的 service worker 把整頁卡死（iOS 主畫面 App 喚醒時的典型白畫面），
    // 自動 reload 一次取代「滑掉 App 重開」。同一分頁 5 分鐘內只會自救一次，慢速網路不會
    // 進入無限重載；正常載入時計時器到期前 readyState 早已離開 'loading'，完全不動作。
    // （此 inline <script> 由 html_page_response() 以 sha256 雜湊放行；不碰 innerHTML。）
    const SHARED_ASSETS: &str = concat!(
        r#"<script id="PAGE_BOOT_WATCHDOG_V1">(function(){var K='tronclass_boot_watchdog_v1';try{var last=Number(sessionStorage.getItem(K)||0);if(last&&(Date.now()-last)<300000)return}catch(_){}setTimeout(function(){if(document.readyState!=='loading'||document.body)return;try{sessionStorage.setItem(K,String(Date.now()))}catch(_){}location.reload()},20000)})();</script>"#,
        // PWA_IOS_META_V1：所有 admin 頁共用的主畫面 App meta（子頁各自的 <link rel="manifest">
        // 重複無害；apple-touch-icon 用不透明 180px PNG，iOS 不吃 SVG）。
        r#"<link rel="manifest" href="/manifest.webmanifest">"#,
        // PWA_ICON_FULL_BLEED_V1：apple-touch-icon 不帶 sizes（Safari 只認一張 180 滿版圖，多餘屬性
        // 會讓部分版本走「依裝置挑尺寸」的舊路徑）；另補 rel="icon" PNG，Safari 分享面板／書籤
        // 找不到 favicon 時會退回「小圖墊白底」的顯示方式。
        r#"<link rel="apple-touch-icon" href="/admin/pwa/apple-touch-icon.png?v=pwaicon9">"#,
        r#"<link rel="icon" type="image/png" sizes="192x192" href="/admin/pwa/icon-192.png?v=pwaicon9">"#,
        r#"<meta name="apple-mobile-web-app-capable" content="yes">"#,
        r#"<meta name="mobile-web-app-capable" content="yes">"#,
        r#"<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent">"#,
        r#"<meta name="apple-mobile-web-app-title" content="Tronclass">"#,
        r#"<script src="/admin/static/app-settings.js?v=20260902settings10"></script>"#,
        r#"<link rel="preload" href="/admin/static/theme.css?v=20260821fast" as="style">"#,
        r#"<link rel="preload" href="/admin/static/js/dompurify.min.js?v=20260826dompurify3414" as="script">"#,
        r#"<script src="/admin/static/js/dompurify.min.js?v=20260826dompurify3414"></script>"#,
        r#"<script src="/admin/static/js/trusted-types-init.js?v=20260823trustedtypes"></script>"#,
        // TT_INLINE_GUARD_V1（fix29）＋TT_FRAME_GUARD_V1（fix37）：2026-09-14 實測
        // /admin/homework 回報「Failed to set innerHTML … requires 'TrustedHTML'
        // assignment」——外部 trusted-types-init.js 因任何原因未執行（快取損毀、
        // 網路中斷、瀏覽器擴充攔截…）時頁面沒有 default policy，所有 innerHTML
        // 整頁掛掉。這段 inline（隨 HTML 本體到達、經 html_page_response 以
        // sha256 雜湊放行）在外部腳本缺席時建立 pass-through policy（與 files.html、
        // shares-admin.js 的 identity policy 先例一致；createScriptURL 保留
        // same-origin 檢查）。外部腳本正常時 defaultPolicy 已存在，本段不動作。
        //
        // ⚠️ 順序不可調換：本段必須排在 trusted-types-init.js「之後」。提早執行會
        // 先建出 pass-through policy，trusted-types-init.js 看到 defaultPolicy 已
        // 存在就 return——DOMPurify 消�生效。（static_check §19 會驗順序。）
        //
        // fix37（WebKit 回報）：客戶端錯誤「TypeError: This assignment requires a
        // TrustedHTML」是 Safari/WebKit 的字串（WebKit 原始碼
        // makeString("This assignment requires a ", trustedTypeToString(type))；
        // Chrome 是 "Failed to set the 'innerHTML' property on 'Element': This
        // document requires 'TrustedHTML' assignment"）。WebKit 查 default policy
        // 時看的是「被寫入文件」所屬 realm 的 trustedTypes 工廠：父頁面有 policy
        // 不代表 about:blank／sandboxed 子文件有，所以第三方注入碼（實務上是
        // Cloudflare JavaScript Detections）寫子文件時照樣拋錯——面板自己的
        // innerHTML 全部走同一條 default policy，不會產生這個訊息。這裡做兩件事：
        //   ① 標記狀態（window.__ttGuardV1 = present／missing／unsupported），
        //      client-log.js 會把狀態附在錯誤訊息後，health.rs 據此區分「已知
        //      第三方噪音」與「我方真的沒有 policy」（後者仍以 ERROR 記錄）。
        //   ② 對同源子文件補裝同一條 default policy（掃描既有 iframe ＋
        //      MutationObserver 監看新增者；跨域 frame 取用 trustedTypes 會拋
        //      錯，已包在 try 內；不影響面板自身寫入路徑）。
        r#"<script id="TT_INLINE_GUARD_V1">(function(){
function install(cw){try{if(!cw||!cw.trustedTypes||cw.trustedTypes.defaultPolicy)return;cw.trustedTypes.createPolicy('default',{createHTML:function(s){return String(s)},createScriptURL:function(s){var u=new URL(String(s),location.origin);if(u.origin!==location.origin)throw new TypeError('Untrusted script URL blocked');return u.href}})}catch(_){}}
install(window);
try{window.__ttGuardV1=window.trustedTypes?(window.trustedTypes.defaultPolicy?'present':'missing'):'unsupported'}catch(_){}
function scanFrames(){try{var list=document.querySelectorAll('iframe');for(var i=0;i<list.length;i++){install(list[i].contentWindow)}}catch(_){}}
scanFrames();
try{var mo=new MutationObserver(function(records){for(var i=0;i<records.length;i++){var added=records[i].addedNodes;for(var j=0;j<added.length;j++){var node=added[j];if(node&&node.tagName==='IFRAME'){install(node.contentWindow)}}}});mo.observe(document.documentElement||document,{childList:true,subtree:true});setTimeout(function(){try{mo.disconnect()}catch(_){}},60000)}catch(_){}
})();</script>"#,

        r#"<link rel="preload" href="/admin/static/session-security.js?v=20260829nopwbtn" as="script">"#,
        r#"<script id="sessionSecurityHardeningV1" src="/admin/static/session-security.js?v=20260829nopwbtn"></script>"#,
        r#"<link rel="stylesheet" href="/admin/static/theme.css?v=20260821fast">"#,
        r#"<script defer src="/admin/static/web-push.js?v=20260825push"></script>"#,
        r#"<script defer src="/admin/static/live-events.js?v=20260908live"></script>"#,
        // CLIENT_ERROR_REPORT_V1：前端錯誤回報（defer；未登入頁由 login_page_response 剝掉）。
        r#"<script defer src="/admin/static/client-log.js?v=20260909clientlog"></script>"#,
        // URL_SAFETY_SHARED_V1：同源 http(s) 網址驗證的共用正本（原本 dashboard.html／
        // health.html／dashboard-live-alerts.js／rollcall-pending-fab.js 各自抄一份）。
        // 排在下面兩支共用模組之前，兩者都會用到它。
        r#"<script defer src="/admin/static/url-safety.js?v=20261005urlsafety1"></script>"#,
        // ROLLCALL_DETECT_SHARED_V1：三份一模一樣（其實已經悄悄分岔）的「偵測到點名／
        // 算未送出人數」判斷邏輯（dashboard.html、rollcall-pending-fab.js、health.html
        // 各自一份）收成這支共用模組。必須排在下面 rollcall-pending-fab.js「之前」——
        // defer 腳本保證照文件順序依序執行，fab.js 會直接呼叫這支模組的函式。
        r#"<script defer src="/admin/static/rollcall-detect.js?v=20261005detect1"></script>"#,
        // ROLLCALL_PENDING_FAB_V1：使用者要求「不管在哪個頁面只要有點名就要顯示」，所以
        // 跟 live-events.js／client-log.js 一樣掛在全站共用層，每個 admin 頁都會載入
        // （未登入頁由 login_page_response 剝掉，邏輯跟 live-events.js 一致）。
        // ROLLCALL_FAB_STUCK_FETCH_FIX_V1：版號 fab1→fab2，避免瀏覽器/CDN 快取舊版（有卡死
        // inFlight 那個 bug 的版本）——不改版號的話，部署新檔案後使用者端可能還是吃到舊快取。
        r#"<script defer src="/admin/static/rollcall-pending-fab.js?v=20261005fab2"></script>"#,
        r#"<style id="appSettingsDynamicCss">
html[data-app-settings][data-theme="dark"]{color-scheme:dark!important}
html[data-app-settings][data-theme="light"]{color-scheme:light!important}
html[data-app-settings][data-accent="blue"]{--primary:#2563EB!important}
html[data-app-settings][data-accent="indigo"]{--primary:#6366F1!important}
html[data-app-settings][data-accent="green"]{--primary:#059669!important}
html[data-app-settings][data-accent="amber"]{--primary:#D97706!important}
html[data-app-settings][data-accent="purple"]{--primary:#7C3AED!important}
html[data-app-settings][data-accent="rose"]{--primary:#E11D48!important}
html[data-app-settings][data-accent="cyan"]{--primary:#0E7490!important}
html[data-app-settings][data-theme="dark"][data-accent="blue"]{--primary:#5B9BFF!important}
html[data-app-settings][data-theme="dark"][data-accent="indigo"]{--primary:#818CF8!important}
html[data-app-settings][data-theme="dark"][data-accent="green"]{--primary:#34D399!important}
html[data-app-settings][data-theme="dark"][data-accent="amber"]{--primary:#FBBF24!important}
html[data-app-settings][data-theme="dark"][data-accent="purple"]{--primary:#A78BFA!important}
html[data-app-settings][data-theme="dark"][data-accent="rose"]{--primary:#FB7185!important}
html[data-app-settings][data-theme="dark"][data-accent="cyan"]{--primary:#22D3EE!important}
html[data-app-settings][data-fontscale="lg"] body{zoom:1.08}
html[data-app-settings][data-fontscale="xl"] body{zoom:1.16}
html[data-app-settings][data-motion="reduced"] *,html[data-app-settings][data-motion="reduced"] *::before,html[data-app-settings][data-motion="reduced"] *::after{animation-duration:.001ms!important;animation-iteration-count:1!important;transition-duration:.001ms!important;scroll-behavior:auto!important}
.app-settings-tab.active{background:var(--card)!important;color:var(--text)!important;border-color:var(--line)!important}
/* 使用者要求:隱藏 Dashboard 右上角的重新整理圖示(#refreshAll)。
   用 CSS 隱藏而非刪除 markup:dashboard 內聯 script 有未加防護的
   q('refreshAll').onclick=...,刪除元素會讓整段腳本拋錯掛掉。 */
#refreshAll{display:none!important}
/* SHARED_MOBILE_NAV_V3: 共用手機導覽列（mobile-nav.js 建立）之字級統一與深色模式。
   寫在此處（隨每頁 HTML 注入、無快取問題），保證所有頁面一致。 */
#mobileMoreFinalV2 .more-grid a,#mobileMoreFinalV2 .more-grid button{font-size:15px!important;font-weight:900!important;min-height:48px!important}
#mobileNavFinalV2 button{background:transparent!important;border:0!important;box-shadow:none!important}
html[data-app-settings][data-theme="dark"] #mobileNavFinalV2{background:rgba(15,23,42,.94)!important;border-color:rgba(148,163,184,.18)!important}
html[data-app-settings][data-theme="dark"] #mobileNavFinalV2 button{background:transparent!important;color:#94a3b8!important;border:0!important;box-shadow:none!important}
html[data-app-settings][data-theme="dark"] #mobileNavFinalV2 button.active{background:rgba(96,165,250,.18)!important;color:#93c5fd!important}
html[data-app-settings][data-theme="dark"] #mobileMoreFinalV2 .sheet{background:rgba(22,29,41,.97)!important;border-color:rgba(148,163,184,.18)!important}
html[data-app-settings][data-theme="dark"] #mobileMoreFinalV2 h3{color:#f1f5f9!important}
html[data-app-settings][data-theme="dark"] #mobileMoreFinalV2 .more-grid a,html[data-app-settings][data-theme="dark"] #mobileMoreFinalV2 .more-grid button{background:rgba(30,41,59,.65)!important;color:#f1f5f9!important;border-color:#334155!important}
html[data-app-settings][data-theme="dark"] #mobileMoreFinalV2 .more-grid .danger{background:rgba(248,113,113,.12)!important;color:#fca5a5!important;border-color:rgba(248,113,113,.35)!important}
@media(prefers-color-scheme:dark){
html:not([data-theme="light"]) #mobileNavFinalV2{background:rgba(15,23,42,.94)!important;border-color:rgba(148,163,184,.18)!important}
html:not([data-theme="light"]) #mobileNavFinalV2 button{background:transparent!important;color:#94a3b8!important;border:0!important;box-shadow:none!important}
html:not([data-theme="light"]) #mobileMoreFinalV2 .sheet{background:rgba(22,29,41,.97)!important;border-color:rgba(148,163,184,.18)!important}
html:not([data-theme="light"]) #mobileMoreFinalV2 h3{color:#f1f5f9!important}
html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid a,html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid button{background:rgba(30,41,59,.65)!important;color:#f1f5f9!important;border-color:#334155!important}
html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid .danger{background:rgba(248,113,113,.12)!important;color:#fca5a5!important;border-color:rgba(248,113,113,.35)!important}
}
/* 保險:若瀏覽器/service worker 仍快取舊版 app-settings.js 並把「設定」
   連結塞進 header,此規則(每次導頁都隨 no-cache HTML 刷新)保證將其隱藏。 */
#appSettingsGearLink,.app-settings-gear{display:none!important}
</style>"#
    );
    let assets = SHARED_ASSETS.to_string();
    if let Some(head_at) = html.find("<head") {
        if let Some(rel) = html[head_at..].find('>') {
            let at = head_at + rel + 1;
            let mut out = String::with_capacity(html.len() + assets.len());
            out.push_str(&html[..at]);
            out.push_str(&assets);
            out.push_str(&html[at..]);
            return stamp_asset_urls(&out);
        }
    }
    stamp_asset_urls(&html.replacen("</head>", &(assets + "</head>"), 1))
}
pub(super) async fn admin_dashboard_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：與 `/admin` 相同的三態；未登入導回登入頁並帶 next。
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/dashboard");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    let html = inject_auth_state(dashboard_html_themed(), &page.session);
    attach_fresh_cookies(html_page_response(&headers, html), &page, &headers)
}

/// LOGIN_PERF_V1：Dashboard 的 `inject_theme` 結果與 session 無關（共用資產標籤＋版號改寫，
/// 對 ~150 KB 的 HTML 做多次字串掃描與配置），只算一次；每個請求只剩 `inject_auth_state`
/// （只改 `<html>` 開頭標籤）與 `html_page_response`（雜湊 CSP／ETag）。
fn dashboard_html_themed() -> &'static str {
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_DASHBOARD_HTML));
    THEMED.as_str()
}

pub(super) async fn admin_activity_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：原本是公開殼＋前端檢查；改為伺服器端驗證（含 refresh 續期）。
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/activity");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_ACTIVITY_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

pub(super) async fn admin_homework_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/homework");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    // HOMEWORK_FILE_V1（fix43）：作業頁的「預覽」需要 PDF.js 與 office-view.js
    // （+JSZip）——沿用教材頁同一支按需載入器（`window.__tronEnsureFileLib`），
    // 不把 ~485KB 的重庫塞進每個頁面的同步載入路徑。
    static THEMED: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        inject_files_office_assets(&inject_theme(ADMIN_HOMEWORK_HTML))
    });
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

// FILES_NATIVE_V1
// 廢除 FileBrowser：/admin/files 改為直接服務同源的「教材雲端硬碟」頁
// （static/files.html），不再重導向到外部 files 主機。教材存取一律走
// /admin/api/files/* 的原有端點，因此網路與 Dashboard 同源、不需跨 app。
pub(super) async fn admin_files_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/files");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        inject_files_office_assets(&inject_theme(ADMIN_FILES_HTML))
    });
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

// SHARE_ADMIN_V1: 分享連結管理頁（僅管理員）。列出所有已發連結、可撤銷。
pub(super) async fn admin_shares_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/shares");
    };
    if !page.session.is_admin {
        return (StatusCode::FORBIDDEN, "僅管理員可存取").into_response();
    }
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    // THEME_AWARE_V1: 套用與 dashboard 相同的主題資產（theme.css + app-settings.js），
    // 讓分享管理頁背景與深淺色設定與主介面一致。
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(SHARES_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

// PREVIEW_ENGINES_V1: 只在「雲端硬碟」頁注入 PDF.js（同源 'self'，合法於 CSP），
// 不要無差別注入到每個 admin 頁面。
// 預覽引擎：PDF/Office(轉PDF後) → PDF.js；Office 未轉時 → Microsoft Office Online。
// PREVIEW_ENGINES_V1: 只在「雲端硬碟」頁注入 PDF.js（同源 'self'，合法於 CSP），
// 不要無差別注入到每個 admin 頁面。
// 預覽引擎：PDF/Office(轉PDF後) → PDF.js；Office 未轉時 → Microsoft Office Online。
// FILES_LAZY_LIB_V1: 四大重庫（jszip/office-view/pdf.min/qrcode，共約 485KB）不再
// 於進頁時同步載入，改由前端依需求動態載入（按 pdf / office / qrcode 分組）：
//   只有真的打開 PDF / Office 預覽 / 產生分享 QR 時才抓取對應腳本；已載入者用
//   promise 快取避免重複。pdf.worker.min.js（~1.08MB）本來就只在渲染 PDF 時才抓，
//   維持不變。此變更可讓 files 頁冷啟動減少約 485KB 的同步下載與解析。
pub(super) fn inject_files_office_assets(html: &str) -> String {
    // 版本號必須隨內容更新：static_assets.rs 對靜態檔下
    // `max-age=604800, immutable`，不換 query string 瀏覽器會沿用舊檔一週。
    const FILELIB_LOADER: &str = r#"<script id="tronLazyFileLibV1">
(function(){
  'use strict';
  if (window.__tronEnsureFileLib) return;
  var loaded = {};
  function loadOne(url){
    return new Promise(function(res, rej){
      var s = document.createElement('script');
      s.src = url;
      s.async = false;              // 依序執行；office-view 依賴 JSZip
      s.onload = function(){ res(); };
      s.onerror = function(){ rej(new Error('load ' + url + ' failed')); };
      document.head.appendChild(s);
    });
  }
  function ensure(name){
    if (loaded[name]) return Promise.resolve();
    if (loaded.__p && loaded.__p[name]) return loaded.__p[name];
    var urls = {
      pdf:     ['/admin/static/js/pdf.min.js?v=pdfjs1'],
      office:  ['/admin/static/js/jszip.min.js?v=officeview6','/admin/static/js/office-view.js?v=officeview11'],
      qrcode:  ['/admin/static/js/qrcode.min.js?v=qrshare1']
    };
    var list = urls[name];
    if (!list) return Promise.resolve();
    var chain = Promise.resolve();
    list.forEach(function(u){ chain = chain.then(function(){ return loadOne(u); }); });
    var p = chain.then(function(){ loaded[name] = true; });
    if (!loaded.__p) loaded.__p = {};
    loaded.__p[name] = p;
    return p;
  }
  window.__tronEnsureFileLib = ensure;
})();
</script>"#;
    // OFFICE_INJECT_GUARD_V1: 用「真正的 <script id=...> 標籤」判斷是否已注入，
    // 避免重複加上 loader（不能用裸檔名子字串，files.html 註解也會提到檔名）。
    if html.contains(r#"id="tronLazyFileLibV1""#) {
        return html.to_string();
    }
    if let Some(head_at) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + FILELIB_LOADER.len());
        out.push_str(&html[..head_at]);
        out.push_str(FILELIB_LOADER);
        out.push_str(&html[head_at..]);
        out
    } else {
        html.replacen("<head>", &format!("<head>{FILELIB_LOADER}"), 1)
    }
}

const ADMIN_CLEAN_TIMETABLE_HTML: &str = include_str!("static/timetable.html");

pub(super) async fn admin_clean_timetable_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    // ADMIN_PAGE_TRI_STATE_V1：伺服器端驗證（含 refresh 續期），未登入導回登入頁。
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/timetable");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_CLEAN_TIMETABLE_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

// CREDITS_EMIS_V1：學分狀態獨立頁（/admin/credits）。
const CREDITS_PAGE_HTML: &str = include_str!("static/credits.html");
pub(super) const CREDITS_PAGE_JS: &str = include_str!("static/js/credits.js");

pub(super) async fn admin_credits_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/credits");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(CREDITS_PAGE_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

/// SEO_LANDING_V1（fix60）：`GET /` 的分流。
///
/// 需求原話：「我希望能在google打關鍵字就能搜尋到我的網站」。
/// 舊行為是 `/` 一律 302 到 `/admin`，而 `/admin` 未登入時只會回登入頁——
/// Google 看到的永遠是一面登入牆，任何關鍵字都不會有排名。
///
/// 現在依身分分流（對既有使用者完全無感）：
/// - 帶 `?token=`（無 cookie 的舊式呼叫）→ 照舊轉送 `/admin`（token 由 redirect 剝除）
/// - 有有效 session cookie → 照舊 302 到 `/admin`
/// - 其他（訪客、搜尋引擎）→ 回公開介紹頁 `static/landing.html`
///
/// 注意：這裡**不能**把 session 資訊或面板內容帶進介紹頁；介紹頁是純靜態內容。
pub(super) async fn admin_root_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    if session_token_from_uri(&uri).is_some() {
        return admin_root_redirect_handler(uri).await;
    }
    let token = session_token(&headers, "");
    if state.authorize(&token).await.is_some() {
        return admin_root_redirect_handler(uri).await;
    }
    seo::landing_page_response(&headers)
}

pub(super) async fn admin_root_redirect_handler(uri: axum::http::Uri) -> Response {
    // TOKEN_QUERY_STRIP_ALL_ADMIN_V1 (H-2): `/` is outside the scrubbing
    // middleware's `/admin` scope, so forwarding the query verbatim would
    // bounce an attacker-supplied `?token=` straight into the address bar of
    // `/admin`. Drop `token` (and the transient `logout` marker is preserved,
    // since the login shell reads it) while forwarding everything else.
    let loc = match uri.query() {
        Some(q) if !q.is_empty() => {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            let mut kept = 0usize;
            for (name, value) in url::form_urlencoded::parse(q.as_bytes()) {
                if name == "token" {
                    continue;
                }
                serializer.append_pair(&name, &value);
                kept += 1;
            }
            if kept == 0 {
                "/admin".to_string()
            } else {
                format!("/admin?{}", serializer.finish())
            }
        }
        _ => "/admin".to_string(),
    };
    (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, loc)]).into_response()
}

pub(super) async fn admin_password_page_handler(headers: HeaderMap) -> Response {
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_PASSWORD_HTML));
    html_page_response(&headers, THEMED.clone())
}

/// /admin/notifications — 通知設定獨立頁（NOTIFICATIONS_STANDALONE_PAGE_V1）。
/// 原 Dashboard「通知」分頁移除，通知偏好/上課提醒/提示音集中於此頁。
pub(super) async fn admin_notifications_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/notifications");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_NOTIFICATIONS_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

/// /admin/settings — 外觀與本機偏好設定頁。
/// 比照 dashboard：伺服器端先驗證 session，未登入導回登入頁並帶 next 路徑。
pub(super) async fn admin_settings_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        return admin_login_redirect("/admin/settings");
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_SETTINGS_HTML));
    attach_fresh_cookies(html_page_response(&headers, inject_auth_attrs(THEMED.as_str(), &page.session)), &page, &headers)
}

pub(super) async fn admin_passkey_page_handler(headers: HeaderMap) -> Response {
    static THEMED: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| inject_theme(ADMIN_PASSKEY_HTML));
    html_page_response(&headers, THEMED.clone())
}

// ── ADMIN_PAGE_TRI_STATE_V1 ──────────────────────────────────────────────────
//
// 伺服器端依 session 狀態決定 `/admin` 回哪一頁，前端不再「先畫登入卡再用 JS 判斷」：
//   1. 無 session（access、refresh 都無效）      → 登入頁（static/login.html）。
//   2. access 有效                                → Dashboard，並把驗證狀態注入 HTML。
//   3. access 過期／遺失但 refresh cookie 仍有效  → 伺服器端輪換一次（與
//      `/admin/api/session/refresh` 同一條 `rotate_session`），在回應上帶新 cookie，
//      直接回 Dashboard；使用者完全不會看到登入頁閃一下。
// 其他子頁（作業／檔案／設定…）共用 `resolve_page_session`，同樣享有第 3 態。
//
// 注入方式：`<html … data-auth="ok" data-account="<id>" data-admin="1|0" data-viewer="1|0">`
// 加 class `tron-auth-ok dash-auth-ok`（沿用既有 CSS 閘門 class），首幀即為已登入版面。

/// 已驗證的頁面 session；`fresh_cookies` 在第 3 態時帶新 token，回應必須附上 Set-Cookie。
pub(super) struct PageSession {
    pub(super) session: AdminSession,
    pub(super) fresh_cookies: Option<SessionTokens>,
}

/// 解析頁面請求的 session（access → refresh 輪換）。回 `None` 表示未登入。
///
/// SESSION_DUAL_CANDIDATE_V1（fix36）：access 與 refresh 都改為「**兩個 cookie
/// 名字都試**」。舊版只讀 priority 最高的那一顆，於是「過期的 `__Host-`
/// 殘留＋剛種下的 local」這種並存狀態下，每次都拿舊 token 去驗／去輪換，
/// 必定失敗 → 使用者看到「登入成功後又跳回登入畫面」。
pub(super) async fn resolve_page_session(
    state: &AdminWebState,
    headers: &HeaderMap,
    query_token: &str,
) -> Option<PageSession> {
    let token = session_token(headers, query_token);
    if !token.is_empty() {
        if let Some(session) = state.authorize_any(&token).await {
            return Some(PageSession {
                session,
                fresh_cookies: None,
            });
        }
    }
    // 同一次請求可能同時帶著兩個名字的 cookie；逐一試到有效的那一顆為止。
    for candidate in super::session_cookie_candidates(headers) {
        if candidate == token {
            continue;
        }
        if let Some(session) = state.authorize_any(&candidate).await {
            return Some(PageSession {
                session,
                fresh_cookies: None,
            });
        }
    }
    // access 無效：試 refresh cookie（兩個名字都試；只在有 cookie 時才碰 DB）。
    //
    // 頁面 handler 沒有 peer address（ConnectInfo）可用：這裡的 origin 只會拿到 UA，
    // ip 為空字串；`rotate_session` 只用它「回填」DB 內仍為空的欄位（or_fill），
    // 不會覆蓋原始登入來源，因此裝置頁看到的仍是同一台裝置。
    for refresh in super::refresh_cookie_candidates(headers) {
        let Some(tokens) = state
            .rotate_session(&refresh, Some(SessionOrigin::from_request(headers, "")))
            .await
        else {
            continue;
        };
        let Some(session) = state.authorize_any(&tokens.access).await else {
            continue;
        };
        return Some(PageSession {
            session,
            fresh_cookies: Some(tokens),
        });
    }
    None
}

/// 把第 3 態輪換出的新 cookie 附到頁面回應上（含 Filebrowser SSO 重簽）。
pub(super) fn attach_fresh_cookies(
    response: Response,
    page: &PageSession,
    headers: &HeaderMap,
) -> Response {
    let Some(tokens) = page.fresh_cookies.as_ref() else {
        return response;
    };
    let response = append_session_cookie(response, &tokens.access, headers);
    let response = append_refresh_cookie(response, &tokens.refresh, headers);
    if page.session.must_change_password || page.session.account_id == "<token-admin>" {
        return response;
    }
    fb_sso::append_fb_sso_cookie(
        response,
        &page.session.account_id,
        page.session.is_admin,
        &tokens.access,
        headers,
    )
}

fn html_attr_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 在 `<html …>` 上注入驗證狀態，並把 `tron-auth-pending` 換成 `tron-auth-ok dash-auth-ok`。
pub(super) fn inject_auth_state(html: &str, session: &AdminSession) -> String {
    let Some(open_at) = html.find("<html") else {
        return html.to_string();
    };
    let Some(rel_close) = html[open_at..].find('>') else {
        return html.to_string();
    };
    let close_at = open_at + rel_close;
    let tag = &html[open_at..=close_at];
    let mut new_tag = tag.replace("tron-auth-pending", "tron-auth-ok dash-auth-ok");
    if !new_tag.contains("tron-auth-ok") {
        // 沒有 class 屬性時補一個。
        new_tag = new_tag.replacen("<html", "<html class=\"tron-auth-ok dash-auth-ok\"", 1);
    }
    let new_tag = format!("{}{}>", &new_tag[..new_tag.len() - 1], auth_attrs(session));
    format!("{}{}{}", &html[..open_at], new_tag, &html[close_at + 1..])
}

/// LOGIN_PERF_V1：`<html>` 上的驗證資料屬性（Dashboard 與所有子頁共用同一組）。
fn auth_attrs(session: &AdminSession) -> String {
    format!(
        " data-auth=\"ok\" data-account=\"{}\" data-admin=\"{}\" data-viewer=\"{}\"",
        html_attr_escape(&session.account_id),
        if session.is_admin { "1" } else { "0" },
        if session.is_viewer { "1" } else { "0" },
    )
}

/// LOGIN_PERF_V1：子頁版本——只把 `data-auth / data-account / data-admin / data-viewer` 注入
/// `<html>`，**不動 class**（子頁的版面 class 由各頁自己的開機腳本／mobile-nav.js 控制，
/// 直接加 `dash-auth-ok` 會改變 theme.css 對 `main` 的桌機寬度規則）。
///
/// 有了這組屬性，mobile-nav.js（要 is_admin）、activity／files／logs／health／timetable 的
/// 開機驗證、notifications／settings 的 verifySession、session-security.js 的首登檢查都不必
/// 再各打一次 `/admin/api/status`——伺服器送出這頁時 session 已經驗證過了。
pub(super) fn inject_auth_attrs(html: &str, session: &AdminSession) -> String {
    let Some(open_at) = html.find("<html") else {
        return html.to_string();
    };
    let Some(rel_close) = html[open_at..].find('>') else {
        return html.to_string();
    };
    let close_at = open_at + rel_close;
    format!("{}{}{}", &html[..close_at], auth_attrs(session), &html[close_at..])
}

/// 登入頁（無 session 時的 `/admin`）。走 `inject_theme` 以取得 theme.css / app-settings /
/// session-security 等共用資產；HTML 是靜態的，`html_page_response` 會給 ETag 與雜湊 CSP。
fn login_page_response(headers: &HeaderMap) -> Response {
    // LOGIN_PERF_V1：登入頁沒有任何 per-session 內容，inject_theme（字串拼接＋版號改寫）
    // 的結果全程相同，只做一次。雜湊 CSP／ETag 由 html_page_response 依本體計算（可 304）。
    // 登入頁用不到：web-push.js（推播訂閱要已登入）、live-events.js（未登入時 SSE 只會
    // 401 後重試三次）、rollcall-pending-fab.js（同樣要打一次 /admin/api/status，
    // 未登入一定 401，沒意義先載）、dompurify + trusted-types-init（登入頁沒有任何
    // innerHTML 寫入；session-security.js 的密碼視窗只在已登入頁的按鈕觸發）。連同
    // 對應的 preload 一起拿掉，首次開登入頁少幾個請求與對應的解析時間。
    static LOGIN_HTML_THEMED: once_cell::sync::Lazy<String> = once_cell::sync::Lazy::new(|| {
        let themed = inject_theme(ADMIN_LOGIN_HTML);
        strip_asset_tags(
            &themed,
            &[
                "/admin/static/web-push.js",
                "/admin/static/live-events.js",
                "/admin/static/client-log.js",
                "/admin/static/url-safety.js",
                "/admin/static/rollcall-detect.js",
                "/admin/static/rollcall-pending-fab.js",
                "/admin/static/js/dompurify.min.js",
                "/admin/static/js/trusted-types-init.js",
            ],
        )
    });
    html_page_response(headers, LOGIN_HTML_THEMED.clone())
}

/// LOGIN_PERF_V1：移除引用指定資產的 `<script … src="<prefix>…"></script>` 與
/// `<link … href="<prefix>…">`（preload）標籤。只比對 URL 前綴，版號 `?sv=` 不影響；
/// 找不到就原樣回傳。
pub(super) fn strip_asset_tags(html: &str, url_prefixes: &[&str]) -> String {
    let mut out = html.to_string();
    for prefix in url_prefixes {
        out = strip_tag_by_attr(&out, "<script", &format!("src=\"{prefix}"), Some("</script>"));
        out = strip_tag_by_attr(&out, "<link", &format!("href=\"{prefix}"), None);
    }
    out
}

/// 把「開頭為 `open_tag`、屬性含 `attr_needle`」的元素整個移除；`close_tag` 為 Some 時
/// 連同內容與結尾標籤一起移除，None 表示 void 元素（只移除到第一個 `>`）。
/// 安全網：屬性必須位於同一個開頭標籤內（open 與屬性之間不能出現 `>`），否則
/// （例如 inline script 字串裡剛好有同樣的 URL）跳過該處繼續往後找。
fn strip_tag_by_attr(html: &str, open_tag: &str, attr_needle: &str, close_tag: Option<&str>) -> String {
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find(attr_needle) {
        let attr_at = cursor + rel;
        let candidate = html[cursor..attr_at].rfind(open_tag).map(|r| cursor + r);
        let Some(open_at) = candidate.filter(|&o| !html[o..attr_at].contains('>')) else {
            // 不是我們要的標籤：原樣保留到屬性之後，繼續找。
            let skip_to = attr_at + attr_needle.len();
            out.push_str(&html[cursor..skip_to]);
            cursor = skip_to;
            continue;
        };
        let end = match close_tag {
            Some(close) => match html[attr_at..].find(close) {
                Some(r) => attr_at + r + close.len(),
                None => break,
            },
            None => match html[attr_at..].find('>') {
                Some(r) => attr_at + r + 1,
                None => break,
            },
        };
        out.push_str(&html[cursor..open_at]);
        cursor = end;
    }
    out.push_str(&html[cursor..]);
    out
}

pub(super) async fn admin_page_handler(
    State(state): State<AdminWebState>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some(page) = resolve_page_session(&state, &headers, &query.token).await else {
        // SESSION_DIAG_V1（fix33）：帶著（無效）cookie 卻被送到登入頁＝登入循環
        // 的特徵——記錄 cookie 存在性幫助定位（完全沒 cookie 的自然訪客保持
        // 安靜，避免 log 噪音）。has_access/has_refresh 皆 true 仍失敗＝兩個
        // token 都被撤銷（rotate 競態或重啟清空）；只有其一＝cookie 不對稱。
        let has_access = session_cookie_from_headers(&headers).is_some();
        let has_refresh = refresh_cookie_from_headers(&headers).is_some();
        if has_access || has_refresh {
            tracing::warn!(
                has_access_cookie = has_access,
                has_refresh_cookie = has_refresh,
                has_next_param = query.next.is_some(),
                "login page served with invalid cookies (possible login loop)"
            );
        }
        // 第 1 態：沒有任何有效 session → 登入頁。
        return login_page_response(&headers);
    };
    if page.session.must_change_password {
        return attach_fresh_cookies(must_change_password_redirect(), &page, &headers);
    }
    // 已登入卻帶著 ?next=（例如子頁把人送來時 access 剛好已由 refresh 續期）：直接去目的地。
    if let Some(next) = query.next.as_deref().and_then(safe_next_path) {
        return attach_fresh_cookies(
            (StatusCode::FOUND, [(header::LOCATION, next)]).into_response(),
            &page,
            &headers,
        );
    }
    // 第 2 / 3 態：Dashboard，驗證狀態直接注入 HTML。
    let html = inject_auth_state(dashboard_html_themed(), &page.session);
    attach_fresh_cookies(html_page_response(&headers, html), &page, &headers)
}

#[cfg(test)]
pub(super) fn safe_next_path_for_test(raw: &str) -> Option<String> {
    safe_next_path(raw)
}

/// 只接受站內 `/admin…` 相對路徑（不含 scheme / host / `//`），避免 open redirect。
fn safe_next_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() > 512 || !raw.starts_with("/admin") || raw.starts_with("//") {
        return None;
    }
    if raw.contains("://") || raw.contains('\\') || raw.chars().any(char::is_control) {
        return None;
    }
    let path = raw.split(['?', '#']).next().unwrap_or("");
    if path == "/admin/logout" || path.starts_with("/admin/api/") || path.starts_with("/admin/static/") {
        return None;
    }
    if path == "/admin" || path == "/admin/" {
        return None;
    }
    Some(raw.to_string())
}
