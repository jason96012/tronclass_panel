/* SESSION_SECURITY_HARDENING_V1
 *
 * Transitional browser hardening for the legacy static Admin Web. The backend
 * returns the non-secret marker "cookie" while the real random session stays in
 * an HttpOnly cookie. This script prevents legacy patches from saving either a
 * password or an actual session token in Web Storage.
 */
(function () {
  'use strict';
  if (window.TRONCLASS_SESSION_SECURITY_HARDENING_V1) return;
  window.TRONCLASS_SESSION_SECURITY_HARDENING_V1 = true;

  var SESSION_KEY = 'tronclass_admin_session';
  var LEGACY_PASSWORD_KEY = 'tronclass_admin_saved_login';
  var SESSION_MARKER = 'cookie';
  var forcedPasswordChange = false;

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

  // Legacy inline scripts still try to save `tronclass_admin_saved_login` and,
  // after a login, `tronclass_admin_session`. Block those writes at their common
  // storage API rather than relying on every historical patch being removed.
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

  async function secureLogout() {
    try {
      await fetch('/admin/api/logout', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'content-type': 'application/json' },
        body: '{}',
        cache: 'no-store'
      });
    } catch (_) {
      // Always clear browser-side state even if the network vanished.
    }
    clearSessionMarker();
    try { localStorage.setItem('tronclass_dashboard_manual_logout', '1'); } catch (_) {}
    location.replace('/admin?logout=1&v=securelogout' + Date.now());
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
    var host = document.querySelector('.top-actions, .actions, header .toolbar-right');
    if (!host) return;
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

  async function enforceFirstLoginPasswordChange() {
    var marker = '';
    try { marker = (localStorage.getItem(SESSION_KEY) || sessionStorage.getItem(SESSION_KEY) || '').trim(); } catch (_) {}
    if (marker !== SESSION_MARKER || forcedPasswordChange) return;
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

  function boot() {
    clearLegacySecrets();
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
