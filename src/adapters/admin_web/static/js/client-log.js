/* CLIENT_ERROR_REPORT_V1
 *
 * 前端錯誤回報：把瀏覽器端的 window.onerror / unhandledrejection / CSP 違規、
 * 以及 PAGE_BOOT_WATCHDOG_V1 的「自動重載」事件，送到 POST /admin/api/client-log，
 * 由伺服器寫進系統日誌（logs 頁可見、Pterodactyl console 也看得到）。
 *
 * 設計：
 *   - 只在已登入的頁面生效（伺服器端要 session；未登入 401 就靜默丟棄）。
 *   - 每頁最多送 20 筆、同一訊息 60 秒內只送一次，避免錯誤迴圈灌爆。
 *   - 用 navigator.sendBeacon（頁面卸載時也能送出），不支援時退回 fetch keepalive。
 *   - 不送 cookie 以外的任何識別；訊息、堆疊都在伺服器端截斷與過濾控制字元。
 *   - PAGE_BOOT_WATCHDOG_V1 在 sessionStorage 留下的重載紀錄（tronclass_boot_watchdog_v1）
 *     於下一次載入時補送一筆「boot_watchdog_reload」，讓看門狗真的被觸發時有跡可循。
 */
(function () {
  'use strict';
  if (window.CLIENT_ERROR_REPORT_V1) return;
  window.CLIENT_ERROR_REPORT_V1 = true;

  var ENDPOINT = '/admin/api/client-log';
  var MAX_PER_PAGE = 20;
  var DEDUPE_MS = 60000;
  var WATCHDOG_KEY = 'tronclass_boot_watchdog_v1';
  var WATCHDOG_REPORTED_KEY = 'tronclass_boot_watchdog_reported_v1';
  var sent = 0;
  var recent = Object.create(null);

  function loggedIn() {
    try {
      var html = document.documentElement;
      return html.getAttribute('data-auth') === 'ok' || html.classList.contains('tron-auth-ok');
    } catch (_) { return false; }
  }
  function clip(value, max) {
    var text = String(value == null ? '' : value);
    return text.length > max ? text.slice(0, max) + '…' : text;
  }
  function post(kind, message, extra) {
    if (sent >= MAX_PER_PAGE) return;
    var key = kind + '|' + clip(message, 120);
    var now = Date.now();
    if (recent[key] && (now - recent[key]) < DEDUPE_MS) return;
    recent[key] = now;
    sent += 1;
    var payload = {
      kind: clip(kind, 32),
      message: clip(message, 1000),
      page: clip(location.pathname + location.hash, 200),
      stack: clip(extra && extra.stack, 2000),
      source: clip(extra && extra.source, 300),
      line: Number(extra && extra.line) || 0,
      col: Number(extra && extra.col) || 0,
      ua: clip(navigator.userAgent, 200),
      standalone: !!(navigator.standalone === true || (window.matchMedia && matchMedia('(display-mode: standalone)').matches)),
      visibility: document.visibilityState || ''
    };
    var body = JSON.stringify(payload);
    try {
      if (navigator.sendBeacon) {
        var blob = new Blob([body], { type: 'application/json' });
        if (navigator.sendBeacon(ENDPOINT, blob)) return;
      }
    } catch (_) {}
    try {
      fetch(ENDPOINT, {
        method: 'POST', credentials: 'same-origin', keepalive: true, cache: 'no-store',
        headers: { 'content-type': 'application/json' }, body: body
      }).catch(function () {});
    } catch (_) {}
  }

  function install() {
    if (!loggedIn()) return;
    window.addEventListener('error', function (event) {
      if (!event) return;
      // 資源載入失敗（<script>/<img> 404）走 capture 階段、沒有 error 物件。
      if (event.target && event.target !== window && !event.error) {
        var el = event.target;
        var src = el.src || el.href || '';
        if (src) post('resource_error', (el.tagName || 'RESOURCE').toLowerCase() + ' 載入失敗', { source: src });
        return;
      }
      var err = event.error;
      post('js_error', event.message || (err && err.message) || 'script error', {
        stack: err && err.stack, source: event.filename, line: event.lineno, col: event.colno
      });
    }, true);
    window.addEventListener('unhandledrejection', function (event) {
      var reason = event && event.reason;
      var message = reason && (reason.message || reason.error || reason.statusText) || String(reason || 'unhandled rejection');
      // 401/403 的 API 拒絕是正常流程（session 過期→refresh），不回報。
      if (reason && (reason.status === 401 || reason.status === 403)) return;
      post('unhandled_rejection', message, { stack: reason && reason.stack });
    });
    document.addEventListener('securitypolicyviolation', function (event) {
      if (!event) return;
      var enforced = event.disposition !== 'report';
      post(enforced ? 'csp_violation' : 'csp_report_only', (event.violatedDirective || event.effectiveDirective || 'csp') + (enforced ? ' 阻擋 ' : ' 回報（未阻擋）') + (event.blockedURI || 'inline'), {
        source: event.sourceFile, line: event.lineNumber, col: event.columnNumber,
        stack: clip(event.sample || '', 200)
      });
    });
    // PAGE_BOOT_WATCHDOG_V1 的自動重載事件：上一輪載入卡在 <head> 超過 20 秒後 reload。
    try {
      var last = Number(sessionStorage.getItem(WATCHDOG_KEY) || 0);
      var reported = Number(sessionStorage.getItem(WATCHDOG_REPORTED_KEY) || 0);
      if (last && last !== reported) {
        sessionStorage.setItem(WATCHDOG_REPORTED_KEY, String(last));
        post('boot_watchdog_reload', '頁面啟動看門狗於 ' + new Date(last).toISOString() + ' 觸發自動重載（<head> 卡住超過 20 秒）');
      }
    } catch (_) {}
  }

  window.TronclassClientLog = { report: post };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', install, { once: true });
  } else {
    install();
  }
})();
