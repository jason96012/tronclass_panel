/* INLINE_SPLIT_V1：dashboard-head.js 原本內嵌於 dashboard.html 的 <head> 內聯 <script>，抽成靜態檔（CSP script-src/style-src 'self'；
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

/* ── inline block: dashboardPerformanceGuardV2 (原 dashboard.html:12) ── */
(function () {
  'use strict';
  if (window.DASHBOARD_PERFORMANCE_GUARD_V2) return;
  window.DASHBOARD_PERFORMANCE_GUARD_V2 = true;

  // DASHBOARD_STATUS_FETCH_DEDUPE_V2:
  // Coordinate legacy inline scripts and deferred modules that still call
  // fetch('/admin/api/status') directly.  Cache and in-flight maps are keyed by
  // normalized URL, so include_history/history_limit variants never share the
  // wrong payload.
  var nativeFetch = window.fetch.bind(window);
  var statusCacheByKey = Object.create(null);
  var statusRequestByKey = Object.create(null);
  var ACTIVE_CACHE_MS = 20000;
  var HIDDEN_CACHE_MS = 60000;

  function requestUrl(input) {
    try {
      if (typeof input === 'string') return new URL(input, location.origin);
      if (input && input.url) return new URL(input.url, location.origin);
    } catch (_) {}
    return null;
  }
  function requestMethod(input, init) {
    return String((init && init.method) || (input && input.method) || 'GET').toUpperCase();
  }
  function statusKey(url) {
    // Normalize semantically identical variants onto one key. The dashboard
    // boots three separate status callers that ask for the same data in
    // slightly different ways:
    //   /admin/api/status
    //   /admin/api/status?include_history=false
    //   /admin/api/status?history_limit=0&include_history=false
    // Keying on the raw query string made all three distinct, so the dedupe
    // never fired and the slowest request on the page ran three times.
    // history_limit is meaningless once include_history=false, and `token` is
    // only a compatibility marker, so both are dropped from the key.
    var copy = new URL(url.href);
    // Timetable responses differ per account, so account_id must stay in the
    // key; only the meaningless token marker is stripped.
    if (copy.pathname === '/admin/api/timetable') {
      copy.searchParams.delete('token');
      try { copy.searchParams.sort(); } catch (_) {}
      return copy.pathname + copy.search;
    }
    var noHistory = copy.searchParams.get('include_history') === 'false'
      || copy.searchParams.get('history_limit') === '0';
    // STATUS_SLIM_V1：history_limit=8（總覽）與完整歷史（點名分頁）payload 不同，
    // 必須分開快取；只有 include_history=false 時 history_limit 才無意義。
    var limit = copy.searchParams.get('history_limit');
    copy.searchParams.delete('token');
    copy.searchParams.delete('history_limit');
    copy.searchParams.delete('include_history');
    if (noHistory) copy.searchParams.set('include_history', 'false');
    else if (limit) copy.searchParams.set('history_limit', limit);
    try { copy.searchParams.sort(); } catch (_) {}
    return copy.pathname + copy.search;
  }
  function isStatusRequest(input, init) {
    var url = requestUrl(input);
    if (!url || requestMethod(input, init) !== 'GET') return false;
    // renderAll() calls loadTimetable(), and renderAll() runs from several
    // places during boot, so the timetable was fetched twice back-to-back for
    // the same account. It is coalesced here rather than by editing
    // loadTimetable, so the many overlapping dashboard patches that call it
    // keep working untouched.
    return url.pathname === '/admin/api/status'
        || url.pathname === '/admin/api/timetable';
  }
  function isStateChangingRequest(input, init) {
    var url = requestUrl(input);
    var method = requestMethod(input, init);
    return !!url && method !== 'GET' && (
      url.pathname === '/admin/api/action' ||
      url.pathname.indexOf('/admin/api/timetable') === 0 ||
      url.pathname.indexOf('/admin/api/notifications/') === 0 ||
      url.pathname === '/admin/api/class_reminder'
    );
  }
  function clearStatusCache() {
    statusCacheByKey = Object.create(null);
    statusRequestByKey = Object.create(null);
    if (window.TronclassRuntime && window.TronclassRuntime.clearStatusCache) {
      try { window.TronclassRuntime.clearStatusCache(); } catch (_) {}
    }
  }
  function usableCache(key) {
    var cached = statusCacheByKey[key];
    if (!cached) return null;
    var ttl = document.hidden ? HIDDEN_CACHE_MS : ACTIVE_CACHE_MS;
    return (Date.now() - cached.at) < ttl ? cached.response : null;
  }

  // STATUS_ETAG_V1：/admin/api/status 的呼叫端一律帶 cache:'no-store'（避免瀏覽器 HTTP 快取
  // 吐舊資料），副作用是瀏覽器不會替我們做條件請求。這裡自己做：記住每個 key 最近一次
  // 200 的 ETag 與本文，下次同 key 請求加 If-None-Match；伺服器回 304 就用記住的本文
  // 合成一個 200 Response 回給呼叫端（呼叫端完全無感，仍是 response.json()）。
  // 只對 /admin/api/status 做（timetable 回應沒有 ETag，帶了也只是被忽略）。
  var statusEtagByKey = Object.create(null);
  function withIfNoneMatch(input, init, key) {
    var known = statusEtagByKey[key];
    if (!known || !known.etag) return init;
    var next = Object.assign({}, init);
    var h = new Headers(init.headers || (input && input.headers) || undefined);
    if (!h.has('If-None-Match')) h.set('If-None-Match', known.etag);
    next.headers = h;
    return next;
  }
  function rememberEtag(key, response) {
    try {
      var etag = response.headers.get('ETag');
      if (!etag) return response;
      return response.clone().text().then(function (body) {
        statusEtagByKey[key] = { etag: etag, body: body };
        return response;
      }, function () { return response; });
    } catch (_) { return response; }
  }
  function reviveFrom304(key, response) {
    var known = statusEtagByKey[key];
    if (!known) return null;
    return new Response(known.body, {
      status: 200,
      statusText: 'OK',
      headers: { 'content-type': 'application/json', 'etag': known.etag, 'x-status-revalidated': '304' }
    });
  }

  // STATUS_FETCH_TIMEOUT_V1：/admin/api/status 與 timetable 的請求一律加 20 秒逾時。
  // 原本沒有逾時：伺服器忙碌（熱重編佔滿 CPU／SQLite 等鎖）或 iOS 回前景後 socket 已死
  // 但瀏覽器不報錯時，nativeFetch 會一直 pending；而下面的去重表 statusRequestByKey 會讓
  // 之後每 15 秒的輪詢全部掛在同一個永遠不會結束的 promise 上 → 畫面永遠停在「尚未更新／—」。
  // 逾時後以 TimeoutError 拒絕（status=0），呼叫端（loadStatus）走失敗重試路徑。
  var STATUS_FETCH_TIMEOUT_MS = 20000;
  function timedFetch(input, init) {
    if (typeof AbortController !== 'function') return nativeFetch(input, init);
    var ctrl = new AbortController();
    var upstream = init && init.signal;
    if (upstream) {
      if (upstream.aborted) ctrl.abort();
      else upstream.addEventListener('abort', function () { ctrl.abort(); }, { once: true });
    }
    var timedOut = false;
    var timer = setTimeout(function () { timedOut = true; ctrl.abort(); }, STATUS_FETCH_TIMEOUT_MS);
    var next = Object.assign({}, init, { signal: ctrl.signal });
    return nativeFetch(input, next).then(function (response) {
      clearTimeout(timer);
      return response;
    }, function (error) {
      clearTimeout(timer);
      if (timedOut) {
        var err = new Error('status_timeout');
        err.name = 'TimeoutError';
        err.status = 0;
        throw err;
      }
      throw error;
    });
  }

  window.fetch = function (input, init) {
    // RUNTIME_PATCH_CONSOLIDATE_V1：原 HTTPONLY_SESSION_V2 的「fetch 預設帶 cookie」併入這裡，
    // 整頁只剩這一層 window.fetch 包裝（session-security.js 在更早的 <head> 注入點已抓到原生 fetch）。
    init = init || {};
    if (!init.credentials) init.credentials = 'same-origin';
    if (isStateChangingRequest(input, init)) clearStatusCache();
    if (!isStatusRequest(input, init)) return nativeFetch(input, init);

    var url = requestUrl(input);
    var key = statusKey(url);
    var cached = usableCache(key);
    if (cached) return Promise.resolve(cached.clone());
    if (statusRequestByKey[key]) {
      return statusRequestByKey[key].then(function (response) { return response.clone(); });
    }

    var isStatusPath = url.pathname === '/admin/api/status';
    var sendInit = isStatusPath ? withIfNoneMatch(input, init, key) : init;
    statusRequestByKey[key] = timedFetch(input, sendInit).then(function (response) {
      delete statusRequestByKey[key];
      if (isStatusPath && response && response.status === 304) {
        var revived = reviveFrom304(key, response);
        if (revived) {
          statusCacheByKey[key] = { response: revived, at: Date.now() };
          return revived;
        }
        // 沒有可用的舊本文（理論上不會：只有記得 ETag 才會送 If-None-Match）→ 不帶條件重抓一次。
        delete statusEtagByKey[key];
        return timedFetch(input, init).then(function (fresh) {
          if (fresh && fresh.ok) statusCacheByKey[key] = { response: fresh, at: Date.now() };
          return fresh;
        });
      }
      if (response && response.ok) {
        statusCacheByKey[key] = { response: response, at: Date.now() };
        if (isStatusPath) return rememberEtag(key, response);
      }
      return response;
    }, function (error) {
      delete statusRequestByKey[key];
      throw error;
    });
    return statusRequestByKey[key].then(function (response) { return response.clone(); });
  };

  // LOGIN_PREFETCH_STATUS_V1：登入頁在跳轉前已把 /admin/api/status?history_limit=8 的結果
  // 放進 sessionStorage；這裡把它種進同一個去重快取（同 key），首屏的第一個 loadStatus()
  // 直接命中、零往返。只接受 15 秒內的預抓，用完即刪（只給登入後第一屏用）。
  (function seedFromLoginPrefetch() {
    var raw = null;
    try { raw = sessionStorage.getItem('tronclass_status_prefetch_v1'); sessionStorage.removeItem('tronclass_status_prefetch_v1'); } catch (_) {}
    if (!raw) return;
    try {
      var parsed = JSON.parse(raw);
      if (!parsed || !parsed.status || !parsed.status.accounts) return;
      if ((Date.now() - Number(parsed.at || 0)) > 15000) return;
      var url = new URL('/admin/api/status?history_limit=8', location.origin);
      var key = statusKey(url);
      var response = new Response(JSON.stringify(parsed.status), { status: 200, headers: { 'content-type': 'application/json' } });
      // 以預抓時間為準（不是現在），讓 20 秒的去重快取扣掉登入→跳轉的耗時。
      statusCacheByKey[key] = { response: response, at: Number(parsed.at) || Date.now() };
    } catch (_) {}
  })();

  window.TronclassStatusFetchCoordinator = {
    clear: clearStatusCache,
    activeTtlMs: ACTIVE_CACHE_MS,
    hiddenTtlMs: HIDDEN_CACHE_MS
  };

  document.addEventListener('visibilitychange', function () {
    if (!document.hidden) clearStatusCache();
  }, { passive: true });

  // Install before all dashboard patches: polling callbacks that only update UI
  // are skipped while the tab is hidden. Timers resume naturally when visible.
  //
  // RUNTIME_PATCH_CONSOLIDATE_V1：原 TT_JITTER_INTERVAL_GUARD_V1（獨立課表分頁啟用時，
  // 停掉舊 Dashboard 的課表／狀態輪詢，避免兩套渲染互相覆蓋而閃爍）也併入這一層，
  // 整頁只剩這一個 window.setInterval 包裝；timetable.js 仍以 window.__TT_STABLE_ACTIVE__ 溝通。
  window.TT_JITTER_INTERVAL_GUARD_V1 = true;
  window.__TT_STABLE_ACTIVE__ = false;
  var HIDDEN_SKIP_RE = /loadStatus|loadTimetable|loadNotify|loadClassReminder|enhanceCourses|renderDashboardFilterPanel|ensureNotify|ensurePanel/i;
  var TT_SKIP_RE = /loadStatus|renderAll|loadTimetable|renderTimetable|nextClass|ttMain|view-timetable|#timetable/i;
  var nativeSetInterval = window.setInterval.bind(window);
  window.setInterval = function (callback, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    var source = '';
    try { source = typeof callback === 'function' ? Function.prototype.toString.call(callback) : String(callback || ''); } catch (_) {}
    var skipWhenHidden = HIDDEN_SKIP_RE.test(source);
    var skipWhenTimetable = TT_SKIP_RE.test(source);
    if (typeof callback !== 'function') {
      if (typeof callback === 'string' && skipWhenTimetable) {
        return nativeSetInterval.apply(window, ["if(!window.__TT_STABLE_ACTIVE__){" + callback + "}", delay].concat(args));
      }
      return nativeSetInterval.apply(window, arguments);
    }
    if (!skipWhenHidden && !skipWhenTimetable) return nativeSetInterval.apply(window, arguments);
    var guarded = function () {
      if (skipWhenHidden && document.hidden) return;
      if (skipWhenTimetable && window.__TT_STABLE_ACTIVE__) return;
      return callback.apply(this, arguments);
    };
    return nativeSetInterval.apply(window, [guarded, delay].concat(args));
  };
})();

