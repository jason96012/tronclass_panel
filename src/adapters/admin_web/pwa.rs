//! PWA assets (manifest / service worker / icon) for Admin Web.
//!
//! PWA_NO_FETCH_INTERCEPT_V2 — the service worker no longer intercepts any
//! request and no longer keeps a CacheStorage "app shell".
//!
//! Why (iOS 主畫面 App 偶發白畫面，要滑掉重開才會好)：
//! - Dashboard HTML 是 `no-store`，`<head>` 最前面是同步的
//!   `app-settings.js` / `dompurify.min.js` / `trusted-types-init.js` /
//!   `session-security.js` 與 `theme.css`（pages::inject_theme）。
//! - 舊版 PWA_SHELL_CACHE_V1 對 `/admin/static/*` 做 `respondWith(caches.open …)`。
//!   只要 SW 有 fetch handler，scope 內「每一個」請求（包含 /admin/dashboard 這個
//!   導覽本身，即使 handler 最後不 respondWith）都必須先把 SW 行程喚醒、派送 fetch
//!   事件。iOS 主畫面 App 從背景喚醒／冷啟動時，WebKit 長期存在 SW 行程或 CacheStorage
//!   被暫停、事件永遠不回應的問題（bugs.webkit.org 211018 等）；此時不是導覽本身卡住，
//!   就是解析器卡在第一支同步 `<script>`，`<body>` 根本沒被解析，使用者只看到 manifest
//!   `background_color` 的近白畫面。滑掉 App 會連 SW 行程一起結束，重開就正常——
//!   與使用者回報「有時候白畫面，要滑掉再開才會出現」完全吻合。
//! - 這層快取本來就是多餘的：靜態資產已是 `max-age=604800, immutable` 加
//!   `?sv=<build id>`（asset_versions.rs），瀏覽器 HTTP cache 早已秒開；HTML 又從不
//!   快取，所以離線也啟動不了。沒有 fetch handler 時瀏覽器連 SW 都不必喚醒，
//!   導覽與子資源直接走網路／HTTP cache，不再有任何卡住的路徑。
//!
//! 保留：Web Push（`push` / `notificationclick`）。`activate` 會刪掉所有舊的
//! CacheStorage（tron-shell-* / tron-static-*）釋放空間；`skipWaiting` +
//! `clients.claim` 讓已安裝的舊 SW 在下一次成功開啟時立即被換掉。
//!
//! 規則（static/README_SAFE_UPDATES.md 第 6 條）：Service Worker 不要加入
//! fetch interception / cache / redirect。

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;

pub(super) async fn manifest_handler() -> Response {
    let manifest = json!({
        "name": "Tronclass Rollcall",
        "short_name": "Tronclass",
        "description": "Tronclass/iClass 點名監控、教材與管理工具",
        "start_url": "/admin",
        "scope": "/",
        "display": "standalone",
        "orientation": "portrait",
        "theme_color": "#2563eb",
        // PWA_IOS_META_V1：啟動閃屏底色對齊 Dashboard 深色背景（dashboard-login-bg-active
        // 為 #071122），避免每次開 App 先白一下再變深，也讓真正卡住的白畫面更容易辨識。
        "background_color": "#071122",
        // PWA_IOS_META_V1：iOS／Android 主畫面都不吃 SVG 圖示（iOS 會拿網頁截圖當圖示），
        // 補 PNG 192／512；SVG 保留給支援的平台。
        "icons": [
            {
                "src": "/admin/pwa/icon-192.png",
                "sizes": "192x192",
                "type": "image/png",
                "purpose": "any maskable"
            },
            {
                "src": "/admin/pwa/icon-512.png",
                "sizes": "512x512",
                "type": "image/png",
                "purpose": "any maskable"
            },
            {
                "src": "/admin/pwa/icon.svg",
                "sizes": "any",
                "type": "image/svg+xml",
                "purpose": "any"
            }
        ],
        "shortcuts": [
            { "name": "我的頁面", "short_name": "我的", "url": "/admin/me" },
            { "name": "教材", "short_name": "教材", "url": "/admin/files" }
        ]
    });

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/manifest+json; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        manifest.to_string(),
    )
        .into_response()
}

