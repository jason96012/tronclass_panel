// suspensions-admin.js — 帳號暫時停權管理頁（管理員）。
// ACCOUNT_SUSPEND_V1：全程以 DOM API（createElement／textContent）建立內容，不使用 innerHTML，
// 以符合後台的 Trusted Types 規則。不使用 inline style／inline script。
(function () {
  'use strict';

  var DURATION_LABEL = { '1h': '1 小時', '1d': '1 天', '3d': '3 天', '7d': '7 天', 'custom': '自訂到期時間' };
  var data = null;

  function q(id) { return document.getElementById(id); }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function nowSecs() {
    return data && data.now ? data.now : Math.floor(Date.now() / 1000);
  }

  function fmtUnix(unix) {
    var d = new Date(unix * 1000);
    return isNaN(d) ? '—' : d.toLocaleString('zh-TW', { hour12: false });
  }

  function remainLabel(until) {
    var s = until - nowSecs();
    if (s <= 0) return '已到期';
    var d = Math.floor(s / 86400);
    var h = Math.floor((s % 86400) / 3600);
    var m = Math.floor((s % 3600) / 60);
    return (d ? d + ' 天 ' : '') + (h ? h + ' 小時 ' : '') + (m || !(d || h) ? m + ' 分鐘' : '');
  }

  function setMsg(text, ok) {
    var m = q('susMsg');
    m.textContent = text || '';
    m.className = 'msg ' + (ok ? 'ok' : 'bad');
  }

  function errorText(error) {
    var map = {
      'admin only': '只有管理員可以設定停權。',
      'unauthorized': '登入已失效，請重新登入。',
      'account not found': '找不到這個帳號。',
      'account_id is required': '請先選擇帳號。',
      '不能停權自己': '不能停權自己。',
      '請指定自訂到期時間': '請選擇自訂到期時間。',
      '不支援的停權時長': '不支援的停權時長。',
      '到期時間必須晚於現在至少 1 分鐘': '到期時間必須晚於現在至少 1 分鐘。',
      '最長只能停權 365 天': '最長只能停權 365 天。',
      '停權原因過長（上限 200 字）': '停權原因過長（上限 200 字）。'
    };
    var msg = error && error.message;
    return map[msg] || (msg ? msg : '操作失敗，請稍後再試。');
  }

  function request(path, body) {
    var opts = { method: body ? 'POST' : 'GET', cache: 'no-store', credentials: 'same-origin' };
    if (body) {
      opts.headers = { 'content-type': 'application/json' };
      opts.body = JSON.stringify(body);
    }
    return fetch(path, opts).then(function (res) {
      return res.json().catch(function () { return {}; }).then(function (b) {
        if (!res.ok) {
          var e = new Error((b && b.error) || ('HTTP ' + res.status));
          e.status = res.status;
          throw e;
        }
        return b;
      });
    });
  }

  function accountById(id) {
    var list = (data && data.accounts) || [];
    for (var i = 0; i < list.length; i++) if (list[i].id === id) return list[i];
    return null;
  }

  function suspensionOf(id) {
    var list = (data && data.suspensions) || [];
    for (var i = 0; i < list.length; i++) if (list[i].account_id === id) return list[i];
    return null;
  }

  function accountLabel(acc) {
    return acc ? (acc.username + '（' + acc.provider + '）') : '';
  }

  // ── 表單：帳號下拉選單 ───────────────────────────────────────────────
  function renderAccountSelect() {
    var sel = q('susAccount');
    var keep = sel.value;
    sel.textContent = '';
    var first = el('option', '', '請選擇帳號');
    first.value = '';
    sel.appendChild(first);
    var accounts = (data && data.accounts) || [];
    accounts.forEach(function (acc) {
      var parts = [acc.username + '（' + acc.provider + '）'];
      if (acc.is_admin) parts.push('管理員');
      if (acc.is_viewer) parts.push('檢視者');
      if (!acc.enabled) parts.push('已停用');
      if (suspensionOf(acc.id)) parts.push('停權中');
      var opt = el('option', '', parts.join(' · '));
      opt.value = acc.id;
      if (acc.id === data.self_account_id) {
        opt.disabled = true;
        opt.textContent += '（自己，不可停權）';
      }
      sel.appendChild(opt);
    });
    if (keep) sel.value = keep;
  }

  // ── 目前停權中 ───────────────────────────────────────────────────────
  function renderActive() {
    var body = q('activeBody');
    body.textContent = '';
    var rows = (data && data.suspensions) || [];
    if (!rows.length) {
      var tr = el('tr');
      var td = el('td', 'empty', '目前沒有停權中的帳號。');
      td.colSpan = 6;
      tr.appendChild(td);
      body.appendChild(tr);
      return;
    }
    rows.forEach(function (row) {
      var tr = el('tr');
      var acc = accountById(row.account_id);
      tr.appendChild(el('td', '', acc ? accountLabel(acc) : row.account_id));
      tr.appendChild(el('td', '', fmtUnix(row.until)));
      tr.appendChild(el('td', '', remainLabel(row.until)));
      tr.appendChild(el('td', '', row.reason || '—'));
      tr.appendChild(el('td', 'muted', row.created_by || '—'));
      var td = el('td');
      var btn = el('button', 'small', '提前解除');
      btn.type = 'button';
      btn.addEventListener('click', function () { clearSuspension(row.account_id, acc ? acc.username : row.account_id); });
      td.appendChild(btn);
      tr.appendChild(td);
      body.appendChild(tr);
    });
  }

  // ── 所有帳號 ─────────────────────────────────────────────────────────
  function renderAccounts() {
    var body = q('accountsBody');
    body.textContent = '';
    var accounts = (data && data.accounts) || [];
    if (!accounts.length) {
      var tr0 = el('tr');
      var td0 = el('td', 'empty', '沒有任何帳號。');
      td0.colSpan = 4;
      tr0.appendChild(td0);
      body.appendChild(tr0);
      return;
    }
    accounts.forEach(function (acc) {
      var tr = el('tr');
      tr.appendChild(el('td', '', acc.username));
      tr.appendChild(el('td', '', acc.provider));
      var status = el('td');
      if (acc.is_admin) status.appendChild(el('span', 'badge', '管理員'));
      if (acc.is_viewer) status.appendChild(el('span', 'badge', '檢視者'));
      if (!acc.enabled) status.appendChild(el('span', 'badge off', '已停用'));
      var sus = suspensionOf(acc.id);
      if (sus) status.appendChild(el('span', 'badge stop', '停權至 ' + fmtUnix(sus.until)));
      if (!status.childNodes.length) status.appendChild(el('span', 'muted', '一般'));
      tr.appendChild(status);
      var td = el('td');
      if (acc.id !== data.self_account_id) {
        if (sus) {
          var clr = el('button', 'small', '提前解除');
          clr.type = 'button';
          clr.addEventListener('click', function () { clearSuspension(acc.id, acc.username); });
          td.appendChild(clr);
        } else {
          var pick = el('button', 'small', '設為停權');
          pick.type = 'button';
          pick.addEventListener('click', function () {
            q('susAccount').value = acc.id;
            setMsg('', true);
            q('susAccount').focus();
            window.scrollTo({ top: 0, behavior: 'smooth' });
          });
          td.appendChild(pick);
        }
      } else {
        td.appendChild(el('span', 'muted', '（自己）'));
      }
      tr.appendChild(td);
      body.appendChild(tr);
    });
  }

  function render() {
    renderAccountSelect();
    renderActive();
    renderAccounts();
  }

  // ── 載入 ─────────────────────────────────────────────────────────────
  function load() {
    return request('/admin/api/suspensions').then(function (body) {
      data = body;
      render();
      var isAdmin = !!body.is_admin;
      q('susSubmit').disabled = !isAdmin;
      q('susAccount').disabled = !isAdmin;
      q('susDuration').disabled = !isAdmin;
      if (!isAdmin) setMsg('你是檢視者，只能查看停權狀態，無法設定。', false);
    }).catch(function (error) {
      setMsg('載入失敗：' + errorText(error), false);
    });
  }

  // ── 停權／解除 ───────────────────────────────────────────────────────
  function applySuspension() {
    var accountId = q('susAccount').value;
    if (!accountId) { setMsg('請先選擇帳號。', false); return; }
    var acc = accountById(accountId);
    var duration = q('susDuration').value;
    var body = {
      account_id: accountId,
      duration: duration,
      reason: q('susReason').value.trim()
    };
    if (duration === 'custom') {
      var raw = q('susUntil').value;
      var ms = raw ? new Date(raw).getTime() : NaN;
      if (!raw || isNaN(ms)) { setMsg('請選擇自訂到期時間。', false); return; }
      body.until = Math.floor(ms / 1000);
    }
    var when = duration === 'custom' ? fmtUnix(body.until) : DURATION_LABEL[duration];
    var name = acc ? accountLabel(acc) : accountId;
    if (!window.confirm('確定要停權「' + name + '」到 ' + when + '？\n該帳號所有登入中的裝置會立即被登出。')) return;

    var btn = q('susSubmit');
    btn.disabled = true;
    request('/admin/api/suspensions/set', body).then(function (res) {
      setMsg('已停權「' + name + '」，到期時間：' + fmtUnix(res.until) + '。', true);
      q('susReason').value = '';
      return load();
    }).catch(function (error) {
      setMsg('停權失敗：' + errorText(error), false);
    }).then(function () {
      btn.disabled = false;
    });
  }

  function clearSuspension(accountId, username) {
    if (!window.confirm('確定要解除「' + username + '」的停權？')) return;
    request('/admin/api/suspensions/clear', { account_id: accountId }).then(function () {
      setMsg('已解除「' + username + '」的停權。', true);
      return load();
    }).catch(function (error) {
      setMsg('解除失敗：' + errorText(error), false);
    });
  }

  // ── 初始化 ───────────────────────────────────────────────────────────
  function initDatetimeInput() {
    var input = q('susUntil');
    var d = new Date(Date.now() + 2 * 60 * 1000);
    var pad = function (n) { return (n < 10 ? '0' : '') + n; };
    var local = d.getFullYear() + '-' + pad(d.getMonth() + 1) + '-' + pad(d.getDate()) +
      'T' + pad(d.getHours()) + ':' + pad(d.getMinutes());
    input.min = local;
  }

  function init() {
    initDatetimeInput();
    q('susDuration').addEventListener('change', function () {
      q('susUntilWrap').hidden = q('susDuration').value !== 'custom';
    });
    q('susSubmit').addEventListener('click', applySuspension);
    q('refresh').addEventListener('click', function () { load(); });
    load();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