/* ── inline block: MORE_ALLOW_NAV_V1 (原 dashboard.html:290) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="MORE_ALLOW_NAV_V1">，瀏覽器會把有 id 的元素暴露成
     window.MORE_ALLOW_NAV_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.MORE_ALLOW_NAV_V1) return;
  window.MORE_ALLOW_NAV_V1=true;
  document.addEventListener('click', function(ev){
    var el=ev.target;
    if(el&&el.nodeType===3) el=el.parentElement;
    var a=el&&el.closest&&el.closest('a[href]');
    if(!a) return;
    var href=a.getAttribute('href')||'';
    if(href.indexOf('/admin/password')===0 || href.indexOf('/admin/passkey')===0){
      if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
    }
  }, true);
})();

/* ── inline block: MORE_DEAD_BUTTONS_V1 (原 dashboard.html:328) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="MORE_DEAD_BUTTONS_V1">，瀏覽器會把有 id 的元素暴露成
     window.MORE_DEAD_BUTTONS_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.MORE_DEAD_BUTTONS_V1) return;
  window.MORE_DEAD_BUTTONS_V1 = true;
  function elOf(ev){
    var el=ev&&ev.target;
    if(el&&el.nodeType===3) el=el.parentElement;
    return el;
  }
  function hit(ev, kind){
    var el=elOf(ev);
    if(!el||!el.closest) return null;
    var b=el.closest('button,a,[data-more-action]');
    if(!b) return null;
    var act=(b.getAttribute('data-more-action')||'');
    var id=b.id||'';
    var tx=(b.textContent||'').replace(/\s+/g,'').trim();
    if(kind==='passkey'){
      if(act==='passkey'||/passkey/i.test(id)||tx==='設定通行密鑰'||tx==='設定通行密碼') return b;
    }
    if(kind==='password'){
      if(act==='password'||/ChangePassword|changePassword/i.test(id)||tx==='變更密碼'||tx==='修改密碼') return b;
    }
    return null;
  }
  document.addEventListener('pointerup', function(ev){
    if(hit(ev,'passkey')){
      ev.preventDefault();
      if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
      ev.stopPropagation();
      if(typeof window.tronPasskeyRegister==='function') window.tronPasskeyRegister();
      return;
    }
    if(hit(ev,'password')){
      ev.preventDefault();
      if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
      ev.stopPropagation();
      if(typeof window.tronOpenChangePassword==='function') window.tronOpenChangePassword();
    }
  }, true);
})();

/* ── inline block: PASSKEY_MORE_TAP_V2 (原 dashboard.html:402) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="PASSKEY_MORE_TAP_V2">，瀏覽器會把有 id 的元素暴露成
     window.PASSKEY_MORE_TAP_V2（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.PASSKEY_MORE_TAP_V2) return;
  window.PASSKEY_MORE_TAP_V2 = true;

  function q(id){ return document.getElementById(id); }
  function closeMore(){
    ['mobileMoreFinalV2','mobileSheet'].forEach(function(id){
      var el=q(id); if(el) el.classList.remove('visible');
    });
    document.querySelectorAll('.mobile-sheet,.mobile-more').forEach(function(el){
      el.classList.remove('visible');
    });
  }
  function toast(text, ok){
    try{
      if(window.TronclassDashboard && window.TronclassDashboard.toast){
        window.TronclassDashboard.toast(text, ok!==false);
        return;
      }
    }catch(_){}
    var el=q('toast');
    if(el){
      el.textContent=text;
      el.className='toast visible '+(ok===false?'bad':'ok');
      el.style.zIndex='2147483647';
      clearTimeout(el._pkTap);
      el._pkTap=setTimeout(function(){ el.classList.remove('visible'); }, 3600);
      return;
    }
    try{ alert(text); }catch(_){}
  }
  function b64urlEncode(buf){
    var bytes=new Uint8Array(buf), s='';
    for(var i=0;i<bytes.length;i++) s+=String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'');
  }
  function b64urlDecode(str){
    str=String(str||'').replace(/-/g,'+').replace(/_/g,'/');
    while(str.length%4) str+='=';
    var bin=atob(str), out=new Uint8Array(bin.length);
    for(var i=0;i<bin.length;i++) out[i]=bin.charCodeAt(i);
    return out.buffer;
  }
  function prepareCreate(opts){
    opts=JSON.parse(JSON.stringify(opts||{}));
    opts.challenge=b64urlDecode(opts.challenge);
    if(opts.user&&opts.user.id) opts.user.id=b64urlDecode(opts.user.id);
    if(opts.excludeCredentials){
      opts.excludeCredentials=opts.excludeCredentials.map(function(c){
        return Object.assign({}, c, {id:b64urlDecode(c.id)});
      });
    }
    return opts;
  }
  function friendly(e){
    var m=String((e&&(e.message||e.name))||e||'');
    if(/NotAllowedError|not allowed|gesture|user verification/i.test(m)){
      return '系統沒有跳出驗證。請再按一次「設定通行密鑰」，並在手機上完成 Face ID / 密碼。';
    }
    if(/InvalidStateError|already registered|excludeCredentials/i.test(m)){
      return '這個裝置可能已經登錄過通行密鑰。可直接用通行密鑰登入，或換一台裝置再試。';
    }
    if(/unauthorized|401/i.test(m)) return '登入已過期，請重新登入後再設定通行密鑰。';
    if(/NotSupportedError|not supported/i.test(m)) return '此瀏覽器不支援通行密鑰。請用 Safari 開啟。';
    if(/abort|cancel|NotAllowed/i.test(m)) return '已取消設定通行密鑰。';
    return m || '設定通行密鑰失敗';
  }
  function ensureOverlay(){
    var wrap=q('passkeyTapOverlay');
    if(wrap) return wrap;
    wrap=document.createElement('div');
    wrap.id='passkeyTapOverlay';
    wrap.innerHTML='<div class="pk-card"><h3>設定通行密鑰</h3><p id="pkTapMsg">準備建立通行密鑰…</p><div class="pk-actions"><button id="pkTapClose" type="button">關閉</button><button id="pkTapGo" type="button">繼續設定</button></div></div>';
    document.body.appendChild(wrap);
    wrap.addEventListener('click', function(ev){ if(ev.target===wrap) hideOverlay(); });
    q('pkTapClose').onclick=hideOverlay;
    q('pkTapGo').onclick=function(){ startRegister(true); };
    return wrap;
  }
  function showOverlay(msg, showGo){
    var wrap=ensureOverlay();
    var p=q('pkTapMsg'); if(p) p.textContent=msg||'準備建立通行密鑰…';
    var go=q('pkTapGo'); if(go) go.style.display=showGo?'block':'none';
    wrap.classList.add('visible');
  }
  function hideOverlay(){
    var wrap=q('passkeyTapOverlay');
    if(wrap) wrap.classList.remove('visible');
  }
  async function startRegister(fromContinue){
    closeMore();
    if(!window.isSecureContext && location.hostname!=='localhost'){
      showOverlay('Passkey 需要 HTTPS，請用 https://dashboard.jasoncloudserver.me 開啟。', false);
      toast('Passkey 需要 HTTPS', false);
      return;
    }
    if(!window.PublicKeyCredential || !navigator.credentials || !navigator.credentials.create){
      showOverlay('此瀏覽器不支援通行密鑰。請用 iPhone 的 Safari 開啟。', false);
      toast('此瀏覽器不支援通行密鑰', false);
      return;
    }
    showOverlay(fromContinue?'請在系統視窗完成驗證…':'準備建立通行密鑰…', false);
    toast('準備建立通行密鑰…');
    try{
      var res=await fetch('/admin/api/passkey/register/start',{
        method:'POST',
        credentials:'same-origin',
        cache:'no-store',
        headers:{'content-type':'application/json'}
      });
      var json=await res.json().catch(function(){ return {}; });
      if(!res.ok) throw new Error(json.error||json.message||('HTTP '+res.status));
      if(!json.publicKey) throw new Error('伺服器沒有回傳通行密鑰參數');
      var cred=await navigator.credentials.create({publicKey:prepareCreate(json.publicKey)});
      if(!cred) throw new Error('未建立通行密鑰');
      var body={
        ceremony_id: json.ceremony_id,
        response:{
          id: cred.id,
          attestationObject: b64urlEncode(cred.response.attestationObject),
          clientDataJSON: b64urlEncode(cred.response.clientDataJSON),
          transports: cred.response.getTransports ? cred.response.getTransports() : []
        }
      };
      var fin=await fetch('/admin/api/passkey/register/finish',{
        method:'POST',
        credentials:'same-origin',
        cache:'no-store',
        headers:{'content-type':'application/json'},
        body: JSON.stringify(body)
      });
      var done=await fin.json().catch(function(){ return {}; });
      if(!fin.ok) throw new Error(done.error||done.message||('HTTP '+fin.status));
      hideOverlay();
      toast('通行密鑰已設定完成，下次可用 Passkey 登入。');
    }catch(e){
      var msg=friendly(e);
      var needRetry=/沒有跳出驗證|NotAllowed|gesture|user verification/i.test(String((e&&(e.name+' '+e.message))||msg));
      showOverlay(msg+(needRetry?'\n請按「繼續設定」，手機會再跳出 Face ID / 密碼。':''), needRetry);
      toast(msg, false);
    }
  }

  window.tronPasskeyRegister=function(){ startRegister(false); };

  function isPasskeyBtn(el){
    if(el&&el.nodeType===3) el=el.parentElement; if(el&&el.nodeType===3) el=el.parentElement; if(!el||!el.closest) return null;
    var b=el.closest('button,a,[data-more-action],#morePasskeyFinalV2,#mPasskey,#passkeyRegisterBtn,#mobilePasskeySheetBtn');
    if(!b) return null;
    var act=(b.getAttribute('data-more-action')||'');
    var id=b.id||'';
    var tx=(b.textContent||'').replace(/\s+/g,'').trim();
    if(act==='passkey') return b;
    if(/passkey/i.test(id)) return b;
    if(tx==='設定通行密鑰' || tx==='設定通行密碼' || tx==='通行密鑰') return b;
    return null;
  }

  // 越早綁越好，搶在舊腳本 stopImmediatePropagation 之前
  document.addEventListener('click', function(ev){
    var b=isPasskeyBtn(ev.target);
    if(!b) return;
    ev.preventDefault();
    if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
    ev.stopPropagation();
    startRegister(false);
  }, true);

  function bindDirect(){
    ['morePasskeyFinalV2','mPasskey','passkeyRegisterBtn','mobilePasskeySheetBtn'].forEach(function(id){
      var el=q(id);
      if(!el || el._pkTapBound) return;
      el._pkTapBound=true;
      el.addEventListener('click', function(ev){
        ev.preventDefault();
        if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        ev.stopPropagation();
        startRegister(false);
      }, true);
    });
    document.querySelectorAll('button,a').forEach(function(el){
      if(el._pkTapBound) return;
      var tx=(el.textContent||'').replace(/\s+/g,'').trim();
      if(tx!=='設定通行密鑰' && tx!=='設定通行密碼') return;
      el._pkTapBound=true;
      el.addEventListener('click', function(ev){
        ev.preventDefault();
        if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        ev.stopPropagation();
        startRegister(false);
      }, true);
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', bindDirect);
  else bindDirect();
  setTimeout(bindDirect, 80);
  setTimeout(bindDirect, 400);
  setTimeout(bindDirect, 1200);
})();

/* ── inline block: PASSWORD_MORE_TAP_V2 (原 dashboard.html:642) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="PASSWORD_MORE_TAP_V2">，瀏覽器會把有 id 的元素暴露成
     window.PASSWORD_MORE_TAP_V2（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.PASSWORD_MORE_TAP_V2) return;
  window.PASSWORD_MORE_TAP_V2 = true;

  function q(id){ return document.getElementById(id); }
  function closeMore(){
    ['mobileMoreFinalV2','mobileSheet'].forEach(function(id){
      var el=q(id); if(el) el.classList.remove('visible');
    });
    document.querySelectorAll('.mobile-sheet,.mobile-more').forEach(function(el){
      el.classList.remove('visible');
    });
  }
  function toast(text, ok){
    try{
      if(window.TronclassDashboard && window.TronclassDashboard.toast){
        window.TronclassDashboard.toast(text, ok!==false);
        return;
      }
    }catch(_){}
    var el=q('toast');
    if(el){
      el.textContent=text;
      el.className='toast visible '+(ok===false?'bad':'ok');
      el.style.zIndex='2147483647';
      clearTimeout(el._pwTap);
      el._pwTap=setTimeout(function(){ el.classList.remove('visible'); }, 3200);
      return;
    }
    try{ alert(text); }catch(_){}
  }
  function ensure(){
    var wrap=q('dashPwOverlay');
    if(wrap) return wrap;
    wrap=document.createElement('div');
    wrap.id='dashPwOverlay';
    wrap.innerHTML=
      '<div class="pw-card">'+
        '<h3>變更密碼</h3>'+
        '<p class="pw-hint">這是 Dashboard 專用密碼，不是 Tronclass 密碼。</p>'+
        '<label for="dashPwCurrent">目前 Dashboard 密碼</label>'+
        '<input id="dashPwCurrent" type="password" autocomplete="current-password">'+
        '<label for="dashPwNew">新密碼（至少 10 字）</label>'+
        '<input id="dashPwNew" type="password" autocomplete="new-password">'+
        '<label for="dashPwConfirm">再輸入一次新密碼</label>'+
        '<input id="dashPwConfirm" type="password" autocomplete="new-password">'+
        '<div id="dashPwMsg" class="pw-msg"></div>'+
        '<div class="pw-actions">'+
          '<button id="dashPwCancel" type="button">取消</button>'+
          '<button id="dashPwSave" type="button">儲存</button>'+
        '</div>'+
      '</div>';
    document.body.appendChild(wrap);
    wrap.addEventListener('click', function(ev){ if(ev.target===wrap) hide(); });
    q('dashPwCancel').onclick=hide;
    q('dashPwSave').onclick=submit;
    return wrap;
  }
  function setMsg(text, kind){
    var el=q('dashPwMsg');
    if(!el) return;
    el.textContent=text||'';
    el.className='pw-msg'+(kind?(' '+kind):'');
  }
  function show(){
    closeMore();
    var wrap=ensure();
    ['dashPwCurrent','dashPwNew','dashPwConfirm'].forEach(function(id){
      if(q(id)) q(id).value='';
    });
    setMsg('', '');
    wrap.classList.add('visible');
    wrap.style.display='flex';
    wrap.style.zIndex='2147483646';
    setTimeout(function(){ if(q('dashPwCurrent')) q('dashPwCurrent').focus(); }, 80);
  }
  function hide(){
    var wrap=q('dashPwOverlay');
    if(wrap){
      wrap.classList.remove('visible');
      wrap.style.display='none';
    }
  }
  async function submit(){
    var cur=(q('dashPwCurrent')&&q('dashPwCurrent').value)||'';
    var n=(q('dashPwNew')&&q('dashPwNew').value)||'';
    var c=(q('dashPwConfirm')&&q('dashPwConfirm').value)||'';
    if(!cur){ setMsg('請輸入目前密碼。', 'bad'); return; }
    if(!n||!c){ setMsg('請輸入並確認新密碼。', 'bad'); return; }
    if(n!==c){ setMsg('兩次輸入的新密碼不一致。', 'bad'); return; }
    if(n.length<10){ setMsg('新密碼至少 10 個字元。', 'bad'); return; }
    try{
      var res=await fetch('/admin/api/password',{
        method:'POST',
        credentials:'same-origin',
        cache:'no-store',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({current_password:cur, new_password:n})
      });
      var body=await res.json().catch(function(){ return {}; });
      if(!res.ok) throw new Error(body.error||body.message||('HTTP '+res.status));
      setMsg('密碼已更新。', 'ok');
      toast('密碼已更新');
      setTimeout(hide, 500);
    }catch(e){
      var m=String((e&&e.message)||e||'');
      if(/incorrect|current Dashboard password|unauthorized|401/i.test(m)){
        m='目前密碼不正確，或登入已過期。';
      }
      setMsg(m||'變更密碼失敗', 'bad');
    }
  }

  window.tronOpenChangePassword=show;

  function isPwBtn(el){
    if(el&&el.nodeType===3) el=el.parentElement; if(el&&el.nodeType===3) el=el.parentElement; if(!el||!el.closest) return null;
    var b=el.closest('button,a,[data-more-action],#moreChangePasswordFinalV2,#mChangePassword,#changePasswordBtn');
    if(!b) return null;
    var act=(b.getAttribute('data-more-action')||'');
    var id=b.id||'';
    var tx=(b.textContent||'').replace(/\s+/g,'').trim();
    if(act==='password') return b;
    if(/ChangePassword|changePassword|change-password/i.test(id)) return b;
    if(tx==='變更密碼' || tx==='修改密碼' || tx==='更改密碼') return b;
    return null;
  }

  document.addEventListener('click', function(ev){
    var b=isPwBtn(ev.target);
    if(!b) return;
    ev.preventDefault();
    if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
    ev.stopPropagation();
    // iOS：先關更多，下一幀再畫視窗，避免被蓋住或直接吃掉
    closeMore();
    setTimeout(show, 40);
  }, true);

  function bindDirect(){
    ['moreChangePasswordFinalV2','mChangePassword','changePasswordBtn'].forEach(function(id){
      var el=q(id);
      if(!el || el._pwTapBound) return;
      el._pwTapBound=true;
      el.addEventListener('click', function(ev){
        ev.preventDefault();
        if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        ev.stopPropagation();
        closeMore();
        setTimeout(show, 40);
      }, true);
    });
    document.querySelectorAll('button,a').forEach(function(el){
      if(el._pwTapBound) return;
      var tx=(el.textContent||'').replace(/\s+/g,'').trim();
      if(tx!=='變更密碼' && tx!=='修改密碼' && tx!=='更改密碼') return;
      el._pwTapBound=true;
      el.addEventListener('click', function(ev){
        ev.preventDefault();
        if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        ev.stopPropagation();
        closeMore();
        setTimeout(show, 40);
      }, true);
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', bindDirect);
  else bindDirect();
  setTimeout(bindDirect, 80);
  setTimeout(bindDirect, 400);
  setTimeout(bindDirect, 1200);
})();

/* ── inline block: AUTH_STATE_FROM_SERVER_V1 (原 dashboard.html:842) ── */
(function(){
  // AUTH_STATE_FROM_SERVER_V1：驗證狀態由伺服器注入在 <html data-auth data-account data-admin data-viewer>
  // （pages::inject_auth_state）。這裡只把它轉成既有腳本依賴的全域旗標／事件，不再打
  // /admin/api/status 做「開機驗證」，也沒有任何 localStorage 提示旗標或 referrer 推測。
  if(window.AUTH_STATE_FROM_SERVER_V1) return;
  window.AUTH_STATE_FROM_SERVER_V1 = true;
  var html = document.documentElement;
  function setState(s){
    html.classList.remove('tron-auth-pending','tron-auth-ok','tron-auth-logged-out');
    if(s==='ok'){
      html.classList.add('tron-auth-ok','dash-auth-ok');
      if(document.body) document.body.classList.add('logged-in','auth-ok','dash-auth-ok');
    }else if(s==='out'){
      html.classList.add('tron-auth-logged-out');
      if(document.body) document.body.classList.remove('logged-in','auth-ok','dash-auth-ok');
    }else{
      html.classList.add('tron-auth-pending');
    }
    window.__tronAuthState = s;
    try{ window.dispatchEvent(new CustomEvent('tronclass:auth-state',{detail:s})); }catch(_){}
  }
  window.__tronAuthSetState = setState;
  var injected = html.getAttribute('data-auth') === 'ok';
  if(injected){
    // 伺服器已驗證：第一幀就是已登入版面。狀態摘要（is_admin / current_account_id）給
    // dashboard-permissions.js 與 onboarding 先用，完整 status 由主腳本 loadStatus() 補齊。
    window.__tronAuthStatus = {
      current_account_id: html.getAttribute('data-account') || '',
      is_admin: html.getAttribute('data-admin') === '1',
      is_viewer: html.getAttribute('data-viewer') === '1',
      __partial: true
    };
    setState('ok');
    window.__tronAuthBoot = Promise.resolve(window.__tronAuthStatus);
    // body 的 logged-in / auth-ok / dash-auth-ok class 已直接寫在 markup 上，不需二次派發事件。
  }else{
    // 不應發生（伺服器沒注入卻回了 Dashboard）：保守地當作未登入，交給 session-security.js 導回。
    setState('out');
    window.__tronAuthBoot = Promise.reject(Object.assign(new Error('unauthorized'),{status:401}));
  }
})();

