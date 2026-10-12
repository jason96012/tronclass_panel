// shares-admin.js — 分享連結管理頁前端（管理員）。
// TRUSTED_TYPES_V1: 後台 CSP 為 require-trusted-types-for 'script' + trusted-types default dompurify，
// innerHTML 字串會經過頁面既有的 default policy（trusted-types-init.js 或 pages.rs 的 inline 守衛，
// 皆為消毒型，見 M1）。本檔不再自建 identity policy：那會把已消毒的 default 覆寫成原樣放行。
(function () {
  'use strict';

  function setHTML(el, html) {
    el.innerHTML = html;
  }

  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c];
    });
  }
  function fmtDate(secs) {
    try { return new Date(secs * 1000).toLocaleString(); } catch (e) { return String(secs); }
  }
  function buildLink(rec) {
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

  function render(list) {
    var content = document.getElementById('content');
    if (!list || !list.length) { setHTML(content, '<div class="empty">尚無分享連結</div>'); return; }
    var now = Math.floor(Date.now() / 1000);
    var rows = list.map(function (r, i) {
      var revoked = !!r.revoked;
      var expired = r.exp < now;
      var badge = revoked ? '<span class="badge rev">已撤銷</span>'
        : expired ? '<span class="badge exp">已過期</span>'
        : '<span class="badge ok">有效</span>';
      return '<tr>'
        + '<td class="path">' + esc(r.path) + '</td>'
        + '<td>' + fmtDate(r.exp) + '</td>'
        + '<td>' + badge + '</td>'
        + '<td class="td-nowrap">'
        + '<button type="button" data-qr="' + i + '">QR／複製</button> '
        + (revoked ? '' : '<button type="button" class="danger" data-revoke="' + i + '">撤銷</button>')
        + '</td></tr>';
    }).join('');
    setHTML(content, '<table><thead><tr><th>檔案路徑</th><th>到期時間</th><th>狀態</th><th>動作</th></tr></thead><tbody>' + rows + '</tbody></table>');
    content.querySelectorAll('[data-qr]').forEach(function (b) {
      b.addEventListener('click', function () { showQr(buildLink(list[+b.getAttribute('data-qr')])); });
    });
    content.querySelectorAll('[data-revoke]').forEach(function (b) {
      b.addEventListener('click', function () {
        var rec = list[+b.getAttribute('data-revoke')];
        if (!confirm('確定撤銷此分享連結？\n' + rec.path)) return;
        fetch('/admin/api/shares/revoke', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ path: rec.path, exp: rec.exp })
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
