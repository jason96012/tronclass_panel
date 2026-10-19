// shares-admin.js — 分享連結管理頁前端（管理員）。
// TRUSTED_TYPES_V1: 後台 CSP 為 require-trusted-types-for 'script' + trusted-types default dompurify，
// innerHTML 字串會經過頁面既有的 default policy（trusted-types-init.js 或 pages.rs 的 inline 守衛，
// 皆為消毒型，見 M1）。本檔不再自建 identity policy：那會把已消毒的 default 覆寫成原樣放行。
// SHARE_V2: 新增密碼／檢視次數／最後存取欄位、存取紀錄展開列；撤銷時帶 sid 精準撤銷。
(function () {
  'use strict';

  function setHTML(el, html) {
    el.innerHTML = html;
  }

  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>\"']/g, function (c) {
      return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '\"': '&quot;', "'": '&#39;' })[c];
    });
  }
  function fmtDate(secs) {
    try { return new Date(secs * 1000).toLocaleString(); } catch (e) { return String(secs); }
  }
  var ACTION_LABELS = {
    view: '開啟預覽',
    download: '下載',
    unlock_ok: '密碼正確',
    unlock_fail: '密碼錯誤',
    blocked_limit: '達次數上限（已擋）'
  };
  // 新連結用伺服器給的 public_path（含 sid 與 v2 簽章）；舊連結退回 v1 組法。
  function buildLink(rec) {
    if (rec.public_path) return location.origin + rec.public_path;
    return location.origin + '/pub/share?path=' + encodeURIComponent(rec.path) + '&exp=' + rec.exp + '&sig=' + rec.sig;
  }

  function showQr(link) {
    var old = document.getElementById('share-modal'); if (old) old.remove();
    var bg = document.createElement('div'); bg.id = 'share-modal'; bg.className = 'modal-bg';
    var m = document.createElement('div'); m.className = 'modal';
    setHTML(m,
      '<div class="share-modal-title">分享連結</div>'
      + '<div id="qr" class="share-qr-box"></div>'
      + '<div class="linkbox" id="lnk"></div>'
      + '<div class="share-actions">'
      + '<button id="cp" class="primary" type="button">複製連結</button>'
      + '<button id="cl" type="button">關閉</button></div>');
    bg.appendChild(m); document.body.appendChild(bg);
    document.getElementById('lnk').textContent = link;
    try {
      if (typeof qrcode === 'function') {
        var qr = qrcode(0, 'M'); qr.addData(link); qr.make();
        setHTML(document.getElementById('qr'), qr.createSvgTag({ cellSize: 5, margin: 2, scalable: true }));
        var svg = document.getElementById('qr').querySelector('svg'); if (svg) { svg.style.width = '220px'; svg.style.height = '220px'; }
      } else { document.getElementById('qr').textContent = '（QR 需 qrcode.min.js）'; }
    } catch (e) { document.getElementById('qr').textContent = 'QR 產生失敗'; }
    document.getElementById('cp').onclick = function () {
      (navigator.clipboard ? navigator.clipboard.writeText(link) : Promise.reject())
        .then(function () { document.getElementById('cp').textContent = '已複製'; })
        .catch(function () { window.prompt('複製：', link); });
    };
    document.getElementById('cl').onclick = function () { bg.remove(); };
    bg.onclick = function (e) { if (e.target === bg) bg.remove(); };
  }

  function viewsLabel(r) {
    if (!r.sid) return '—';
    return r.max_views ? (r.views + ' / ' + r.max_views) : (r.views + ' / 不限');
  }
  function lastLabel(r) {
    return r.sid && r.last_access ? fmtDate(r.last_access) : '—';
  }
  function logHtml(r) {
    if (!r.access || !r.access.length) return '<div class="empty">尚無存取紀錄</div>';
    var rows = r.access.map(function (a) {
      return '<tr><td class="td-nowrap">' + esc(fmtDate(a.time)) + '</td>'
        + '<td>' + esc(ACTION_LABELS[a.action] || a.action) + '</td>'
        + '<td>' + esc(a.ip) + '</td>'
        + '<td class="ua">' + esc(a.ua) + '</td></tr>';
    }).join('');
    var more = (r.access_total > r.access.length)
      ? '<div class="sub">僅顯示最近 ' + r.access.length + ' 筆（共 ' + r.access_total + ' 筆，最多保留 200 筆）</div>'
      : '';
    return '<table class="log"><thead><tr><th>時間</th><th>動作</th><th>IP</th><th>裝置／瀏覽器</th></tr></thead><tbody>'
      + rows + '</tbody></table>' + more;
  }

  function render(list) {
    var content = document.getElementById('content');
    if (!list || !list.length) { setHTML(content, '<div class="empty">尚無分享連結</div>'); return; }
    var now = Math.floor(Date.now() / 1000);
    var active = list.filter(function (r) { return !r.revoked && r.exp >= now; }).length;
    var totalViews = list.reduce(function (s, r) { return s + (r.views || 0); }, 0);
    var summary = '<div class="sub">共 ' + list.length + ' 條｜有效 ' + active + ' 條｜累計開啟預覽 ' + totalViews + ' 次</div>';
    var rows = list.map(function (r, i) {
      var revoked = !!r.revoked;
      var expired = r.exp < now;
      var badge = revoked ? '<span class="badge rev">已撤銷</span>'
        : expired ? '<span class="badge exp">已過期</span>'
        : '<span class="badge ok">有效</span>';
      return '<tr>'
        + '<td class="path">' + esc(r.path) + (r.sid ? '' : ' <span class="tag">舊版</span>') + '</td>'
        + '<td class="td-nowrap">' + fmtDate(r.exp) + '</td>'
        + '<td>' + badge + '</td>'
        + '<td>' + (r.sid ? (r.has_password ? '🔒 有' : '—') : '—') + '</td>'
        + '<td class="td-nowrap">' + esc(viewsLabel(r)) + '</td>'
        + '<td class="td-nowrap">' + esc(lastLabel(r)) + '</td>'
        + '<td class="td-nowrap">'
        + '<button type="button" data-qr="' + i + '">QR／複製</button> '
        + (r.sid ? '<button type="button" data-log="' + i + '">紀錄（' + (r.access_total || 0) + '）</button> ' : '')
        + (revoked ? '' : '<button type="button" class="danger" data-revoke="' + i + '">撤銷</button>')
        + '</td></tr>'
        + (r.sid ? '<tr class="log-row" id="log-' + i + '" hidden><td colspan="7">' + logHtml(r) + '</td></tr>' : '');
    }).join('');
    setHTML(content, summary
      + '<table><thead><tr><th>檔案路徑</th><th>到期時間</th><th>狀態</th><th>密碼</th><th>檢視次數</th><th>最後存取</th><th>動作</th></tr></thead><tbody>'
      + rows + '</tbody></table>');

    content.querySelectorAll('[data-qr]').forEach(function (b) {
      b.addEventListener('click', function () { showQr(buildLink(list[+b.getAttribute('data-qr')])); });
    });
    content.querySelectorAll('[data-log]').forEach(function (b) {
      b.addEventListener('click', function () {
        var row = document.getElementById('log-' + b.getAttribute('data-log'));
        if (row) row.hidden = !row.hidden;
      });
    });
    content.querySelectorAll('[data-revoke]').forEach(function (b) {
      b.addEventListener('click', function () {
        var rec = list[+b.getAttribute('data-revoke')];
        if (!confirm('確定撤銷此分享連結？\n' + rec.path)) return;
        fetch('/admin/api/shares/revoke', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ path: rec.path, exp: rec.exp, sid: rec.sid || '' })
        }).then(function () { load(); }).catch(function () { alert('撤銷失敗'); });
      });
    });
  }

  function load() {
    setHTML(document.getElementById('content'), '<div class="empty">載入中…</div>');
    fetch('/admin/api/shares', { cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (j) { render((j && j.items) || []); })
      .catch(function () { setHTML(document.getElementById('content'), '<div class="empty">載入失敗（需管理員登入）</div>'); });
  }

  document.addEventListener('DOMContentLoaded', function () {
    var rb = document.getElementById('refresh'); if (rb) rb.addEventListener('click', load);
    load();
  });
})();
