/* NOTIFICATIONS_PAGE_V1 — 獨立通知設定頁（/admin/notifications）
 *
 * 由 dashboard 內嵌「通知」分頁改為獨立網頁（NOTIFICATIONS_STANDALONE_PAGE_V1）。
 * 本檔自包含，不依賴 dashboard 的 tab/view 系統：
 * 1) Discord 通知開關：GET/POST /admin/api/notifications/preferences
 * 2) 上課前 Discord 提醒：GET/POST /admin/api/class_reminder
 * 3) 作業截止提醒階梯：GET/POST /admin/api/homework/ladder（HOMEWORK_LADDER_V1）
 * 3b) 晨間摘要每帳號開關：GET/POST /admin/api/digest/preference（DIGEST_PER_ACCOUNT_V1）
 * 4) 即時點名提示音：localStorage（tronclass_rollcall_sound_settings_v1，
 *    與控制台 dashboard 內的 TronclassRollcallAlertSettings 共用同一個 key）
 * 未登入（401/403）時導回登入頁。
 */
(function () {
  'use strict';
  if (window.TronclassNotificationsPage) return;

  function q(id) { return document.getElementById(id); }
  function esc(v) {
    return String(v == null ? '' : v).replace(/[&<>"']/g, function (ch) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch];
    });
  }
  async function parseResponse(res) {
    var body = await res.json().catch(function () { return {}; });
    if (!res.ok) {
      var e = new Error(body.error || body.message || ('HTTP ' + res.status));
      e.status = res.status;
      throw e;
    }
    return body;
  }
  async function api(path, opts) {
    opts = opts || {};
    opts.credentials = opts.credentials || 'same-origin';
    opts.cache = opts.cache || 'no-store';
    return fetch(path, opts).then(parseResponse);
  }
  function setMsg(id, text, kind) {
    var el = q(id);
    if (!el) return;
    el.textContent = text || '';
    el.className = 'msg' + (kind ? ' ' + kind : '');
  }
  function verifySession() {
    /* LOGIN_PERF_V1：伺服器送出這頁時已驗證過 session（<html data-auth="ok">），不必再打一次。 */
    if (document.documentElement.getAttribute('data-auth') === 'ok') return;
    fetch('/admin/api/status', {
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { 'Accept': 'application/json' }
    }).then(function (res) {
      if (res.status === 401 || res.status === 403) {
        location.replace('/admin?next=' + encodeURIComponent('/admin/notifications'));
      }
    }).catch(function () { /* 驗證失敗時保留原頁 */ });
  }

  var lastStatus = null;
  var forceStatusRefresh = false; /* BINDING_SELF_SERVICE_V1：儲存綁定後略過 runtime 快取 */
  async function loadStatus() {
    if (lastStatus) return lastStatus;
    var force = forceStatusRefresh; forceStatusRefresh = false;
    lastStatus = await (window.TronclassRuntime && window.TronclassRuntime.statusSummary
      ? window.TronclassRuntime.statusSummary(force)
      : api('/admin/api/status?include_history=false&history_limit=0'));
    return lastStatus;
  }

  async function fillAccountSelect(sel, onReady) {
    try {
      var st = await loadStatus();
      var old = sel.value;
      sel.innerHTML = '';
      sel.disabled = false;
      var accounts = Array.isArray(st.accounts) ? st.accounts : [];
      if (st.is_admin) {
        accounts.forEach(function (a) {
          var id = a.account_id || a.id || a.username || '';
          if (!id) return;
          var name = a.user_name || a.name || a.display_name || a.username || '';
          sel.appendChild(new Option((id + (name ? '  ' + name : '')).trim(), id));
        });
        var preferred = (st.current_account_id && st.current_account_id !== '<token-admin>') ? st.current_account_id : '';
        if (old) sel.value = old;
        if (preferred && Array.prototype.some.call(sel.options, function (o) { return o.value === preferred; })) sel.value = preferred;
        if (!sel.value && sel.options.length) sel.selectedIndex = 0;
      } else {
        var me = st.current_account_id || st.account_id || '';
        sel.appendChild(new Option(me || '目前帳號', me));
        sel.disabled = true;
      }
      if (onReady) onReady();
    } catch (e) {
      sel.innerHTML = '';
      sel.appendChild(new Option('載入失敗', ''));
      sel.disabled = true;
    }
  }

  // ── 0. 通知管道綁定（BINDING_SELF_SERVICE_V1）──────────────────────────────
  function bindingAccountRow(accountId) {
    var st = lastStatus;
    var accounts = st && Array.isArray(st.accounts) ? st.accounts : [];
    return accounts.find(function (a) { return (a.account_id || a.id) === accountId; }) || null;
  }
  async function loadBinding() {
    var sel = q('bindingAccount');
    var account = sel.value;
    // /admin/api/status 已含 line_user_id / discord_user_id，不需要額外 API；
    // 重新整理時清掉快取讓 status 重抓。
    var row = bindingAccountRow(account);
    q('bindingDiscord').value = row ? (row.discord_user_id || '') : '';
    q('bindingLine').value = row ? (row.line_user_id || '') : '';
    var d = row && row.discord_user_id ? '已綁定 Discord' : '未綁定 Discord';
    var l = row && row.line_user_id ? '已綁定 LINE' : '未綁定 LINE';
    q('bindingInfo').textContent = row ? (d + '｜' + l) : '找不到帳號資料';
    setMsg('bindingMsg', '');
  }
  async function saveBinding() {
    var btn = q('bindingSave');
    var account = q('bindingAccount').value;
    var discord = q('bindingDiscord').value.trim();
    var line = q('bindingLine').value.trim();
    if (discord && !/^\d{5,32}$/.test(discord)) { setMsg('bindingMsg', 'Discord User ID 必須是純數字。', 'bad'); return; }
    if (line && !/^U[0-9a-fA-F]{32}$/.test(line)) { setMsg('bindingMsg', 'LINE User ID 應為 U 開頭共 33 碼。', 'bad'); return; }
    btn.disabled = true;
    setMsg('bindingMsg', '儲存中…');
    try {
      var res = await api('/admin/api/action', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ action: 'set_binding', account_id: account || undefined, discord_user_id: discord, line_user_id: line })
      });
      setMsg('bindingMsg', (res && res.message) || '綁定已更新', 'good');
      lastStatus = null; forceStatusRefresh = true;
      await fillAccountSelect(q('bindingAccount'), loadBinding);
    } catch (e) {
      setMsg('bindingMsg', e.message || '儲存失敗', 'bad');
    } finally {
      btn.disabled = false;
    }
  }

  // ── 1. Discord 通知開關 ──────────────────────────────────────────────────
  function renderNotifyPrefs(prefs) {
    var list = q('notifyList');
    list.innerHTML = prefs && prefs.length
      ? prefs.map(function (p) {
          return '<label class="pref"><input type="checkbox" data-notify-key="' + esc(p.key) + '"' + (p.enabled ? ' checked' : '') + '><span><b>' + esc(p.label) + '</b><i>' + esc(p.description || '') + '</i></span></label>';
        }).join('')
      : '<div class="small">沒有通知項目。</div>';
  }
  async function loadNotifyPrefs() {
    var sel = q('notifyAccount');
    var account = sel && sel.value ? sel.value : '';
    try {
      var body = await api('/admin/api/notifications/preferences' + (account ? '?account_id=' + encodeURIComponent(account) : ''));
      q('notifyInfo').textContent = '目前設定帳號：' + (body.account_id || account || '-');
      renderNotifyPrefs(body.preferences || []);
      setMsg('notifyMsg', '');
    } catch (e) {
      q('notifyList').innerHTML = '<div class="small c-bad">讀取失敗：' + esc(e.message || e) + '</div>';
    }
  }
  async function saveNotifyPrefs() {
    var sel = q('notifyAccount');
    var account = sel && sel.value ? sel.value : '';
    var prefs = Array.prototype.slice.call(document.querySelectorAll('[data-notify-key]')).map(function (cb) {
      return { key: cb.dataset.notifyKey, enabled: cb.checked };
    });
    try {
      await api('/admin/api/notifications/preferences', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ account_id: account || undefined, preferences: prefs })
      });
      setMsg('notifyMsg', 'Discord 通知設定已儲存', 'ok');
    } catch (e) {
      setMsg('notifyMsg', e.message || '儲存通知設定失敗', 'bad');
    }
  }
  function setAllNotify(v) {
    document.querySelectorAll('[data-notify-key]').forEach(function (cb) { cb.checked = v; });
  }

  // ── 1b. 晨間摘要每帳號開關（DIGEST_PER_ACCOUNT_V1）─────────────────────
  async function loadDigestPref() {
    var sel = q('digestPrefAccount');
    var account = sel && sel.value ? sel.value : '';
    try {
      var body = await api('/admin/api/digest/preference' + (account ? '?account_id=' + encodeURIComponent(account) : ''));
      q('digestPrefEnabled').checked = body.enabled !== false;
      var info = '目前設定帳號：' + (body.account_id || account || '-');
      if (body.global_enabled === false) info += '（管理員已全域關閉晨間摘要）';
      else if (body.time) info += '，每天 ' + body.time + ' 發送';
      q('digestPrefInfo').textContent = info;
      setMsg('digestPrefMsg', '');
    } catch (e) {
      q('digestPrefInfo').textContent = '讀取失敗：' + (e.message || e);
    }
  }
  async function saveDigestPref() {
    var sel = q('digestPrefAccount');
    var account = sel && sel.value ? sel.value : '';
    try {
      await api('/admin/api/digest/preference', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ account_id: account || undefined, enabled: !!q('digestPrefEnabled').checked })
      });
      setMsg('digestPrefMsg', '晨間摘要設定已儲存', 'ok');
    } catch (e) {
      setMsg('digestPrefMsg', e.message || '儲存晨間摘要設定失敗', 'bad');
    }
  }

  // ── 2. 上課前 Discord 提醒 ───────────────────────────────────────────────
  async function loadClassReminderPrefs() {
    var sel = q('classReminderAccount');
    var account = sel && sel.value ? sel.value : '';
    try {
      var body = await api('/admin/api/class_reminder' + (account ? '?account_id=' + encodeURIComponent(account) : ''));
      q('classReminderEnabled').checked = !!body.discord_enabled;
      q('classReminderMinutes').value = Number(body.minutes_before == null ? 10 : body.minutes_before);
      q('classReminderInfo').textContent = '目前設定帳號：' + (body.account_id || account || '-');
      setMsg('classReminderMsg', '');
    } catch (e) {
      q('classReminderInfo').textContent = '讀取失敗：' + (e.message || e);
    }
  }
  async function saveClassReminder() {
    var sel = q('classReminderAccount');
    var account = sel && sel.value ? sel.value : '';
    var minutes = parseInt(q('classReminderMinutes').value || '10', 10);
    if (!Number.isFinite(minutes)) minutes = 10;
    minutes = Math.max(0, Math.min(180, minutes));
    try {
      await api('/admin/api/class_reminder', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ account_id: account || undefined, discord_enabled: !!q('classReminderEnabled').checked, minutes_before: minutes })
      });
      q('classReminderMinutes').value = minutes;
      setMsg('classReminderMsg', '上課前 Discord 提醒設定已儲存', 'ok');
    } catch (e) {
      setMsg('classReminderMsg', e.message || '儲存上課提醒設定失敗', 'bad');
    }
  }

  // ── 3. 作業截止提醒階梯（HOMEWORK_LADDER_V1）─────────────────────────────
  function renderHwLadderRow(value) {
    var box = q('hwLadderRows');
    if (!box) return;
    var row = document.createElement('div');
    row.className = 'row2';
    row.innerHTML = '<input type="number" min="1" max="168" step="1" data-hw-stage value="' + esc(value) + '">' +
      '<button type="button" class="sec" data-hw-remove>移除</button>';
    row.querySelector('[data-hw-remove]').addEventListener('click', function () { row.remove(); });
    box.appendChild(row);
  }
  function renderHwLadder(stages) {
    var box = q('hwLadderRows');
    if (!box) return;
    box.innerHTML = '';
    (stages || []).forEach(function (h) { renderHwLadderRow(h); });
  }
  async function loadHwLadder() {
    var sel = q('hwLadderAccount');
    var account = sel && sel.value ? sel.value : '';
    try {
      var body = await api('/admin/api/homework/ladder' + (account ? '?account_id=' + encodeURIComponent(account) : ''));
      renderHwLadder(body.stages || []);
      q('hwLadderInfo').textContent = '目前設定帳號：' + (body.account_id || account || '-') +
        (body.use_default ? '（使用預設）' : '（自訂）');
      setMsg('hwLadderMsg', '');
    } catch (e) {
      q('hwLadderInfo').textContent = '讀取失敗：' + (e.message || e);
    }
  }
  function collectHwLadder() {
    var values = [];
    var bad = null;
    Array.prototype.slice.call(document.querySelectorAll('[data-hw-stage]')).forEach(function (input) {
      var raw = (input.value || '').trim();
      if (!raw) return;
      var n = parseInt(raw, 10);
      if (!Number.isFinite(n) || n < 1 || n > 168) { bad = raw; return; }
      if (values.indexOf(n) === -1) values.push(n);
    });
    return bad === null ? { stages: values } : { error: bad };
  }
  async function saveHwLadder() {
    var sel = q('hwLadderAccount');
    var account = sel && sel.value ? sel.value : '';
    var collected = collectHwLadder();
    if (collected.error != null) {
      setMsg('hwLadderMsg', '階段值無效：' + collected.error + '（需 1～168 的整數）', 'bad');
      return;
    }
    if (collected.stages.length > 8) {
      setMsg('hwLadderMsg', '最多 8 個階段', 'bad');
      return;
    }
    try {
      var body = await api('/admin/api/homework/ladder', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ account_id: account || undefined, stages: collected.stages })
      });
      renderHwLadder(body.stages || []);
      setMsg('hwLadderMsg', body.use_default ? '已恢復預設階梯（24/6/1）' : '作業提醒階梯已儲存', 'ok');
    } catch (e) {
      setMsg('hwLadderMsg', e.message || '儲存作業提醒階梯失敗', 'bad');
    }
  }

  // ── 4. 即時點名提示音（與控制台共用同一個 localStorage key） ──────────────
  var SOUND_KEY = 'tronclass_rollcall_sound_settings_v1';
  function soundGet() {
    try {
      return Object.assign({ enabled: true, volume: 0.35, pattern: 'double' }, JSON.parse(localStorage.getItem(SOUND_KEY) || '{}'));
    } catch (_) { return { enabled: true, volume: 0.35, pattern: 'double' }; }
  }
  function soundSet(next) {
    var value = Object.assign(soundGet(), next || {});
    try { localStorage.setItem(SOUND_KEY, JSON.stringify(value)); } catch (_) { }
    return value;
  }
  function soundTest() {
    try {
      var Ctx = window.AudioContext || window.webkitAudioContext;
      if (!Ctx) return;
      var ctx = new Ctx();
      var s = soundGet();
      var gain = ctx.createGain();
      gain.gain.value = Math.max(0.0001, Number(s.volume == null ? 0.35 : s.volume));
      gain.connect(ctx.destination);
      var notes = s.pattern === 'chime' ? [660, 880, 1100] : [880, 880];
      notes.forEach(function (freq, i) {
        var osc = ctx.createOscillator();
        osc.type = 'sine';
        osc.frequency.value = freq;
        osc.connect(gain);
        var t0 = ctx.currentTime + i * 0.18;
        osc.start(t0);
        osc.stop(t0 + 0.15);
      });
      setTimeout(function () { ctx.close().catch(function () { }); }, 900);
    } catch (_) { }
  }
  function bindSoundControls() {
    var enabled = q('rollcallSoundEnabled'), pattern = q('rollcallSoundPattern'), volume = q('rollcallSoundVolume'), test = q('rollcallSoundTest');
    if (!enabled || !pattern || !volume || !test) return;
    var value = soundGet();
    enabled.checked = !!value.enabled;
    pattern.value = value.pattern || 'double';
    volume.value = Math.round(Number(value.volume == null ? 0.35 : value.volume) * 100);
    function save() { soundSet({ enabled: !!enabled.checked, pattern: pattern.value, volume: Number(volume.value) / 100 }); }
    enabled.addEventListener('change', save);
    pattern.addEventListener('change', save);
    volume.addEventListener('input', save);
    test.addEventListener('click', function () { save(); soundTest(); });
  }

  // ── 啟動 ─────────────────────────────────────────────────────────────────
  function boot() {
    verifySession();

    var reload = q('notifyReload');
    if (reload) reload.addEventListener('click', function () { fillAccountSelect(q('notifyAccount'), loadNotifyPrefs); });
    var save = q('notifySave');
    if (save) save.addEventListener('click', saveNotifyPrefs);
    var allOn = q('notifyAllOn');
    if (allOn) allOn.addEventListener('click', function () { setAllNotify(true); });
    var allOff = q('notifyAllOff');
    if (allOff) allOff.addEventListener('click', function () { setAllNotify(false); });
    var account = q('notifyAccount');
    if (account) account.addEventListener('change', loadNotifyPrefs);

    var crReload = q('classReminderReload');
    if (crReload) crReload.addEventListener('click', function () { fillAccountSelect(q('classReminderAccount'), loadClassReminderPrefs); });
    var crSave = q('classReminderSave');
    if (crSave) crSave.addEventListener('click', saveClassReminder);
    var crAccount = q('classReminderAccount');
    if (crAccount) crAccount.addEventListener('change', loadClassReminderPrefs);
    Array.prototype.slice.call(document.querySelectorAll('[data-class-min]')).forEach(function (btn) {
      btn.addEventListener('click', function () { q('classReminderMinutes').value = btn.getAttribute('data-class-min'); });
    });

    var dgReload = q('digestPrefReload');
    if (dgReload) dgReload.addEventListener('click', function () { fillAccountSelect(q('digestPrefAccount'), loadDigestPref); });
    var dgSave = q('digestPrefSave');
    if (dgSave) dgSave.addEventListener('click', saveDigestPref);
    var dgAccount = q('digestPrefAccount');
    if (dgAccount) dgAccount.addEventListener('change', loadDigestPref);

    var hwReload = q('hwLadderReload');
    if (hwReload) hwReload.addEventListener('click', function () { fillAccountSelect(q('hwLadderAccount'), loadHwLadder); });
    var hwSave = q('hwLadderSave');
    if (hwSave) hwSave.addEventListener('click', saveHwLadder);
    var hwAdd = q('hwLadderAdd');
    if (hwAdd) hwAdd.addEventListener('click', function () { renderHwLadderRow(''); });
    var hwAccount = q('hwLadderAccount');
    if (hwAccount) hwAccount.addEventListener('change', loadHwLadder);
    Array.prototype.slice.call(document.querySelectorAll('[data-hw-ladder]')).forEach(function (btn) {
      btn.addEventListener('click', function () {
        var raw = btn.getAttribute('data-hw-ladder') || '';
        var stages = raw ? raw.split(',').map(function (s) { return parseInt(s, 10); }).filter(function (n) { return Number.isFinite(n); }) : [];
        renderHwLadder(stages);
      });
    });

    bindSoundControls();

    if (q('bindingAccount')) {
      fillAccountSelect(q('bindingAccount'), loadBinding);
      q('bindingAccount').addEventListener('change', loadBinding);
      q('bindingSave').addEventListener('click', saveBinding);
      q('bindingReload').addEventListener('click', function () { lastStatus = null; forceStatusRefresh = true; fillAccountSelect(q('bindingAccount'), loadBinding); });
      /* 從新手導覽的「去綁定」按鈕帶 #binding 進來時，捲到綁定區並聚焦輸入框 */
      if (location.hash === '#binding') {
        setTimeout(function () {
          var sec = q('binding'); if (sec && sec.scrollIntoView) sec.scrollIntoView({ behavior: 'smooth', block: 'start' });
          var inp = q('bindingDiscord'); if (inp && !inp.value) { try { inp.focus({ preventScroll: true }); } catch (_) {} }
        }, 350);
      }
    }
    fillAccountSelect(q('notifyAccount'), loadNotifyPrefs);
    fillAccountSelect(q('classReminderAccount'), loadClassReminderPrefs);
    fillAccountSelect(q('hwLadderAccount'), loadHwLadder);
    if (q('digestPrefAccount')) fillAccountSelect(q('digestPrefAccount'), loadDigestPref);
  }

  window.TronclassNotificationsPage = { reload: loadNotifyPrefs };

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
})();
