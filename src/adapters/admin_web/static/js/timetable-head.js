/* INLINE_SPLIT_V1：timetable-head.js 原本內嵌於 timetable.html 的 <head> 內聯 <script>，抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: BAN_CRUDE_LOGIN_V1 (原 timetable.html:29) ── */
(function(){
  if(window.BAN_CRUDE_LOGIN_V1) return;
  window.BAN_CRUDE_LOGIN_V1 = true;
  function isCrude(card){
    if(!card) return false;
    if(card.querySelector('.animated-login-card-inner')) return false;
    var tx=(card.textContent||'').replace(/\s+/g,'');
    return !!(card.querySelector('.login-grid') || tx.indexOf('需要登入')>=0 || tx.indexOf('帳號 / account')>=0);
  }
  function kill(root){
    var scope=root&&root.querySelectorAll?root:document;
    var cards=[];
    if(root&&root.id==='loginCard') cards.push(root);
    try{ cards=cards.concat(Array.prototype.slice.call(scope.querySelectorAll('#loginCard,.login-card,#loginPanel'))); }catch(_){}
    cards.forEach(function(card){
      if(!isCrude(card)) return;
      card.innerHTML='';
      card.hidden=true;
      card.setAttribute('aria-hidden','true');
      card.style.display='none';
    });
  }
  kill(document);
  try{ new MutationObserver(function(ms){ ms.forEach(function(m){ Array.prototype.forEach.call(m.addedNodes||[], function(n){ if(n.nodeType===1) kill(n); }); }); }).observe(document.documentElement,{childList:true,subtree:true}); }catch(_){}
})();

/* ── inline block: hideAdminTokenFromUrlEarlyV1 (原 timetable.html:67) ── */
(function(){
  'use strict';
  if (window.HIDE_ADMIN_TOKEN_FROM_URL_EARLY_V1) return;
  window.HIDE_ADMIN_TOKEN_FROM_URL_EARLY_V1 = true;
  var LS_SESSION = 'tronclass_admin_session';
  var LS_LOGOUT = 'tronclass_dashboard_manual_logout';
  function sanitizeCurrentUrl(){
    try {
      var u = new URL(location.href);
      var changed = false;
      var token = u.searchParams.get('token') || '';
      if (u.searchParams.get('logout') === '1') {
        /* LOGOUT_SERVER_SIDE_V1：?logout=1 已無意義，只清遺留旗標。 */
        localStorage.removeItem(LS_SESSION);
        localStorage.removeItem(LS_LOGOUT);
      } else if (token) {
        localStorage.setItem(LS_SESSION, token);
        localStorage.removeItem(LS_LOGOUT);
      }
      if (u.searchParams.has('token')) { u.searchParams.delete('token'); changed = true; }
      if (changed) history.replaceState(null, document.title, u.pathname + (u.search ? u.search : '') + u.hash);
    } catch(_) {}
  }
  sanitizeCurrentUrl();
  window.tronAdminToken = function(){ try { return (localStorage.getItem(LS_SESSION) || '').trim(); } catch(_) { return ''; } };
})();

/* ── inline block: consolidatedAdminAuthEarlyV1 (原 timetable.html:131) ── */
(function(){
  'use strict';
  if (window.COOKIE_SUBPAGE_AUTH_V1) return;
  window.COOKIE_SUBPAGE_AUTH_V1 = true;
  var LO = 'tronclass_dashboard_manual_logout';
  try {
    var p = new URLSearchParams(location.search);
    /* LOGOUT_SERVER_SIDE_V1：登出已改為伺服器端（POST /admin/logout 清 cookie）；
       不再以 ?logout=1 / tronclass_dashboard_manual_logout 判斷，只清掉遺留旗標。 */
    try { localStorage.removeItem(LO); } catch (_) {}
    if (p.has('logout')) { p.delete('logout'); history.replaceState(null, '', location.pathname + (p.toString() ? '?' + p.toString() : '') + location.hash); }
    /* LOGIN_PERF_V1：這頁是伺服器驗證過 session 才送出的（pages::inject_auth_attrs 會在 <html>
       寫入 data-auth="ok" 與 data-account / data-admin / data-viewer）。有這個屬性就直接進入
       已登入版面，不再多打一次 /admin/api/status 當「開機驗證」——省一趟往返，第一幀不閃。
       沒有屬性（理論上不會發生）才退回舊的網路驗證。 */
    var authOk = document.documentElement.getAttribute('data-auth') === 'ok';
    if (authOk) {
      window.__tronAuthStatus = window.__tronAuthStatus || {
        current_account_id: document.documentElement.getAttribute('data-account') || '',
        is_admin: document.documentElement.getAttribute('data-admin') === '1',
        is_viewer: document.documentElement.getAttribute('data-viewer') === '1',
        __partial: true
      };
      window.__tronAuthState = 'ok';
      document.documentElement.classList.remove('tron-auth-pending','tron-auth-logged-out');
      document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','auth-ok');
      if (document.body) document.body.classList.add('logged-in','auth-ok','dash-auth-ok');
      else document.addEventListener('DOMContentLoaded', function(){ document.body.classList.add('logged-in','auth-ok','dash-auth-ok'); }, { once: true });
    } else {
    document.documentElement.classList.add('tron-auth-pending');
    fetch('/admin/api/status?include_history=false'+(window.__tronBootToken?('&token='+encodeURIComponent(window.__tronBootToken)):''), {credentials:'same-origin', cache:'no-store'}).then(function(r){
      if (r.ok) {
        document.documentElement.classList.remove('tron-auth-pending','tron-auth-logged-out');
        document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','auth-ok');
        if (document.body) document.body.classList.add('logged-in','auth-ok','dash-auth-ok');
        return;
      }
      var next = encodeURIComponent(location.pathname + location.search + location.hash);
      location.replace('/admin?next=' + next);
    }).catch(function(){
      var next = encodeURIComponent(location.pathname + location.search + location.hash);
      location.replace('/admin?next=' + next);
    });
    }
  } catch(_) {
    location.replace('/admin');
  }
})();

/* ── inline block: optimizeV47Theme (原 timetable.html:201) ── */
  (function(){
    'use strict';
    var mq=window.matchMedia('(prefers-color-scheme:dark)');
  var THEME_LS='tronclass_app_settings_v1';
  function themeChoice(){try{var s=JSON.parse(localStorage.getItem(THEME_LS)||'{}');var t=String(s.theme||'auto').toLowerCase();return(t==='light'||t==='dark')?t:'auto'}catch(_){return'auto'}}
  function applyTheme(){var c=themeChoice();document.documentElement.setAttribute('data-theme',c==='auto'?(mq.matches?'dark':'light'):c)}
  /* THEME_CHOICE_V1: 使用者設定（外觀設定頁）優先；auto 才跟隨系統，並同步跨分頁變更 */
  applyTheme();
  try{mq.addEventListener('change',applyTheme)}catch(e){mq.addListener(applyTheme)}
  try{window.addEventListener('storage',function(ev){if(!ev||!ev.key||ev.key===THEME_LS)applyTheme()})}catch(_){}
  window.__setTheme=function(t){document.documentElement.setAttribute('data-theme',t||'dark')};
    document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','mobile-nav-final-auth');
    if(document.body) document.body.classList.add('auth-ok','dash-auth-ok','logged-in');
  })();
  
