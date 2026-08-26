//! Minimal safe PWA assets for Admin Web.
//!
//! This intentionally does NOT cache/intercept application requests. It only
//! enables installability (manifest + service worker + icon) without changing
//! runtime page behavior.

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
// Tronclass PWA service worker FAST_PAGE_LOAD_V1.
// Only /admin/static/* is cached. HTML and /admin/api/* always hit the network.
const STATIC_CACHE = 'tron-static-v20260826resumeauth';
self.addEventListener('install', event => {
  self.skipWaiting();
});
self.addEventListener('activate', event => {
  event.waitUntil((async () => {
    // Authenticated HTML is deliberately never retained. Removing every old
    // HTML cache prevents a stale Dashboard from appearing after server restart.
    const keep = [STATIC_CACHE];
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
    const target = typeof payload.url === 'string' && payload.url.indexOf('/') === 0
      ? payload.url : '/admin/dashboard#rollcall';
    await self.registration.showNotification(title, {
      body: String(payload.body || '偵測到新的點名，請立即查看。'),
      icon: '/admin/pwa/icon.svg',
      badge: '/admin/pwa/icon.svg',
      tag: String(payload.tag || 'tronclass-rollcall'),
      renotify: true,
      requireInteraction: true,
      // Let iOS apply the PWA's system notification sound setting. Explicit
      // silent/vibrate options have had inconsistent WebKit behavior, while
      // the platform default follows 設定 → 通知 → Tronclass → 聲音.
      data: { url: target }
    });
  })());
});
self.addEventListener('notificationclick', event => {
  event.notification.close();
  const target = event.notification && event.notification.data && event.notification.data.url
    ? event.notification.data.url : '/admin/dashboard#rollcall';
  event.waitUntil((async () => {
    const absolute = new URL(target, self.location.origin).href;
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    for (const client of windows) {
      try {
        if ('navigate' in client) await client.navigate(absolute);
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
    event.respondWith((async () => {
      const cache = await caches.open(STATIC_CACHE);
      const cached = await cache.match(req);
      if (cached) return cached;
      const res = await fetch(req);
      if (res && res.ok) { try { await cache.put(req, res.clone()); } catch (_) {} }
      return res;
    })());
    return;
  }
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
