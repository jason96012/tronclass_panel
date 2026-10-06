/* ROLLCALL_PENDING_FAB_V1：全站共用的「還有點名尚未送出」捷徑按鈕（由 inject_theme
 * 注入每個 admin 頁，所以不管使用者停在哪一頁都看得到——這是使用者明確要求的：
 * 「不管在哪個頁面只要有點名就要顯示」）。
 *
 * 跟既有的 rollcallQuickAlert（dashboard.html 專用的全螢幕彈窗，只在「剛偵測到新點名」
 * 那一刻跳一次、關掉後這次就不會再跳）不一樣：這顆右下角小圓按鈕是「只要還沒送出就一直在」
 * 的持續提醒，離開 Dashboard 到別頁（公告／設定／教材…）也看得到；一旦送出完成就自動消失，
 * 不需要使用者自己按關閉。
 *
 * 資料來源：跟 Dashboard「點名」分頁、dashboard-live-alerts.js 用同一支
 * `/admin/api/status`（已有 2 秒短快取），分組／完成判斷邏輯交給共用模組
 * rollcall-detect.js（window.TronclassRollcallDetect，見該檔頭註解——原本這裡跟
 * dashboard.html、health.html 各自抄了一份，三邊已經悄悄分岔，所以收成一份正本）。
 * 更新時機：
 *   ① 進頁時跑一次；
 *   ② live-events.js 的 SSE 廣播 `tronclass:live-status`（帳號狀態一變就打一次，近乎即時）；
 *   ③ 每 30 秒的備援輪詢（SSE 斷線、或瀏覽器丟棄背景分頁連線時的保險）；
 *   ④ 分頁從背景切回前景、或登入狀態變化時。
 * 不會另外開一條 SSE 連線——live-events.js 已經是全站共用的那一條，這裡只是搭它的便車。
 */