/* ── inline block: HTTPONLY_SESSION_V2 (原 dashboard.html:885) ── */
(function(){
  /* HTTPONLY_SESSION_V2：只保留「清除遺留 token 儲存」與「fetch 預設帶 cookie」。
     登出改由 session-security.js 的 secureLogout 走 POST /admin/logout（LOGOUT_SERVER_SIDE_V1）。 */
  var KEYS=['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token','tronclass_session_hint','tronclass_dashboard_manual_logout'];
  function wipe(){ KEYS.forEach(function(k){ try{ localStorage.removeItem(k); sessionStorage.removeItem(k);}catch(_){}}); }
  wipe();
  try{ var u=new URL(location.href); var changed=false; ['token','logout','login','v'].forEach(function(k){ if(u.searchParams.has(k)){ u.searchParams.delete(k); changed=true; } }); if(changed) history.replaceState(null,'',u.pathname+u.search+u.hash);}catch(_){}
  /* RUNTIME_PATCH_CONSOLIDATE_V1：fetch 預設 credentials 已併入 dashboardPerformanceGuardV2 的唯一 fetch 包裝。 */
})();

/* ── inline block: dashboardTimetableStandaloneRedirectMobileFixV1 (原 dashboard.html:982) ── */
(function(){'use strict';if(window.DASHBOARD_TIMETABLE_STANDALONE_REDIRECT_MOBILE_FIX_V1)return;window.DASHBOARD_TIMETABLE_STANDALONE_REDIRECT_MOBILE_FIX_V1=true;function tok(){return''}function go(edit){var u=new URL('/admin/timetable',location.origin); if(edit)u.searchParams.set('edit','1'); location.href=u.pathname+u.search;}if((location.pathname==='/admin'||location.pathname==='/admin/'||location.pathname==='/admin/dashboard')&&location.hash==='#timetable'){setTimeout(function(){go(false)},80)}document.addEventListener('click',function(ev){var n=ev.target&&ev.target.closest&&ev.target.closest('[data-view="timetable"],[data-switch="timetable"],a[href$="#timetable"],button,a,[role="button"]');if(!n)return;var txt=(n.textContent||'').replace(/\s+/g,'').trim();if((n.dataset&&(n.dataset.view==='timetable'||n.dataset.switch==='timetable'))||txt==='課表'||txt.indexOf('編輯課表')>=0){ev.preventDefault();ev.stopPropagation();if(ev.stopImmediatePropagation)ev.stopImmediatePropagation();go(txt.indexOf('編輯課表')>=0)}},true);})();

