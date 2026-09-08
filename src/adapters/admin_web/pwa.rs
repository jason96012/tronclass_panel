//! PWA assets (manifest / service worker / icon) for Admin Web.
//!
//! PWA_SHELL_CACHE_V1 — App-shell caching:
//! the service worker now **pre-caches the dashboard app shell** (every
//! `/admin/static/*` CSS/JS asset) during `install` and serves those assets
//! with *stale-while-revalidate* (instant paint from cache, fresh copy fetched
//! in the background). This matches the project's existing HTTP caching
//! philosophy (`stale-while-revalidate=86400` on static assets) and makes an
//! installed PWA start instantly even on a slow upstream.
//!
//! Safety rules (see static/README_SAFE_UPDATES.md) are preserved:
//! - authenticated **HTML pages are never cached** — no stale Dashboard after
//!   a server restart, no authenticated content persisted on disk;
//! - `/admin/api/*` **always hits the network** — no data interception;
//! - **no redirects** are ever issued from the service worker.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;

/// PWA_SHELL_CACHE_V1 — single source of truth for the app-shell asset list.
///
/// Must stay in sync with the `/admin/static/*` routes registered in
/// `build_admin_router()` (mod.rs). Assets injected by `pages.rs`
/// (`theme.css`, `session-security.js`, `dompurify.min.js`, …) and lazily
/// loaded libraries (`pdf.min.js`, `office-view.js`, `jszip.min.js`, …) are
/// included so the whole shell — not just the eagerly referenced subset —
/// survives an offline start.
const SHELL_ASSET_URLS: &[&str] = &[
    "/admin/static/theme.css",
    "/admin/static/session-security.js",
    "/admin/static/app-settings.js",
    "/admin/static/notifications-page.js",
    "/admin/static/core-api.js",
    "/admin/static/web-push.js",
    "/admin/static/live-events.js",
    "/admin/static/js/dompurify.min.js",
    "/admin/static/js/trusted-types-init.js",
    "/admin/static/js/jszip.min.js",
    "/admin/static/js/office-view.js",
    "/admin/static/js/qrcode.min.js",
    "/admin/static/js/shares-admin.js",
    "/admin/static/js/dashboard-particles.js",
    "/admin/static/js/dashboard-login-card.js",
    "/admin/static/js/dashboard-live-alerts.js",
    "/admin/static/js/dashboard-permissions.js",
    "/admin/static/js/dashboard-cookie-badge.js",
    "/admin/static/js/dashboard-modal.js",
    "/admin/static/js/dashboard-header-more.js",
    "/admin/static/js/dashboard-more-buttons.js",
    "/admin/static/js/dashboard-more-links.js",
    "/admin/static/js/dashboard-more-pages.js",
    "/admin/static/js/dashboard-notify-purge.js",
    "/admin/static/js/dashboard-desktop-size.js",
    "/admin/static/js/dashboard-header-spacious.js",
    "/admin/static/js/dashboard-more-actions.js",
    "/admin/static/js/mobile-nav.js",
    "/admin/static/js/dashboard-onboarding.js",
    "/admin/static/js/pdf.min.js",
    "/admin/static/js/pdf.worker.min.js",
    "/admin/static/accounts.js",
    "/admin/static/mock-rollcall.js",
    "/admin/static/timetable.js",
    "/admin/static/maintenance-implementation.js",
    "/admin/static/maintenance.js",
    "/admin/static/maintenance-components.css",
    "/admin/static/account-components.css",
    "/admin/static/mobile-timetable.css",
    "/admin/static/mobile-navigation.css",
    "/admin/static/dashboard.css",
    "/admin/static/dashboard-late-fixes.css",
    "/admin/static/dashboard-mobile.css",
    "/admin/static/dashboard-components.css",
    "/admin/static/dashboard-access.css",
    "/admin/static/dashboard-runtime.js",
    "/admin/static/dashboard-modules.js",
];

/// Cache-name version stamp. Bump it when the shell asset *set* changes
/// (stale-while-revalidate already refreshes asset *contents* in the
/// background, so a bump is only needed to evict entries for removed URLs).
const SHELL_CACHE_NAME: &str = "tron-shell-v20260908b";

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
        "background_color": "#f7f8fb",
        "icons": [
            {
                "src": "/admin/pwa/icon.svg",
                "sizes": "any",
                "type": "image/svg+xml",
                "purpose": "any maskable"
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
// Tronclass PWA service worker PWA_SHELL_CACHE_V1.
// The app shell (/admin/static/*) is pre-cached on install and served with
// stale-while-revalidate. HTML and /admin/api/* always hit the network and
// authenticated pages are deliberately never retained on disk.
const SHELL_CACHE = '__SHELL_CACHE__';
const SHELL_ASSETS = __SHELL_ASSETS__;
self.addEventListener('install', event => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL_CACHE);
    // Pre-cache each asset individually: one missing/failed URL must not
    // abort the whole install (a partial shell still beats no shell).
    await Promise.all(SHELL_ASSETS.map(async asset => {
      try {
        const res = await fetch(asset, { cache: 'no-store' });
        if (res && res.ok) await cache.put(asset, res);
      } catch (_) {}
    }));
    await self.skipWaiting();
  })());
});
self.addEventListener('activate', event => {
  event.waitUntil((async () => {
    // Authenticated HTML is deliberately never retained. Removing every old
    // cache (including legacy tron-static-* names) prevents a stale Dashboard
    // from appearing after a server restart and evicts removed shell assets.
    const keep = [SHELL_CACHE];
    const keys = await caches.keys();
    await Promise.all(keys.filter(k => keep.indexOf(k) < 0).map(k => caches.delete(k)));
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
self.addEventListener('fetch', event => {
  const req = event.request;
  if (req.method !== 'GET') return;
  let url;
  try { url = new URL(req.url); } catch (_) { return; }
  if (url.origin !== self.location.origin) return;
  if (url.pathname.indexOf('/admin/api/') === 0) return;
  if (/[?&]logout=/.test(url.search)) return;
  if (url.pathname.indexOf('/admin/static/') === 0) {
    // Stale-while-revalidate: paint instantly from cache, refresh in the
    // background. Assets are not content-hashed, so pure cache-first would
    // keep serving old CSS/JS until the cache name is bumped.
    event.respondWith((async () => {
      const cache = await caches.open(SHELL_CACHE);
      const cached = await cache.match(req);
      const revalidate = fetch(req).then(res => {
        if (res && res.ok) { try { cache.put(req, res.clone()); } catch (_) {} }
        return res;
      }).catch(() => null);
      if (cached) return cached;
      const fresh = await revalidate;
      if (fresh) return fresh;
      return new Response('offline', { status: 503, statusText: 'Offline' });
    })());
    return;
  }
});
"#;

    let body = SW
        .replace("__SHELL_CACHE__", SHELL_CACHE_NAME)
        .replace(
            "__SHELL_ASSETS__",
            &serde_json::to_string(SHELL_ASSET_URLS)
                .unwrap_or_else(|_| "[]".to_string()),
        );

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
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
