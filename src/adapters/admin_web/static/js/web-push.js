(function () {
  'use strict';

  var api = '/admin/api/push';
  var bindingAccountKey = 'tronclass_web_push_explicit_account_v2';

  function isIos() {
    return /iPad|iPhone|iPod/.test(navigator.userAgent) ||
      (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  }
  function isStandalone() {
    return navigator.standalone === true ||
      (window.matchMedia && window.matchMedia('(display-mode: standalone)').matches);
  }
  function decodeKey(value) {
    var padding = '='.repeat((4 - value.length % 4) % 4);
    var raw = atob((value + padding).replace(/-/g, '+').replace(/_/g, '/'));
    return Uint8Array.from(raw, function (ch) { return ch.charCodeAt(0); });
  }
  async function jsonFetch(path, options) {
    var response = await fetch(path, Object.assign({
      credentials: 'same-origin', cache: 'no-store'
    }, options || {}));
    var body = await response.json().catch(function () { return {}; });
    if (!response.ok) throw new Error(body.error || ('HTTP ' + response.status));
    return body;
  }
  function assertSupported() {
    if (!window.isSecureContext) throw new Error('手機推播只能在 HTTPS 網址使用。');
    if (!isIos()) throw new Error('目前此功能只開放 iPhone / iOS。');
    if (!isStandalone()) {
      throw new Error('請先在 Safari 點「分享」→「加入主畫面」，再從主畫面的 Tronclass App 開啟並按啟用。');
    }
    if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) {
      throw new Error('這台 iPhone 不支援背景推播；需要 iOS 16.4 或更新版本。');
    }
  }
  async function registration() {
    await navigator.serviceWorker.register('/service-worker.js', { scope: '/' });
    return navigator.serviceWorker.ready;
  }
  async function saveSubscription(subscription) {
    var serialized = subscription.toJSON();
    if (!serialized.keys || !serialized.keys.p256dh || !serialized.keys.auth) {
      throw new Error('iPhone 沒有提供完整的推播金鑰。');
    }
    return jsonFetch(api + '/subscribe', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(serialized)
    });
  }
  async function subscribe() {
    assertSupported();
    // iOS requires the permission prompt to stay directly associated with the
    // user's tap. Ask before awaiting a network request.
    var permission = Notification.permission;
    if (permission !== 'granted') permission = await Notification.requestPermission();
    if (permission !== 'granted') throw new Error('通知權限未允許；請到 iPhone「設定」→「通知」開啟此 App 的通知與聲音。');
    var config = await jsonFetch(api + '/config');
    var reg = await registration();
    var subscription = await reg.pushManager.getSubscription();
    if (!subscription) {
      subscription = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: decodeKey(config.public_key)
      });
    }
    await saveSubscription(subscription);
    try { localStorage.setItem(bindingAccountKey, String(config.account_id || '')); } catch (_) {}
    var testError = '';
    var sent = 0;
    try {
      var result = await jsonFetch(api + '/test', { method: 'POST' });
      sent = result.sent || 0;
      if (result.ok === false) testError = result.error || 'Apple 測試推播未送達';
    } catch (error) {
      testError = error && error.message ? error.message : '測試推播請求失敗';
    }
    return { accountId: config.account_id, sent: sent, testError: testError, delaySecs: (result && result.delay_secs) || 10 };
  }
  async function unsubscribe() {
    assertSupported();
    var reg = await registration();
    var subscription = await reg.pushManager.getSubscription();
    if (!subscription) {
      try { localStorage.removeItem(bindingAccountKey); } catch (_) {}
      return false;
    }
    await jsonFetch(api + '/unsubscribe', {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ endpoint: subscription.endpoint })
    });
    await subscription.unsubscribe();
    try { localStorage.removeItem(bindingAccountKey); } catch (_) {}
    return true;
  }
  async function syncExisting() {
    // Deliberately do not POST the existing endpoint here. A browser has one
    // subscription per PWA origin; auto-saving it after another account logs in
    // silently transfers ownership. Binding is allowed only from subscribe().
    return;
  }
  function explicitlyBoundAccount() {
    try { return localStorage.getItem(bindingAccountKey) || ''; } catch (_) { return ''; }
  }
  async function enableFromButton(button) {
    var original = button && button.textContent;
    if (button) { button.disabled = true; button.textContent = '啟用中…'; }
    try {
      var result = await subscribe();
      if (result.testError) {
        alert('已為帳號「' + result.accountId + '」儲存 iPhone 推播訂閱。\n\n但測試通知未送達：\n' + result.testError + '\n\n請截圖這段詳細訊息以便檢查。');
        if (button) button.textContent = '推播已啟用（測試失敗）';
      } else {
        alert('已為帳號「' + result.accountId + '」啟用 iPhone 背景推播。\n\n請關閉這個訊息後，立刻把 Tronclass App 往上滑掉；約 ' + result.delaySecs + ' 秒後會收到背景測試通知。\n\n通知聲音由 iOS「設定 → 通知」中的聲音開關控制。');
        if (button) button.textContent = '背景測試已安排';
      }
    } catch (error) {
      alert(error && error.message ? error.message : '手機推播啟用失敗。');
      if (button) button.textContent = original;
    } finally {
      if (button) button.disabled = false;
    }
  }

  function ensureStatusManagementLink() {
    var passkey = document.getElementById('passkeyRegisterBtn');
    if (!passkey || !passkey.parentNode) return false;
    var link = document.getElementById('pushStatusManagementLink');
    if (!link) {
      link = document.createElement('a');
      link.id = 'pushStatusManagementLink';
      link.href = '/admin/push-status';
      link.textContent = '推播狀態管理';
    }
    link.className = 'button secondary push-status-desktop-link';
    if (passkey.nextElementSibling !== link) passkey.insertAdjacentElement('afterend', link);
    if (!document.getElementById('pushStatusDesktopLinkStyle')) {
      /* CSP_DYNAMIC_STYLE_NONCE_V1 */
      var style = window.tronCreateStyle ? window.tronCreateStyle('pushStatusDesktopLinkStyle', '') : document.createElement('style');
      style.id = 'pushStatusDesktopLinkStyle';
      style.textContent = '#pushStatusManagementLink{display:inline-flex;align-items:center;justify-content:center;min-height:36px;height:36px;padding:0 12px;border-radius:999px;white-space:nowrap;text-decoration:none;font-size:13px;font-weight:760}@media(max-width:820px){#pushStatusManagementLink{display:none!important}}';
      document.head.appendChild(style);
    }
    return true;
  }

  document.addEventListener('click', function (event) {
    var button = event.target && event.target.closest &&
      event.target.closest('#morePushFinalV2,#rollcallQuickAlertNotify,[data-enable-web-push]');
    if (!button) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    enableFromButton(button);
  }, true);

  window.TronclassWebPush = {
    subscribe: subscribe,
    unsubscribe: unsubscribe,
    syncExisting: syncExisting,
    explicitlyBoundAccount: explicitlyBoundAccount
  };
  // PWA_STATIC_CACHE_V3：非 iOS 瀏覽器也註冊 service worker（只快取帶 ?sv= 的靜態資產；
  // HTML／API 不攔）。iOS 維持原本「啟用推播時才註冊」——避免 WebKit SW 暫停造成白畫面。
  function registerStaticCacheWorker() {
    if (isIos()) return;
    if (!window.isSecureContext || !('serviceWorker' in navigator)) return;
    var run = function () {
      navigator.serviceWorker.register('/service-worker.js', { scope: '/' }).catch(function () {});
    };
    if ('requestIdleCallback' in window) window.requestIdleCallback(run, { timeout: 4000 });
    else setTimeout(run, 1500);
  }
  function boot() {
    syncExisting();
    ensureStatusManagementLink();
    registerStaticCacheWorker();
    var attempts = 0;
    var timer = setInterval(function () {
      attempts += 1;
      // The passkey button is injected asynchronously by the Dashboard.
      ensureStatusManagementLink();
      if (attempts >= 30) clearInterval(timer);
    }, 400);
  }
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot, { once: true });
  } else {
    boot();
  }
}());
