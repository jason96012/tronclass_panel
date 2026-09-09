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
//!
//! PWA_STATIC_CACHE_V3 — 在上述前提下「只對非 iOS」重新加入一層極窄的靜態資產快取：
//! - 只攔同源 GET、路徑在 `/admin/static/`、`/pub/static/` 之下、且帶 `?sv=`（build id）
//!   的請求；HTML 導覽、`/admin/api/*`、SSE、manifest、service-worker.js 本身一律不碰。
//! - 策略：cache-first（`?sv=` 內容不可變，命中即回）；未命中走網路並在 200 時寫入
//!   `tron-static-v3`。舊 build 的項目在 activate 與每次寫入時順手清掉（同路徑不同 sv）。
//! - iOS／iPadOS 的 SW 完全不註冊 fetch 監聽（`self.navigator.userAgent` 判斷）：
//!   WebKit 的 SW 行程暫停問題（見上）正是發生在有 fetch handler 時，那條路維持 V2 行為。
//!   非 iOS 瀏覽器（Chrome／Edge／Firefox／Android）沒有這個問題，離線時靜態資產也能秒開。
//! - 非 iOS 頁面由 web-push.js 的 boot 在閒置時註冊 SW（原本只有 iOS 啟用推播時才會註冊）。

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
        // PWA_ICON_FULL_BLEED_V1：iOS 17+／macOS Sonoma+ 的 Safari 會優先讀 manifest icons
        // （即使頁面有 apple-touch-icon）。purpose 含 "any" 的圖示會被 Safari 當成「形狀不定」
        // → 縮小後墊在白色方塊上（使用者看到的「照片小小一張在白框裡」就是這個）；
        // 只有 purpose 純 "maskable" 才會滿版填滿再套系統圓角。因此：
        //   1. maskable 條目放最前面、且不與 any 合寫（"any maskable" 會被當 any 處理）；
        //   2. SVG 從 manifest 拿掉（Safari 會偏好 sizes:any 的 SVG，蓋過 PNG；路由保留給舊連結）；
        //   3. any 條目留給 Windows／舊 Android 等不套遮罩的平台。
        "icons": [
            {
                "src": "/admin/pwa/icon-512.png?v=pwaicon9",
                "sizes": "512x512",
                "type": "image/png",
                "purpose": "maskable"
            },
            {
                "src": "/admin/pwa/icon-192.png?v=pwaicon9",
                "sizes": "192x192",
                "type": "image/png",
                "purpose": "maskable"
            },
            {
                "src": "/admin/pwa/icon-512.png?v=pwaicon9",
                "sizes": "512x512",
                "type": "image/png",
                "purpose": "any"
            },
            {
                "src": "/admin/pwa/icon-192.png?v=pwaicon9",
                "sizes": "192x192",
                "type": "image/png",
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
// Tronclass PWA service worker PWA_NO_FETCH_INTERCEPT_V2 + PWA_STATIC_CACHE_V3.
// iOS: push notifications only. There is deliberately NO fetch handler and NO
// CacheStorage on iOS: intercepting /admin/static/* made iOS home-screen
// launches hang on a suspended service worker (blank white page until the app
// was force-quit).
// Non-iOS: a narrow cache-first layer for immutable versioned static assets
// (/admin/static/* and /pub/static/* carrying ?sv=<build id>). HTML, /admin/api,
// SSE, manifest and this file are never intercepted.
const STATIC_CACHE = 'tron-static-v3';
const IS_IOS = /iPad|iPhone|iPod/.test(self.navigator.userAgent || '')
  || (/Macintosh/.test(self.navigator.userAgent || '') && (self.navigator.maxTouchPoints || 0) > 1);
function cacheableStaticRequest(request) {
  if (request.method !== 'GET') return null;
  let url;
  try { url = new URL(request.url); } catch (_) { return null; }
  if (url.origin !== self.location.origin) return null;
  const p = url.pathname;
  if (!(p.startsWith('/admin/static/') || p.startsWith('/pub/static/'))) return null;
  if (!url.searchParams.get('sv')) return null;
  return url;
}
self.addEventListener('install', event => {
  event.waitUntil(self.skipWaiting());
});
self.addEventListener('activate', event => {
  event.waitUntil((async () => {
    // Evict every cache created by earlier versions (tron-shell-*, tron-static-v1/v2).
    // On iOS nothing is retained on disk at all.
    try {
      const keys = await caches.keys();
      await Promise.all(keys.filter(k => IS_IOS || k !== STATIC_CACHE).map(k => caches.delete(k)));
    } catch (_) {}
    await self.clients.claim();
  })());
});
if (!IS_IOS) {
  self.addEventListener('fetch', event => {
    const url = cacheableStaticRequest(event.request);
    if (!url) return; // fall through to the network / HTTP cache untouched
    event.respondWith((async () => {
      let cache = null;
      try {
        cache = await caches.open(STATIC_CACHE);
        const hit = await cache.match(event.request, { ignoreVary: true });
        if (hit) return hit;
      } catch (_) {}
      const response = await fetch(event.request);
      if (cache && response && response.status === 200 && response.type === 'basic') {
        try {
          const copy = response.clone();
          event.waitUntil((async () => {
            // Drop older builds of the same path (different ?sv=) before storing.
            try {
              const stale = (await cache.keys()).filter(req => {
                try { const u = new URL(req.url); return u.pathname === url.pathname && u.search !== url.search; }
                catch (_) { return false; }
              });
              await Promise.all(stale.map(req => cache.delete(req)));
            } catch (_) {}
            await cache.put(event.request, copy);
          })());
        } catch (_) {}
      }
      return response;
    })());
  });
}
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
// PUSH_GONE_VISIBLE_V1：推播服務輪替端點時（iOS/Android 都會發生），自動以同一組 VAPID
// 公鑰重新訂閱並告訴伺服器「舊端點 → 新端點」。少了這段，舊端點下次發送就是 410 →
// 訂閱被移除 → 使用者得手動再開一次。fetch 走同源 cookie（不需 token）。
self.addEventListener('pushsubscriptionchange', event => {
  event.waitUntil((async () => {
    const oldSub = event.oldSubscription || null;
    const oldEndpoint = oldSub ? oldSub.endpoint : '';
    let key = null;
    try {
      if (oldSub && oldSub.options && oldSub.options.applicationServerKey) {
        key = oldSub.options.applicationServerKey;
      } else {
        const res = await fetch('/admin/api/push/config', { credentials: 'same-origin', cache: 'no-store' });
        const cfg = await res.json();
        const raw = atob(String(cfg.public_key || '').replace(/-/g, '+').replace(/_/g, '/'));
        key = Uint8Array.from(raw, ch => ch.charCodeAt(0));
      }
      const fresh = event.newSubscription
        || await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      const json = fresh.toJSON();
      await fetch('/admin/api/push/rotate', {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ old_endpoint: oldEndpoint, new_subscription: json })
      });
    } catch (_) {
      // 失敗時不做事：舊端點會在下次發送時被 PUSH_STALE_CLEANUP_V1 清掉並提醒使用者。
    }
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
