/* SHARED_MOBILE_NAV_V2: 在所有手機頁（雲端硬碟除外）重現與 dashboard 完全相同的
   #mobileNavFinalV2 底部導覽列 + #mobileMoreFinalV2 更多面板。
   直接載入 dashboard 使用的 mobile-navigation.css，並建立一模一樣的 DOM 與 SVG 圖示。 */
(function () {
  'use strict';
  if (window.__SHARED_MOBILE_NAV_V2__) return;
  window.__SHARED_MOBILE_NAV_V2__ = true;
  // dashboard 自己會建 nav，不重複。
  if (document.getElementById('mobileNavFinalV2')) return;
  function isMobile() { return window.matchMedia && window.matchMedia('(max-width:820px)').matches; }
  if (!isMobile()) return;

  // 與 dashboard 相同的導覽列 CSS。
  if (!document.querySelector('link[href^="/admin/static/mobile-navigation.css"]')) {
    var link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = '/admin/static/mobile-navigation.css?v=mnav2';
    document.head.appendChild(link);
  }
  // 各頁舊底欄（課表頁 ttMobileNavV2 等）讓位給共用導覽列；body 預留空間。
  // 更多面板內 a/button 字級統一（避免某些頁的 button 預設字級較小）。
  var st = document.createElement('style');
  st.textContent = '#ttMobileNavV2{display:none!important}' +
    /* 手機：底部導覽列不遮內容（雲端硬碟未載入此腳本，不受影響） */
    '@media(max-width:820px){' +
    'body{padding-bottom:calc(96px + env(safe-area-inset-bottom,0px))!important}' +
    'main,.page,.wrap{padding-bottom:calc(120px + env(safe-area-inset-bottom,0px))!important}' +
    '}' +
    '#mobileMoreFinalV2 .more-grid a,#mobileMoreFinalV2 .more-grid button{font-size:15px!important;font-weight:900!important;min-height:48px!important}' +
    /* 深色模式（手動 data-theme）：導覽列容器/按鈕/面板跟隨 */
    'html[data-theme="dark"] #mobileNavFinalV2{background:rgba(15,23,42,.94)!important;border-color:rgba(148,163,184,.18)!important}' +
    'html[data-theme="dark"] #mobileNavFinalV2 button{background:transparent!important;border:0!important;box-shadow:none!important;color:#94a3b8!important}' +
    'html[data-theme="dark"] #mobileNavFinalV2 button.active{background:rgba(59,130,246,.18)!important;color:#93c5fd!important}' +
    'html[data-theme="dark"] #mobileMoreFinalV2 .sheet{background:rgba(22,29,41,.97)!important;border-color:rgba(148,163,184,.18)!important}' +
    'html[data-theme="dark"] #mobileMoreFinalV2 h3{color:#f1f5f9!important}' +
    'html[data-theme="dark"] #mobileMoreFinalV2 .more-grid a,html[data-theme="dark"] #mobileMoreFinalV2 .more-grid button{background:rgba(30,41,59,.65)!important;color:#f1f5f9!important;border-color:#334155!important}' +
    'html[data-theme="dark"] #mobileMoreFinalV2 .more-grid .danger,html[data-theme="dark"] #mobileMoreFinalV2 .more-grid #moreLogoutFinalV2{background:rgba(248,113,113,.12)!important;color:#fca5a5!important;border-color:rgba(248,113,113,.35)!important}' +
    /* 深色模式（跟隨系統） */
    '@media(prefers-color-scheme:dark){' +
    'html:not([data-theme="light"]) #mobileNavFinalV2{background:rgba(15,23,42,.94)!important;border-color:rgba(148,163,184,.18)!important}' +
    'html:not([data-theme="light"]) #mobileNavFinalV2 button{background:transparent!important;border:0!important;box-shadow:none!important;color:#94a3b8!important}' +
    'html:not([data-theme="light"]) #mobileNavFinalV2 button.active{background:rgba(59,130,246,.18)!important;color:#93c5fd!important}' +
    'html:not([data-theme="light"]) #mobileMoreFinalV2 .sheet{background:rgba(22,29,41,.97)!important;border-color:rgba(148,163,184,.18)!important}' +
    'html:not([data-theme="light"]) #mobileMoreFinalV2 h3{color:#f1f5f9!important}' +
    'html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid a,html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid button{background:rgba(30,41,59,.65)!important;color:#f1f5f9!important;border-color:#334155!important}' +
    'html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid .danger,html:not([data-theme="light"]) #mobileMoreFinalV2 .more-grid #moreLogoutFinalV2{background:rgba(248,113,113,.12)!important;color:#fca5a5!important;border-color:rgba(248,113,113,.35)!important}' +
    '}';
  document.head.appendChild(st);

  function q(id) { return document.getElementById(id); }
  // 與 dashboard mobileDashboardNavMoreLayoutScriptV2 完全相同的圖示。
  function icon(name) {
    var icons = {
      overview: '<svg viewBox="0 0 24 24"><path d="M4 13h6V4H4z"></path><path d="M14 20h6V4h-6z"></path><path d="M4 20h6v-3H4z"></path></svg>',
      accounts: '<svg viewBox="0 0 24 24"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"></path><circle cx="9" cy="7" r="4"></circle></svg>',
      rollcall: '<svg viewBox="0 0 24 24"><path d="M9 11l3 3L22 4"></path><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"></path></svg>',
      timetable: '<svg viewBox="0 0 24 24"><path d="M4 5h16v15H4z"></path><path d="M8 3v4"></path><path d="M16 3v4"></path><path d="M4 10h16"></path></svg>',
      more: '<svg viewBox="0 0 24 24"><circle cx="5" cy="12" r="1.5"></circle><circle cx="12" cy="12" r="1.5"></circle><circle cx="19" cy="12" r="1.5"></circle></svg>'
    };
    return icons[name] || '';
  }

  function logout() {
    fetch('/admin/api/logout', { method: 'POST', credentials: 'same-origin', cache: 'no-store' })
      .catch(function () {})
      .then(function () {
        try { localStorage.setItem('tronclass_dashboard_manual_logout', '1'); } catch (_) {}
        location.replace('/admin?logout=1&v=securelogout' + Date.now());
      });
  }
  function closeMore() { var el = q('mobileMoreFinalV2'); if (el) el.classList.remove('visible'); }
  function openMore() { var el = q('mobileMoreFinalV2'); if (el) el.classList.add('visible'); }

  var isAdmin = false;
  function build() {
    if (q('mobileNavFinalV2')) return;
    // mobile-navigation.css 以這些 class 作為「已登入」開關。
    document.documentElement.classList.add('dash-auth-ok', 'mobile-nav-final-auth');
    if (document.body) document.body.classList.add('logged-in', 'auth-ok', 'dash-auth-ok');

    var nav = document.createElement('nav');
    nav.id = 'mobileNavFinalV2';
    nav.innerHTML = '' +
      '<button data-go="/admin/dashboard#overview">' + icon('overview') + '<span>總覽</span></button>' +
      '<button data-go="/admin/dashboard#accounts">' + icon('accounts') + '<span>帳號</span></button>' +
      '<button data-go="/admin/dashboard#rollcall">' + icon('rollcall') + '<span>點名</span></button>' +
      '<button data-go="/admin/dashboard#timetable">' + icon('timetable') + '<span>課表</span></button>' +
      '<button id="sharedMoreBtnFinalV2">' + icon('more') + '<span>更多</span></button>';
    document.body.appendChild(nav);
    nav.querySelectorAll('[data-go]').forEach(function (b) {
      b.onclick = function () { location.assign(b.getAttribute('data-go')); };
    });
    q('sharedMoreBtnFinalV2').onclick = function () {
      var el = q('mobileMoreFinalV2');
      if (el && el.classList.contains('visible')) closeMore(); else openMore();
    };

    var sheet = document.createElement('div');
    sheet.id = 'mobileMoreFinalV2';
    sheet.innerHTML = '<div class="sheet"><h3>更多功能</h3><div class="more-grid">' +
      '<a id="moreSettingsFinalV2" class="button secondary" href="/admin/settings">設定</a>' +
      '<a id="moreHomeworkFinalV2" class="button secondary" href="/admin/homework">作業截止時間</a>' +
      '<a id="moreDriveFinalV2" class="button secondary" href="/admin/files">雲端硬碟</a>' +
      '<a id="moreBulletinsFinalV2" class="button secondary" href="/admin/bulletins">學校公告</a>' +
      '<a id="moreAiFinalV2" class="button secondary" href="/admin/ai">AI 對話</a>' +
      (isAdmin ? '<a id="moreActivityFinalV2" class="button secondary" href="/admin/activity">登入/檔案紀錄</a>' : '') +
      (isAdmin ? '<a id="moreBlacklistFinalV2" class="button secondary" href="/admin/blacklist">黑名單</a>' : '') +
      '<button id="moreRefreshFinalV2" class="secondary" type="button">重新整理</button>' +
      '<a id="moreDetectionScheduleFinalV2" class="button secondary" href="/admin/detection-schedule">點名排程</a>' +
      '<button id="moreLogoutFinalV2" class="danger" type="button">登出</button>' +
      '</div></div>';
    document.body.appendChild(sheet);
    // 雙保險：inline 樣式統一字級（優先於任何頁面的 button/a 預設樣式）。
    sheet.querySelectorAll('.more-grid a,.more-grid button').forEach(function (el) {
      el.style.fontSize = '15px';
      el.style.fontWeight = '900';
      el.style.minHeight = '48px';
    });
    sheet.onclick = function (ev) { if (ev.target === sheet) closeMore(); };
    q('moreRefreshFinalV2').onclick = function () { location.reload(); };
    q('moreLogoutFinalV2').onclick = logout;
  }

  fetch('/admin/api/status', { credentials: 'same-origin', cache: 'no-store' })
    .then(function (r) { return r.ok ? r.json() : {}; })
    .then(function (j) { isAdmin = !!(j && j.is_admin); build(); })
    .catch(function () { build(); });
})();
