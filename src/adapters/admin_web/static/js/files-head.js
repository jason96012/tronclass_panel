/* INLINE_SPLIT_V1：files-head.js 原本內嵌於 files.html 的 <head> 內聯 <script>，抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── MO_STORM_GUARD_V1：MutationObserver 風暴斷路器（防禦性）──
   若某個 observer 的 callback 在 2 秒內同步觸發超過 3000 次（正常開機每個 observer 只有數十次），
   代表「callback 內的 DOM 寫入 → 再次觸發自己」的無限 microtask 迴圈（主執行緒會被卡死、畫面永遠是「—」）。
   此時立刻 disconnect 該 observer，並透過 client-log 回報 kind=mo_storm（含建立點堆疊），讓問題在後台可見而非靜默。 */
(function(){
  'use strict';
  if (window.MO_STORM_GUARD_V1) return;
  window.MO_STORM_GUARD_V1 = true;
  var Native = window.MutationObserver;
  if (typeof Native !== 'function') return;
  var LIMIT = 3000, WINDOW_MS = 2000, reported = 0;
  function report(origin, count){
    if (reported >= 3) return;
    reported += 1;
    var msg = 'MutationObserver storm broken: ' + count + ' callbacks within ' + WINDOW_MS + 'ms (observer disconnected)';
    try { console.error('[MO_STORM_GUARD_V1]', msg, origin); } catch(_) {}
    var tries = 0;
    (function send(){
      try {
        if (window.TronclassClientLog && typeof window.TronclassClientLog.report === 'function') {
          window.TronclassClientLog.report('mo_storm', msg, { stack: origin });
          return;
        }
      } catch(_) {}
      if (++tries < 20) setTimeout(send, 500);
    })();
  }
  function Guarded(callback){
    if (!(this instanceof Guarded)) throw new TypeError("Failed to construct 'MutationObserver': Please use the 'new' operator");
    var origin = '';
    try { origin = String((new Error()).stack || '').split('\n').slice(1, 5).join('\n'); } catch(_) {}
    var count = 0, windowStart = 0, tripped = false, self = this;
    var native = new Native(function(records, observer){
      if (tripped) return;
      var now = Date.now();
      if (now - windowStart > WINDOW_MS) { windowStart = now; count = 0; }
      if (++count > LIMIT) {
        tripped = true;
        try { native.disconnect(); } catch(_) {}
        report(origin, count);
        return;
      }
      return callback.call(self, records, self);
    });
    this.__native = native;
  }
  Guarded.prototype.observe = function(target, options){ return this.__native.observe(target, options); };
  Guarded.prototype.disconnect = function(){ return this.__native.disconnect(); };
  Guarded.prototype.takeRecords = function(){ return this.__native.takeRecords(); };
  try { Guarded.prototype.constructor = Guarded; } catch(_) {}
  try { window.MutationObserver = Guarded; window.__nativeMutationObserver = Native; } catch(_) {}
})();

/* ── inline block: BAN_CRUDE_LOGIN_V1 (原 files.html:24) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="BAN_CRUDE_LOGIN_V1">，瀏覽器會把有 id 的元素暴露成
     window.BAN_CRUDE_LOGIN_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
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

/* ── inline block: hideAdminTokenFromUrlEarlyV1 (原 files.html:55) ── */
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
        localStorage.removeItem(LS_SESSION);
        sessionStorage.removeItem(LS_SESSION);
        localStorage.removeItem(LS_LOGOUT); /* LOGOUT_SERVER_SIDE_V1 */
      } else if (token) {
        localStorage.setItem(LS_SESSION, token);
        sessionStorage.setItem(LS_SESSION, token);
        localStorage.removeItem(LS_LOGOUT);
      }
      if (u.searchParams.has('token')) { u.searchParams.delete('token'); changed = true; }
      if (changed) history.replaceState(null, document.title, u.pathname + (u.search ? u.search : '') + u.hash);
    } catch(_) {}
  }
  sanitizeCurrentUrl();
  window.tronAdminToken = function(){
    try {
      var u = '';
      if (u) return String(u).trim();
      return (localStorage.getItem(LS_SESSION) || sessionStorage.getItem(LS_SESSION) || '').trim();
    } catch(_) { return ''; }
  };
})();

/* ── inline block: consolidatedAdminAuthEarlyV1 (原 files.html:105) ── */
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
    fetch('/admin/api/status', {credentials:'same-origin', cache:'no-store'}).then(function(r){
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

/* ── inline block: optimizeV47Theme (原 files.html:483) ── */
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
  