/* ── inline block: hideAdminTokenFromUrlEarlyV2 (原 dashboard.html:997) ── */
(function(){
  'use strict';
  /* TOKEN_URL_SCRUB_V2：token 只存在 HttpOnly cookie；這裡僅保留舊模組呼叫的 window.tronAdminToken 相容介面（永遠空字串）。 */
  window.tronAdminToken = function(){ return ''; };
})();

/* ── inline block: dashboardNullHrefFinalV2 (原 dashboard.html:1017) ── */
(function(){
  'use strict';
  if (window.DASHBOARD_NULL_HREF_FINAL_V2) return;
  window.DASHBOARD_NULL_HREF_FINAL_V2 = true;

  var knownLinkIds = new Set([
    'meLink','dashboardMeLink','mMe','mobileMeLink','mobileMeForce','mobileFinalMe','mobileStableMe',
    'filesLink','openMaterialsDrive','mFiles','mobileFilesLink','mobileFinalDrive','mobileStableDrive','moreDriveFinalV2',
    'activityLink','activityShortcut','activityMaintenance','openActivityFiles','mActivity','mobileActivityLink','mobileActivityForce','mobileFinalActivity','mobileStableActivity',
    'logsLink','openLogsShortcut','openLogsPage','maintenanceLogsLink','mobileLogsLink','mLogs','mobileLogsForce','mobileFinalLogs','mobileStableLogs',
    'dashboardLink','adminLink','linkAdmin','classicLink','navFiles','navHealth','navLogs','navMock','healthLink','mockLink',
    'previewDownload','downloadLink','materialsLink','controlLink','mobileTopDashboard','mobileDriveTopControl'
  ]);

  var originalGetElementById = Document.prototype.getElementById;

  function hostForPlaceholder(){
    return document.body || document.documentElement;
  }

  function makePlaceholder(id){
    var host = hostForPlaceholder();
    if (!host) return null;
    var a = document.createElement('a');
    a.id = id;
    a.href = '#';
    a.className = 'dash-nullhref-placeholder';
    a.setAttribute('aria-hidden', 'true');
    a.tabIndex = -1;
    a.style.display = 'none';
    a.style.visibility = 'hidden';
    a.style.pointerEvents = 'none';
    host.appendChild(a);
    return a;
  }

  Document.prototype.getElementById = function(id){
    var el = originalGetElementById.call(this, id);
    if (el) return el;
    if (knownLinkIds.has(String(id))) {
      return makePlaceholder(String(id));
    }
    return null;
  };

  window.__dashSafeHrefFinal = function(id, href){
    try {
      var el = document.getElementById(id);
      if (el) el.href = href;
    } catch(e) {
      console.warn('safe href skipped', id, e);
    }
  };

  function cleanupDuplicatePlaceholders(){
    try {
      document.querySelectorAll('.dash-nullhref-placeholder').forEach(function(ph){
        var id = ph.id;
        var all = Array.prototype.slice.call(document.querySelectorAll('[id="' + CSS.escape(id) + '"]'));
        // If a real element with the same id exists, remove placeholder.
        var real = all.find(function(el){ return el !== ph && !el.classList.contains('dash-nullhref-placeholder'); });
        if (real) ph.remove();
      });
    } catch(_) {}
  }

  window.addEventListener('error', function(ev){
    var msg = String(ev.message || '');
    if (msg.indexOf("Cannot set properties of null (setting 'href')") >= 0 || msg.indexOf('Cannot set properties of null (setting "href")') >= 0) {
      console.warn('Dashboard null href was caught. A placeholder should prevent future failures. Please refresh once if the page stayed blank.');
      setTimeout(cleanupDuplicatePlaceholders, 50);
    }
  }, true);

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', cleanupDuplicatePlaceholders);
  else cleanupDuplicatePlaceholders();
})();