(function () {
  'use strict';
  if (window.ROLLCALL_PENDING_FAB_V1) return;
  window.ROLLCALL_PENDING_FAB_V1 = true;
  if (location.pathname.indexOf('/admin') !== 0) return;
  // 跟 live-events.js 同樣的理由：改密碼頁的 bootstrap session 打 /admin/api/status
  // 一律會被擋下，不必浪費請求。
  if (location.pathname.indexOf('/admin/password') === 0) return;

  var POLL_MS = 30000;
  // ROLLCALL_FAB_STUCK_FETCH_FIX_V1：使用者回報「有點名時按鈕要刷新頁面才會出現」——
  // 追查發現 check() 的 inFlight 旗標只在 fetch 的 Promise 鏈結束時才會重設；手機瀏覽器
  // 分頁被系統凍結、或網路中斷又沒有明確觸發 fetch 失敗時，那個 Promise 可能永遠卡著
  // pending（既不 resolve 也不 reject），inFlight 就永遠卡在 true——之後不管是 SSE 廣播的
  // `tronclass:live-status`、30 秒備援輪詢、或回到前景，全部會被 check() 開頭那行
  // `if (inFlight || ...) return;` 擋下，按鈕再也不會更新，直到整頁重新整理（腳本重新
  // 執行、inFlight 變數歸零）才會恢復——這正是使用者看到的現象。修法：幫這次請求加一個逾時，
  // 逾時就主動中止，讓 Promise 鏈一定會走到結尾、保證 inFlight 一定會被重設。
  var CHECK_TIMEOUT_MS = 10000;

  var RECENT_SECS = 1800; // 跟 dashboard.html 的 detected() 用同一個 30 分鐘窗口，數字才會一致。

  var fab = null;
  var iconHost = null;
  var countEl = null;
  var inFlight = false;

  // URL_SAFETY_SHARED_V1：委派給共用模組（inject_theme 保證排在這支腳本之前載入）；
  // 原本這裡跟 dashboard.html／health.html／dashboard-live-alerts.js 各自抄一份
  // 一樣的邏輯，收成正本見 url-safety.js。
  function safeUrl(v) {
    return window.TronclassUrlSafety ? window.TronclassUrlSafety.safeHttpUrl(v) : '';
  }
  // ROLLCALL_DETECT_SHARED_V1：分組／完成判斷交給 rollcall-detect.js（inject_theme 保證
  // 排在這支腳本之前載入）；這裡只留「這顆按鈕要用哪些組」——必須還有人沒送出、
  // 而且要有一個可以點的連結，兩個條件都是 fab 按鈕特有的，不屬於共用判斷邏輯。
  function pendingGroups(status) {
    var accounts = (status && status.accounts) || [];
    if (!window.TronclassRollcallDetect) return []; // 共用模組還沒載入完就先不顯示，等下一次 check() 再試。
    return window.TronclassRollcallDetect.detect(accounts, { windowSecs: RECENT_SECS }).filter(function (g) {
      return g.pending > 0 && safeUrl(g.url);
    });
  }

  /* ROLLCALL_FAB_NAV_CLEARANCE_V1：手機版底部有一條共用導覽列（#mobileNavFinalV2），
     桌機版沒有。與其硬寫死一個跟 mobile-navigation.css 綁死的像素數字（日後那邊的高度一改，
     這裡就會悄悄重疊），改成「量實際畫面上那條導覽列真的佔了多少空間」——不管哪個頁面、
     導覽列存不存在、以後改多高，這顆按鈕永遠貼著它上面一點點，不會被蓋住也不會留一大片空白。 */
  function navClearancePx() {
    var nav = document.getElementById('mobileNavFinalV2') || document.querySelector('.mobile-nav');
    if (!nav) return 0;
    var style = window.getComputedStyle ? window.getComputedStyle(nav) : null;
    if (style && (style.display === 'none' || style.visibility === 'hidden')) return 0;
    var rect = nav.getBoundingClientRect();
    if (!rect || rect.height <= 0) return 0;
    var fromBottom = (window.innerHeight || document.documentElement.clientHeight || 0) - rect.top;
    return fromBottom > 0 ? fromBottom : 0;
  }
  function positionFab() {
    if (!fab) return;
    var clearance = navClearancePx();
    var bottom = clearance > 0 ? clearance + 14 : 24;
    fab.style.bottom = bottom + 'px';
  }

  function ensureFab() {
    if (fab) return fab;
    fab = document.createElement('a');
    fab.id = 'rollcallPendingFab';
    fab.target = '_blank';
    fab.rel = 'noopener';
    fab.setAttribute('role', 'link');
    // 跟 mobile-nav.js「點名」按鈕用同一個圖示（打勾的剪貼簿），全站看起來是同一件事。
    fab.innerHTML =
      '<span class="rpf-ring" aria-hidden="true"></span>' +
      '<svg class="rpf-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M9 11l3 3L22 4"></path><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"></path></svg>' +
      '<span class="rpf-count" id="rollcallPendingFabCount"></span>';
    document.body.appendChild(fab);
    countEl = fab.querySelector('#rollcallPendingFabCount');
    window.addEventListener('resize', positionFab, { passive: true });
    window.addEventListener('orientationchange', positionFab, { passive: true });
    return fab;
  }

  function render(items) {
    if (!items || !items.length) {
      if (fab) fab.classList.remove('visible');
      return;
    }
    ensureFab();
    var top = items[0];
    var totalPending = items.reduce(function (sum, g) {
      return sum + (g.total - g.done);
    }, 0);
    fab.href = safeUrl(top.url) || '#';
    fab.setAttribute(
      'aria-label',
      '還有點名尚未送出：' + (top.course || '點名') + '，還有 ' + totalPending + ' 人未送出，按一下前往點名頁'
    );
    fab.title = fab.getAttribute('aria-label');
    if (countEl) countEl.textContent = totalPending > 9 ? '9+' : String(totalPending);
    fab.classList.toggle('has-count', totalPending > 0);
    fab.classList.add('visible');
    positionFab();
  }

  function check() {
    if (inFlight || document.hidden) return;
    inFlight = true;
    // ROLLCALL_FAB_STUCK_FETCH_FIX_V1：用 AbortController 幫請求上限時，逾時就自己中止，
    // 確保不管網路狀況多糟，這次請求最晚 CHECK_TIMEOUT_MS 之後一定會 resolve/reject，
    // 不會讓 inFlight 卡死（沒有 AbortController 的舊瀏覽器就不中止，退回原本行為）。
    var ac = (typeof AbortController !== 'undefined') ? new AbortController() : null;
    var timedOut = false;
    var timeoutId = ac
      ? setTimeout(function () {
          timedOut = true;
          try { ac.abort(); } catch (_) { /* 已經結束就略過 */ }
        }, CHECK_TIMEOUT_MS)
      : null;
    fetch('/admin/api/status?include_history=false&history_limit=0', {
      credentials: 'same-origin',
      cache: 'no-store',
      signal: ac ? ac.signal : undefined
    })
      .then(function (res) {
        if (res.status === 401 || res.status === 403) {
          render([]); // 未登入／沒權限：不顯示，也不要一直重打。
          return null;
        }
        if (!res.ok) throw new Error('status ' + res.status);
        return res.json();
      })
      .then(function (body) {
        if (body) render(pendingGroups(body));
      })
      .catch(function () {
        /* 單次輪詢失敗（含逾時中止）不用特別提示——下一次 SSE 廣播或 30 秒備援會再試。 */
        void timedOut;
      })
      .then(function () {
        if (timeoutId) clearTimeout(timeoutId);
        inFlight = false;
      });
  }

  window.addEventListener('tronclass:live-status', function () {
    check();
  });
  window.addEventListener('tronclass:auth-state', function (ev) {
    if (ev && ev.detail === 'ok') check();
    else if (ev && ev.detail === 'out') render([]);
  });
  document.addEventListener('visibilitychange', function () {
    if (!document.hidden) check();
  });

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function () {
      setTimeout(check, 500);
    });
  } else {
    setTimeout(check, 500);
  }
  window.setInterval(function () {
    if (!document.hidden) check();
  }, POLL_MS);
})();