pub(super) async fn service_worker_handler() -> Response {
    const SW: &str = r#"
// Tronclass PWA service worker PWA_NO_FETCH_INTERCEPT_V2.
// Push notifications only. There is deliberately NO fetch handler and NO
// CacheStorage: intercepting /admin/static/* made iOS home-screen launches
// hang on a suspended service worker (blank white page until the app was
// force-quit). Static assets are already immutable + versioned (?sv=) so the
// browser HTTP cache gives an instant paint without any SW involvement.
self.addEventListener('install', event => {
  event.waitUntil(self.skipWaiting());
});
self.addEventListener('activate', event => {
  event.waitUntil((async () => {
    // Evict every cache created by earlier versions (tron-shell-*,
    // tron-static-*). Nothing is retained on disk any more.
    try {
      const keys = await caches.keys();
      await Promise.all(keys.map(k => caches.delete(k)));
    } catch (_) {}
    await self.clients.claim();
  })());
});
self.addEventListener('push', event => {
  event.waitUntil((async () => {
    let payload = {};
    try { payload = event.data ? event.data.json() : {}; } catch (_) {
      payload = { body: event.data ? event.data.text() : '偵測到新的點名' };
    }
    const title = String(payload.title || 'Tronclass 點名通知');
    // PUSH_DEEP_LINK_V1：payload.url 可以是站內相對路徑，或後端產生的 https 輸入頁／
    // 掃碼頁絕對網址（scanner public_base_url 可能與儀表板不同網域）。
    const rawUrl = typeof payload.url === 'string' ? payload.url : '';
    const target = (rawUrl.indexOf('/') === 0 || rawUrl.indexOf('https://') === 0)
      ? rawUrl : '/admin/dashboard#rollcall';
    const options = {
      body: String(payload.body || '偵測到新的點名，請立即查看。'),
      icon: '/admin/pwa/icon.svg',
      badge: '/admin/pwa/icon.svg',
      tag: String(payload.tag || 'tronclass-rollcall'),
      renotify: true,
      requireInteraction: true,
      // Let iOS apply the PWA's system notification sound setting. Explicit
      // silent/vibrate options have had inconsistent WebKit behavior, while
      // the platform default follows 設定 → 通知 → Tronclass → 聲音.
      data: { url: target, dashboard: '/admin/dashboard#rollcall' }
    };
    // 支援 actions 的平台（Android Chrome）多給一顆「開啟儀表板」；iOS 會忽略。
    if (payload.deep_link === true) {
      options.actions = [
        { action: 'open', title: '前往輸入' },
        { action: 'dashboard', title: '開啟儀表板' }
      ];
    }
    await self.registration.showNotification(title, options);
  })());
});
self.addEventListener('notificationclick', event => {
  event.notification.close();
  const data = (event.notification && event.notification.data) || {};
  // PUSH_DEEP_LINK_V1：按「開啟儀表板」動作時改開儀表板；預設點擊走 data.url。
  const target = (event.action === 'dashboard' && data.dashboard)
    ? data.dashboard
    : (data.url || '/admin/dashboard#rollcall');
  event.waitUntil((async () => {
    const absolute = new URL(target, self.location.origin).href;
    const sameOrigin = absolute.indexOf(self.location.origin + '/') === 0;
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of windows) {
      try {
        // WindowClient.navigate 只允許同源；跨網域的輸入頁直接開新視窗。
        if (sameOrigin && 'navigate' in client) {
          await client.navigate(absolute);
          return client.focus();
        }
        if (!sameOrigin) break;
        return client.focus();
      } catch (_) {}
    }
    return self.clients.openWindow(absolute);
  })());
});
"#;

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        SW,
    )
        .into_response()
}

pub(super) async fn icon_handler() -> Response {
    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#22c55e"/>
      <stop offset="0.55" stop-color="#2563eb"/>
      <stop offset="1" stop-color="#8b5cf6"/>
    </linearGradient>
  </defs>
  <rect width="512" height="512" rx="112" fill="#eff6ff"/>
  <rect x="96" y="124" width="210" height="260" rx="26" fill="url(#g)" opacity="0.96"/>
  <rect x="154" y="92" width="210" height="260" rx="26" fill="#2563eb" opacity="0.88"/>
  <rect x="212" y="144" width="210" height="260" rx="26" fill="#0f172a" opacity="0.92"/>
  <path d="M256 226h126M256 276h126M256 326h78" stroke="#fff" stroke-width="28" stroke-linecap="round"/>
</svg>"##;

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        SVG,
    )
        .into_response()
}

/// PWA_IOS_META_V1：PNG 圖示（iOS apple-touch-icon 與 manifest 用）。
/// 與 `/admin/pwa/icon.svg` 同樣是公開、可長快取的靜態資源。
fn png_response(bytes: &'static [u8]) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=604800, immutable"),
        ],
        bytes,
    )
        .into_response()
}

pub(super) async fn icon_192_handler() -> Response {
    png_response(include_bytes!("static/pwa/icon-192.png"))
}

pub(super) async fn icon_512_handler() -> Response {
    png_response(include_bytes!("static/pwa/icon-512.png"))
}

pub(super) async fn apple_touch_icon_handler() -> Response {
    png_response(include_bytes!("static/pwa/apple-touch-icon.png"))
}
