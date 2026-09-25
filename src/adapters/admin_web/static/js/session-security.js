/* SESSION_SECURITY_HARDENING_V1
 *
 * Browser migration and defense-in-depth for the static Admin Web. The backend
 * returns the non-secret marker "cookie" while the real random session stays in
 * an HttpOnly cookie. Historical Web Storage secrets are removed on startup, and
 * regressions that try to persist credentials or real session tokens are blocked.
 */
(function () {
  'use strict';
  if (window.TRONCLASS_SESSION_SECURITY_HARDENING_V1) return;
  window.TRONCLASS_SESSION_SECURITY_HARDENING_V1 = true;

  var SESSION_KEY = 'tronclass_admin_session';
  var LEGACY_PASSWORD_KEY = 'tronclass_admin_saved_login';
  var SESSION_MARKER = 'cookie';
  var forcedPasswordChange = false;
  // Capture the original fetch before Dashboard patches install status caches.
  // Resume validation must always reach the restarted server.
  var nativeFetch = window.fetch.bind(window);
  var resumeValidation = null;
  var lastResumeValidationAt = 0;

  function storageItems() {
    var out = [];
    try { out.push(localStorage); } catch (_) {}
    try { out.push(sessionStorage); } catch (_) {}
    return out;
  }

  function clearLegacySecrets() {
    storageItems().forEach(function (store) {
      try { store.removeItem(LEGACY_PASSWORD_KEY); } catch (_) {}
      try {
        // A real token from an older release must not survive this migration.
        // The only allowed browser-side value is the inert compatibility marker.
        var current = store.getItem(SESSION_KEY);
        if (current && current !== SESSION_MARKER) store.removeItem(SESSION_KEY);
      } catch (_) {}
    });
  }

  clearLegacySecrets();

  // Source login handlers persist usernames only. Keep this interceptor as a
  // defense-in-depth regression guard and to prevent stale/third-party code from
  // restoring historical credential or real-session-token storage behavior.
  try {
    var originalSetItem = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key === LEGACY_PASSWORD_KEY) {
        try { this.removeItem(key); } catch (_) {}
        return;
      }
      if (key === SESSION_KEY && String(value || '') !== SESSION_MARKER) {
        try { this.removeItem(key); } catch (_) {}
        return;
      }
      return originalSetItem.call(this, key, value);
    };
  } catch (_) {}

  function setSessionMarker() {
    storageItems().forEach(function (store) {
      try { store.setItem(SESSION_KEY, SESSION_MARKER); } catch (_) {}
    });
  }

  function clearSessionMarker() {
    storageItems().forEach(function (store) {
      try { store.removeItem(SESSION_KEY); } catch (_) {}
      try { store.removeItem(LEGACY_PASSWORD_KEY); } catch (_) {}
    });
  }

  function showToast(message, ok) {
    var toast = document.getElementById('toast');
    if (toast) {
      toast.textContent = message;
      toast.className = 'toast visible ' + (ok === false ? 'bad' : 'ok');
      clearTimeout(toast._sessionSecurityTimer);
      toast._sessionSecurityTimer = setTimeout(function () {
        toast.classList.remove('visible');
      }, 3600);
      return;
    }
    if (message) console.log(message);
  }

  var logoutInFlight = false;
  function secureLogout() {
    // LOGOUT_SERVER_SIDE_V1：POST /admin/logout → 伺服器撤銷 session（access + refresh）、
    // 清掉 access / refresh / SSO cookie 並 302 /admin。這裡用 fetch（redirect:'manual'）而非
    // 表單導覽：Dashboard 各頁帶 Referrer-Policy: no-referrer，表單導覽的 Origin 會變成 null，
    // 舊版 Safari 沒有 Sec-Fetch-Site 時會被同源檢查擋下；fetch 一律帶正確 Origin。
    // 回應的 Set-Cookie 在 fetch 上同樣生效，之後只導向一次到 /admin（已無 cookie → 登入頁）。
    // 不再依賴 ?logout=1、tronclass_dashboard_manual_logout 或 tronclass_session_hint。
    if (logoutInFlight) return Promise.resolve();
    logoutInFlight = true;
    clearSessionMarker();
    try { localStorage.removeItem('tronclass_dashboard_manual_logout'); } catch (_) {}
    try { localStorage.removeItem('tronclass_session_hint'); } catch (_) {}
    return nativeFetch('/admin/logout', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'manual',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      body: ''
    }).catch(function () {
      // 網路失敗也要離開頁面；伺服器端 session 會在到期時自然失效。
    }).then(function () {
      location.replace('/admin');
    });
  }

  function isLogoutControl(target) {
    var control = target && target.closest && target.closest('button,a,[role="button"]');
    if (!control) return false;
    if (control.dataset && control.dataset.secureLogout === '1') return true;
    if (['logoutBtn', 'mLogout', 'moreLogoutFinalV2', 'dashboardLogout'].indexOf(control.id) >= 0) return true;
    return String(control.textContent || '').replace(/\s+/g, '') === '登出';
  }

  document.addEventListener('click', function (event) {
    if (!isLogoutControl(event.target)) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.stopImmediatePropagation) event.stopImmediatePropagation();
    secureLogout();
  }, true);

  function configurePasswordModal(modal, required) {
    var title = document.getElementById('dashboardPasswordModalTitle');
    var hint = document.getElementById('dashboardPasswordModalHint');
    var currentRow = document.getElementById('dashboardCurrentPasswordRow');
    var close = document.getElementById('dashboardPasswordModalClose');
    modal.dataset.required = required ? '1' : '0';
    if (title) title.textContent = required ? '首次登入：請設定新密碼' : '變更 Dashboard 密碼';
    if (hint) {
      hint.textContent = required
        ? '你已使用 Tronclass 密碼完成首次驗證。請立即設定一組新的 Dashboard 密碼才能繼續。'
        : '這是網站專用密碼，不能與 Tronclass 密碼相同。';
    }
    if (currentRow) currentRow.style.display = required ? 'none' : '';
    if (close) close.style.display = required ? 'none' : '';
  }

  function ensurePasswordModal(required) {
    var modal = document.getElementById('dashboardPasswordModalV1');
    if (modal) {
      configurePasswordModal(modal, !!required);
      return modal;
    }
    modal = document.createElement('div');
    modal.id = 'dashboardPasswordModalV1';
    modal.className = 'session-security-modal-backdrop';
    modal.innerHTML = '' +
      '<div class="session-security-modal" role="dialog" aria-modal="true" aria-labelledby="dashboardPasswordModalTitle">' +
        '<div class="session-security-modal-head">' +
          '<div><h2 id="dashboardPasswordModalTitle">變更 Dashboard 密碼</h2>' +
          '<p id="dashboardPasswordModalHint">這是網站專用密碼，不能與 Tronclass 密碼相同。</p></div>' +
          '<button id="dashboardPasswordModalClose" class="secondary" type="button">關閉</button>' +
        '</div>' +
        '<label id="dashboardCurrentPasswordRow">目前 Dashboard 密碼<input id="dashboardCurrentPassword" type="password" autocomplete="current-password"></label>' +
        '<label>新的 Dashboard 密碼<input id="dashboardNewPassword" type="password" autocomplete="new-password"></label>' +
        '<label>確認新的 Dashboard 密碼<input id="dashboardConfirmPassword" type="password" autocomplete="new-password"></label>' +
        '<div id="dashboardPasswordMessage" class="small"></div>' +
        '<div class="session-security-modal-actions"><button id="dashboardPasswordSave" type="button">儲存新密碼</button></div>' +
      '</div>';
    document.body.appendChild(modal);
    document.getElementById('dashboardPasswordModalClose').onclick = function () {
      if (modal.dataset.required !== '1') modal.classList.remove('visible');
    };
    modal.addEventListener('click', function (event) {
      if (event.target === modal && modal.dataset.required !== '1') modal.classList.remove('visible');
    });
    document.getElementById('dashboardPasswordSave').onclick = changePassword;
    configurePasswordModal(modal, !!required);
    return modal;
  }

  function passwordMessage(message, ok) {
    var node = document.getElementById('dashboardPasswordMessage');
    if (!node) return;
    node.textContent = message || '';
    node.className = 'small ' + (ok === false ? 'bad' : ok === true ? 'ok' : '');
  }

  async function changePassword() {
    var modal = document.getElementById('dashboardPasswordModalV1');
    var required = !!(modal && modal.dataset.required === '1');
    var current = document.getElementById('dashboardCurrentPassword');
    var next = document.getElementById('dashboardNewPassword');
    var confirm = document.getElementById('dashboardConfirmPassword');
    if (!current || !next || !confirm) return;
    if (next.value !== confirm.value) {
      passwordMessage('兩次輸入的新密碼不一致。', false);
      return;
    }
    if (!next.value) {
      passwordMessage('新密碼不可為空。', false);
      return;
    }

    passwordMessage('正在儲存...', true);
    try {
      var response = await fetch('/admin/api/password', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          current_password: required ? '' : current.value,
          new_password: next.value
        }),
        cache: 'no-store'
      });
      var body = await response.json().catch(function () { return {}; });
      if (!response.ok) throw new Error(body.error || ('HTTP ' + response.status));
      setSessionMarker();
      current.value = '';
      next.value = '';
      confirm.value = '';
      forcedPasswordChange = false;
      document.documentElement.classList.remove('dashboard-password-change-required');
      if (document.body) document.body.classList.remove('dashboard-password-change-required');
      passwordMessage('Dashboard 密碼已更新；其他裝置的登入已失效。', true);
      showToast('Dashboard 密碼已更新。');
      setTimeout(function () {
        if (required) {
          // Reload once with the replacement normal session, so all protected
          // Dashboard data is fetched only after the required change succeeds.
          location.replace('/admin#overview');
          return;
        }
        var modal = document.getElementById('dashboardPasswordModalV1');
        if (modal) modal.classList.remove('visible');
      }, 1000);
    } catch (error) {
      passwordMessage((error && error.message) || '密碼更新失敗。', false);
    }
  }

  function ensurePasswordButton() {
    if (document.getElementById('dashboardPasswordButtonV1')) return;
    var host = document.querySelector('.top-actions, header .toolbar-right');
    if (!host) return;
    // TOPBAR_LOGOUT_ONLY_V1: 所有頁面的頂欄 header 一律不注入變更密碼
    //（變更密碼改由手機更多面板與設定頁進入）。
    // SUBPAGE_ACTIONS_NO_INJECT_V1: '.actions' 也從候選移除——它會命中各子頁
    // 卡片內的按鈕列（如點名偵測排程的「儲存設定／重新整理」），把變更密碼
    // 按鈕插進卡片裡。host 候選僅剩 header 內的選擇器，一律被下方 guard 擋下。
    if (host.closest('header')) return;
    var button = document.createElement('button');
    button.id = 'dashboardPasswordButtonV1';
    button.type = 'button';
    button.className = 'secondary dashboard-password-button-v1';
    button.textContent = '變更密碼';
    button.onclick = function () {
      var modal = ensurePasswordModal(false);
      modal.classList.add('visible');
      var current = document.getElementById('dashboardCurrentPassword');
      if (current) current.focus();
    };
    host.insertBefore(button, host.firstChild);
  }

  function relabelLegacyRememberCheckbox() {
    document.querySelectorAll('.remember').forEach(function (label) {
      var checkbox = label.querySelector('input[type="checkbox"]');
      if (!checkbox) return;
      // The checkbox now remembers only an account identifier; passwords are
      // never persisted in Web Storage.
      label.lastChild && (label.lastChild.textContent = ' 記住帳號');
    });
  }

  var firstLoginCheckStarted = false;
  async function enforceFirstLoginPasswordChange() {
    // LOGIN_PERF_V1：伺服器驗證過才送出的頁面（<html data-auth="ok">）在 must_change_password
    // 時根本不會被送出（pages.rs 會 307 到 /admin/password），這裡不必再問一次。
    // 其餘頁面也只問一次——過去 boot() 的 500ms 計時器讓這支函式跑 21 次，等於每開一頁
    // 多打 21 趟 /admin/api/status。
    if (serverVerifiedPage()) return;
    // 獨立登入頁沒有 session，可略過（否則殘留的 marker 會白打一趟 401）。
    if (document.getElementById('animatedLoginForm')) return;
    if (firstLoginCheckStarted || forcedPasswordChange) return;
    var marker = '';
    try { marker = (localStorage.getItem(SESSION_KEY) || sessionStorage.getItem(SESSION_KEY) || '').trim(); } catch (_) {}
    if (marker !== SESSION_MARKER) return;
    firstLoginCheckStarted = true;
    try {
      var response = await fetch('/admin/api/status', {
        credentials: 'same-origin',
        cache: 'no-store'
      });
      var body = await response.json().catch(function () { return {}; });
      if (!response.ok || !body.must_change_password) return;
      forcedPasswordChange = true;
      document.documentElement.classList.add('dashboard-password-change-required');
      if (document.body) document.body.classList.add('dashboard-password-change-required');
      var modal = ensurePasswordModal(true);
      modal.classList.add('visible');
      passwordMessage('請先設定新的 Dashboard 密碼，完成後才能使用網站。', false);
      var next = document.getElementById('dashboardNewPassword');
      if (next) next.focus();
    } catch (_) {
      // Other dashboard scripts will surface normal network/login failures.
    }
  }

  var expiredRedirecting = false;
  function showExpiredLogin() {
    clearSessionMarker();
    if (typeof window.__tronAuthSetState === 'function') {
      window.__tronAuthSetState('out');
    } else {
      document.documentElement.classList.remove('tron-auth-ok', 'dash-auth-ok', 'tron-auth-pending');
      document.documentElement.classList.add('tron-auth-logged-out');
      if (document.body) document.body.classList.remove('logged-in', 'auth-ok', 'dash-auth-ok');
    }
    // 登入頁本身（獨立頁）不需要導向。
    if (document.getElementById('animatedLoginForm')) {
      var message = document.getElementById('loginMsg');
      if (message) message.textContent = '登入狀態已過期，請重新登入。';
      return;
    }
    // LOGIN_PAGE_STANDALONE_V1：Dashboard 與所有子頁都沒有內嵌登入卡，一律導回登入頁（帶 next）。
    if (expiredRedirecting) return;
    expiredRedirecting = true;
    var next = location.pathname + location.search + location.hash;
    var target = new URL('/admin', location.origin);
    if (next !== '/admin' && next !== '/admin/') target.searchParams.set('next', next);
    target.searchParams.set('v', 'sessionexpired' + Date.now());
    location.replace(target.pathname + target.search);
  }

  // LOGIN_PERF_V1：這一頁是不是伺服器驗證 session 後才送出的（pages::inject_auth_attrs）。
  function serverVerifiedPage() {
    try { return document.documentElement.getAttribute('data-auth') === 'ok'; } catch (_) { return false; }
  }
  var bootAt = Date.now();
  function validateSessionAfterResume() {
    if (document.hidden) return Promise.resolve();
    // LOGIN_PERF_V1：剛載入的前 5 秒不做「恢復驗證」——頁面本身就是伺服器剛驗證完 session
    // 才送出的；初次 pageshow／focus 觸發的這一趟 /admin/api/status 純屬重複。
    if (serverVerifiedPage() && (Date.now() - bootAt) < 5000) return Promise.resolve();
    var authenticated = window.__tronAuthState === 'ok' ||
      !!(document.body && document.body.classList.contains('logged-in'));
    if (!authenticated) return Promise.resolve();
    var now = Date.now();
    if (resumeValidation) return resumeValidation;
    if (now - lastResumeValidationAt < 1000) return Promise.resolve();
    lastResumeValidationAt = now;
    var previousState = window.__tronAuthState || 'ok';
    if (typeof window.__tronAuthSetState === 'function') window.__tronAuthSetState('pending');
    resumeValidation = nativeFetch('/admin/api/status?include_history=false&resume=' + now, {
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { 'Accept': 'application/json' }
    }).then(async function (response) {
      // REFRESH_TOKEN_V1: access cookie may be short-lived but a valid refresh
      // cookie can silently mint a new one. Try a single refresh before
      // declaring the session expired.
      if ((response.status === 401 || response.status === 403) && window.__tronTryRefresh) {
        try {
          var refreshed = await window.__tronTryRefresh();
          if (refreshed) {
            response = await nativeFetch('/admin/api/status?include_history=false&resume=' + Date.now(), {
              credentials: 'same-origin', cache: 'no-store',
              headers: { 'Accept': 'application/json' }
            });
          }
        } catch (_) {}
      }
      if (response.status === 401 || response.status === 403) {
        showExpiredLogin();
        return;
      }
      if (response.ok && typeof window.__tronAuthSetState === 'function') {
        window.__tronAuthSetState('ok');
      } else if (!response.ok && typeof window.__tronAuthSetState === 'function') {
        window.__tronAuthSetState(previousState);
      }
    }).catch(function () {
      // A temporary network outage is not proof that the session expired.
      if (typeof window.__tronAuthSetState === 'function') window.__tronAuthSetState(previousState);
    }).finally(function () {
      resumeValidation = null;
    });
    return resumeValidation;
  }

  function installResumeSessionValidation() {
    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) validateSessionAfterResume();
    }, { passive: true });
    window.addEventListener('pageshow', function (ev) {
      // 只有從 bfcache 復原（persisted）時才需要重新驗證；一般載入的 pageshow 略過。
      if (ev && ev.persisted === false) return;
      validateSessionAfterResume();
    }, { passive: true });
    window.addEventListener('focus', validateSessionAfterResume, { passive: true });
  }

  function boot() {
    clearLegacySecrets();
    installResumeSessionValidation();
    relabelLegacyRememberCheckbox();
    ensurePasswordButton();
    enforceFirstLoginPasswordChange();
    var count = 0;
    var timer = setInterval(function () {
      clearLegacySecrets();
      relabelLegacyRememberCheckbox();
      ensurePasswordButton();
      enforceFirstLoginPasswordChange();
      count += 1;
      if (count >= 20) clearInterval(timer);
    }, 500);
  }

  window.tronSecureLogout = secureLogout;
  window.tronDashboardSessionMarker = setSessionMarker;
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
})();
