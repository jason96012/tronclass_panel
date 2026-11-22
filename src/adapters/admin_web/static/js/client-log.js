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
  // TT_ASSIGNMENT_DIAG_V1（fix37）：Safari/WebKit 的 Trusted Types 例外字串是
  // 「TypeError: This assignment requires a TrustedHTML」，Chrome 則是
  // 「…Failed to set the 'innerHTML' property on 'Element': This document requires
  // 'TrustedHTML' assignment」。面板頁面在 <head> 最前面就裝好 default policy
  // （pages.rs 的 TT_INLINE_GUARD_V1），所以這種錯誤通常代表「被寫入的文件」不是
  // 我方頁面（about:blank／sandboxed 子文件、第三方注入碼）。把當下的 guard 狀態
  // 附在訊息尾端，health.rs 才能把這類已知噪音與「我方真的沒有 policy」分開記錄。
  function withTtNote(message) {
    try {
      if (!/requires a Trusted|requires 'Trusted/.test(message)) return message;
      var state = 'unsupported';
      if (window.trustedTypes) {
        state = window.__ttGuardV1 || (window.trustedTypes.defaultPolicy ? 'present' : 'missing');
      }
      return message + ' [tt=' + state + ']';
    } catch (_) { return message; }
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
      post('js_error', withTtNote(event.message || (err && err.message) || 'script error'), {
        stack: err && err.stack, source: event.filename, line: event.lineno, col: event.colno
      });
    }, true);
    window.addEventListener('unhandledrejection', function (event) {
      var reason = event && event.reason;
      var message = reason && (reason.message || reason.error || reason.statusText) || String(reason || 'unhandled rejection');
      // 401/403 的 API 拒絕是正常流程（session 過期→refresh），不回報。
      if (reason && (reason.status === 401 || reason.status === 403)) return;
      post('unhandled_rejection', withTtNote(message), { stack: reason && reason.stack });
    });
    document.addEventListener('securitypolicyviolation', function (event) {
      if (!event) return;
      var dir = String(event.violatedDirective || event.effectiveDirective || '').toLowerCase();
      var blocked = String(event.blockedURI || 'inline');
      var sample = String(event.sample || '');
      var src = String(event.sourceFile || '');
      var enforced = event.disposition !== 'report';
      // CSP_RO_SCRIPT_NO_DUP_V1：Report-Only 與正式 CSP 對 script 會重複報。
      if (!enforced && dir.indexOf('script-src') === 0) return;
      // CSP_CF_INLINE_NOISE_V1：Cloudflare 注入的 inline／challenge 被 hash CSP 擋是預期行為。
      var blob = sample + ' ' + src + ' ' + blocked;
      if (/cdn-cgi|__CF\$cv\$params|email-decode|rocket-loader|challenge-platform|cf-beacon/i.test(blob)) return;
      post(enforced ? 'csp_violation' : 'csp_report_only', dir + (enforced ? ' 阻擋 ' : ' 回報（未阻擋）') + blocked, {
        source: event.sourceFile, line: event.lineNumber, col: event.columnNumber,
        stack: clip(sample, 200)
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