/* ── inline block: optimizeV47Theme (原 dashboard.html:1396) ── */
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
})();

/* ── inline block: MORE_PASSKEY_PASSWORD_FORCE_V1 (原 dashboard.html:1579) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="MORE_PASSKEY_PASSWORD_FORCE_V1">，瀏覽器會把有 id 的元素暴露成
     window.MORE_PASSKEY_PASSWORD_FORCE_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.MORE_PASSKEY_PASSWORD_FORCE_V1) return;
  window.MORE_PASSKEY_PASSWORD_FORCE_V1 = true;
  function q(id){ return document.getElementById(id); }
  function closeMore(){
    ['mobileMoreFinalV2','mobileSheet'].forEach(function(id){
      var el=q(id); if(el) el.classList.remove('visible');
    });
    document.querySelectorAll('.mobile-sheet,.mobile-more').forEach(function(el){ el.classList.remove('visible'); });
  }
  function toast(text, ok){
    if(window.TronclassDashboard && window.TronclassDashboard.toast) return window.TronclassDashboard.toast(text, ok!==false);
    var el=q('toast'); if(!el){ alert(text); return; }
    el.textContent=text; el.className='toast visible '+(ok===false?'bad':'ok');
    clearTimeout(el._morePk); el._morePk=setTimeout(function(){ el.classList.remove('visible'); }, 2800);
  }
  function openPasskey(){
    if(typeof window.tronPasskeyRegister==='function'){ window.tronPasskeyRegister(); return; }
    closeMore();
    toast('通行密鑰功能尚未就緒，請重整後再試。', false);
  }
  function openPassword(){
    closeMore();
    if(typeof window.tronOpenChangePassword === 'function'){ window.tronOpenChangePassword(); return; }
    toast('變更密碼視窗尚未就緒，請再按一次。', false);
  }
  function put(grid, id, label, action){
    if(!grid) return;
    var b = grid.querySelector('#'+id);
    if(!b){
      b=document.createElement('button');
      b.id=id; b.type='button'; b.className='secondary';
      var logout=grid.querySelector('.danger,#moreLogoutFinalV2,#mLogout');
      grid.insertBefore(b, logout||null);
    }
    b.textContent=label;
    b.setAttribute('data-more-action', action);
    b.style.display='';
  }
  function enhance(){
    // MORE_SHEET_SETTINGS_V1: 不再補通行密鑰/變更密碼按鈕，改為確保「設定」存在。
    document.querySelectorAll('#mobileMoreFinalV2 .more-grid,.more-grid').forEach(function(grid){
      if(document.getElementById('moreSettingsFinalV2')) return;
      var a=document.createElement('a');
      a.id='moreSettingsFinalV2'; a.className='button secondary'; a.href='/admin/settings';
      a.textContent='設定';
      grid.insertBefore(a, grid.firstChild);
    });
  }
  document.addEventListener('click', function(ev){
    var _el=ev.target; if(_el&&_el.nodeType===3) _el=_el.parentElement; var b=_el&&_el.closest&&_el.closest('[data-more-action],#morePasskeyFinalV2,#moreChangePasswordFinalV2,#mPasskey,#mChangePassword');
    if(!b) return;
    var act=b.getAttribute('data-more-action')||'';
    var id=b.id||'';
    if(act==='passkey' || /Passkey|passkey/.test(id)){ ev.preventDefault(); openPasskey(); }
    else if(act==='password' || /Password|password/.test(id)){ ev.preventDefault(); openPassword(); }
  }, true);
  function start(){
    enhance();
    /* NO_POLL_ENSURE_V1：原本每 500 ms 重跑 enhance() 共 120 次（60 秒）。MutationObserver 已能
       在「更多」面板被建立／改動時即時補強，輪詢只是浪費主執行緒；改為只在 pageshow／resize
       再補一次（bfcache 回來、旋轉螢幕時 DOM 不一定有 mutation）。 */
    try{ new MutationObserver(enhance).observe(document.documentElement,{childList:true,subtree:true}); }catch(_){}
    window.addEventListener('pageshow', enhance);
    window.addEventListener('resize', enhance, {passive:true});
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
