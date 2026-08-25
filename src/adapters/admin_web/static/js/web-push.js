(function () {
  'use strict';

  var api = '/admin/api/push';

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
    var config = await jsonFetch(api + '/config');
    var permission = Notification.permission;
    if (permission !== 'granted') permission = await Notification.requestPermission();
    if (permission !== 'granted') throw new Error('通知權限未允許；請到 iPhone「設定」→「通知」開啟此 App 的通知與聲音。');
    var reg = await registration();
    var subscription = await reg.pushManager.getSubscription();
    if (!subscription) {
      subscription = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: decodeKey(config.public_key)
      });
    }
    await saveSubscription(subscription);
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
    if (!subscription) return false;
    await jsonFetch(api + '/unsubscribe', {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ endpoint: subscription.endpoint })
    });
    await subscription.unsubscribe();
    return true;
  }
  async function syncExisting() {
    if (!isIos() || !isStandalone() || Notification.permission !== 'granted') return;
    try {
      var reg = await registration();
      var subscription = await reg.pushManager.getSubscription();
      if (subscription) await saveSubscription(subscription);
    } catch (_) {}
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
    syncExisting: syncExisting
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', syncExisting, { once: true });
  } else {
    syncExisting();
  }
}());
