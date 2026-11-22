/* URL_SAFETY_SHARED_V1：「這個字串解析出來是不是可以放心拿去用的 http(s) 網址」
 * 共用判斷。
 *
 * 背景：同一套邏輯（用 `new URL(v, location.origin)` 解析，只接受 `http:`／
 * `https:`，解析失敗或協定不對就回空字串——避免 `javascript:`／`data:`
 * 之類的協定被塞進 `<a href>`／`location.href` 造成注入）被複製了 4 份，
 * 命名還不一致：
 *   - `dashboard.html`：`safeHttpUrl()`
 *   - `health.html`：`safeHttpUrl()`
 *   - `dashboard-live-alerts.js`：`safeUrl()`
 *   - `rollcall-pending-fab.js`：`safeUrl()`
 * 這 4 份目前邏輯仍完全一致（還沒像 rollcall 分組判斷那樣分岔），但這種
 * 安全性相關的小工具一旦將來要補強（例如多擋幾種協定），4 處分散就很容易漏改
 * 一處。收成這份正本，4 邊都改成呼叫它（各自保留原本的函式名稱與呼叫點不動，
 * 只改函式本體委派出去，降低改動風險）。
 *
 * 由 pages.rs 的 inject_theme() 注入，跟 rollcall-detect.js 一樣全站 admin 頁
 * 都會載入，且已登記進 asset_versions.rs 的 ASSETS 清單（吸取 rollcall-
 * pending-fab.js 當初漏登記、靠 ?v= 手動版號实際上不會生效的教訓）。
 */
(function () {
  'use strict';
  if (window.TronclassUrlSafety) return;

  function safeHttpUrl(v) {
    try {
      var u = new URL(String(v || ''), location.origin);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return '';
      return u.href;
    } catch (_) {
      return '';
    }
  }

  window.TronclassUrlSafety = { safeHttpUrl: safeHttpUrl };
})();
