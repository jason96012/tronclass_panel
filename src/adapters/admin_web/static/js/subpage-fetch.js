/* SUBPAGE_FETCH_SHARED_V1
 *
 * 子頁（health / timetable / activity / files / logs）共用的唯一一層 window.fetch 包裝。
 * 取代原本各頁內聯、彼此疊加的 1～3 層包裝（SUBPAGE_TOKEN_BOOT_V1、
 * consolidatedAdminAuthBodyV1、hideAdminTokenFromUrlBodyV1、ttStatusFetchDedupeV1、
 * optimizeV47TTAuth）。行為與舊版逐條對應：
 *
 *   1. 同源 /admin/api/* 請求一律 credentials:'same-origin'（HttpOnly cookie 認證）。
 *   2. URL 上殘留的 ?token= 一律剝掉（TOKEN_STORAGE_PURGE_V1：cookie-only）。
 *   3. 401 → 先打一次 /admin/api/session/refresh，成功就用同樣參數重試一次
 *      （與 dashboard 的 core-api.js 相同；login / passkey / refresh 端點本身不重試）。
 *   4. refresh 也失敗 → 清掉本機 session marker 並導回登入頁（原 optimizeV47TTAuth）。
 *      只有 GET /admin/api/status 這類「頁面認證探測」才導頁；其餘 API 把 401 原樣回給呼叫端
 *      （避免一個背景請求把整頁踢出去）。
 *   5. 可選的狀態去重：<html data-fetch-dedupe="1"> 或 window.__TRON_FETCH_DEDUPE__=true 時，
 *      GET /admin/api/status 與 /admin/api/timetable 同 key 在 20 秒（背景 60 秒）內共用同一個
 *      回應（Response.clone），對 timetable / action 的寫入會清快取（原 ttStatusFetchDedupeV1）。
 *
 * 此檔必須在 <head> 內「同步」載入且早於任何 inline script（頁面的 api() 會在定義時
 * 綁走 window.fetch）。dashboard.html 不用這支（它有自己的 RUNTIME_PATCH_CONSOLIDATE_V1）。
 */
(function () {
  'use strict';
  if (window.SUBPAGE_FETCH_SHARED_V1) return;
  window.SUBPAGE_FETCH_SHARED_V1 = true;

  var nativeFetch = window.fetch.bind(window);
  var SESSION_KEY = 'tronclass_admin_session';
  var ACTIVE_TTL = 20000;
  var HIDDEN_TTL = 60000;
  var cacheByKey = Object.create(null);
  var inflightByKey = Object.create(null);
  var refreshPromise = null;

  function dedupeEnabled() {
    if (window.__TRON_FETCH_DEDUPE__ === true) return true;
    try { return document.documentElement.getAttribute('data-fetch-dedupe') === '1'; } catch (_) { return false; }
  }
  function urlOf(input) {
    try {
      if (typeof input === 'string') return new URL(input, location.origin);
      if (input && typeof input.url === 'string') return new URL(input.url, location.origin);
    } catch (_) {}
    return null;
  }
  function methodOf(input, init) {
    return String((init && init.method) || (input && input.method) || 'GET').toUpperCase();
  }
  function isAdminApi(url) {
    return !!url && url.origin === location.origin && url.pathname.indexOf('/admin/api/') === 0;
  }
  function noRetryPath(path) {
    return path === '/admin/api/login'
      || path === '/admin/api/session/refresh'
      || path.indexOf('/admin/api/passkey/login/') === 0;
  }
  function coalescable(url) {
    return url.pathname === '/admin/api/status' || url.pathname === '/admin/api/timetable';
  }
  function writes(url, method) {
    return method !== 'GET' && (
      url.pathname.indexOf('/admin/api/timetable') === 0 ||
      url.pathname === '/admin/api/action'
    );
  }
  function keyOf(url) {
    var c = new URL(url.href);
    c.searchParams.delete('token');
    if (c.pathname === '/admin/api/status') {
      var noHist = c.searchParams.get('include_history') === 'false'
        || c.searchParams.get('history_limit') === '0';
      c.searchParams.delete('history_limit');
      c.searchParams.delete('include_history');
      if (noHist) c.searchParams.set('include_history', 'false');
    }
    try { c.searchParams.sort(); } catch (_) {}
    return c.pathname + c.search;
  }
  function clearCache() {
    cacheByKey = Object.create(null);
    inflightByKey = Object.create(null);
  }
  function refreshSession() {
    if (!refreshPromise) {
      refreshPromise = nativeFetch('/admin/api/session/refresh', {
        method: 'POST', credentials: 'same-origin', cache: 'no-store',
        headers: { 'Accept': 'application/json' }
      }).then(function (res) {
        if (!res.ok) throw new Error('refresh_failed');
        return true;
      }).finally(function () { refreshPromise = null; });
    }
    return refreshPromise;
  }
  function kickToLogin() {
    try { localStorage.removeItem(SESSION_KEY); sessionStorage.removeItem(SESSION_KEY); } catch (_) {}
    var u = new URL('/admin', location.origin);
    var cur = location.pathname + location.search + location.hash;
    if (cur && cur !== '/admin' && cur !== '/admin/') u.searchParams.set('next', cur);
    u.searchParams.set('v', 'auth' + Date.now());
    location.replace(u.pathname + u.search);
  }

  function strippedInput(input, url) {
    if (!url.searchParams.has('token')) return input;
    url.searchParams.delete('token');
    var clean = url.pathname + url.search + url.hash;
    if (typeof input === 'string') return clean;
    try { return new Request(clean, input); } catch (_) { return clean; }
  }

  function send(input, init, url, method) {
    return nativeFetch(input, init).then(function (res) {
      if (res.status !== 401 || noRetryPath(url.pathname) || (init && init.__tronRetried)) return res;
      return refreshSession().then(function () {
        var retry = Object.assign({}, init || {}, { __tronRetried: true });
        return nativeFetch(input, retry);
      }, function () {
        // refresh 失敗：只有頁面級的認證探測才把人送回登入頁。
        if (method === 'GET' && url.pathname === '/admin/api/status') kickToLogin();
        return res;
      });
    });
  }

  window.fetch = function (input, init) {
    var url = urlOf(input);
    if (!isAdminApi(url)) return nativeFetch(input, init);
    init = init || {};
    if (!init.credentials) init.credentials = 'same-origin';
    input = strippedInput(input, url);
    var method = methodOf(input, init);

    if (!dedupeEnabled()) return send(input, init, url, method);

    if (writes(url, method)) clearCache();
    if (method !== 'GET' || !coalescable(url)) return send(input, init, url, method);

    var key = keyOf(url);
    var hit = cacheByKey[key];
    var ttl = document.hidden ? HIDDEN_TTL : ACTIVE_TTL;
    if (hit && (Date.now() - hit.at) < ttl) return Promise.resolve(hit.res.clone());
    if (inflightByKey[key]) return inflightByKey[key].then(function (r) { return r.clone(); });

    inflightByKey[key] = send(input, init, url, method).then(function (res) {
      delete inflightByKey[key];
      if (res && res.ok) cacheByKey[key] = { res: res, at: Date.now() };
      return res;
    }, function (err) {
      delete inflightByKey[key];
      throw err;
    });
    return inflightByKey[key].then(function (r) { return r.clone(); });
  };

  // 舊頁面透過這些名字溝通；保留以免既有腳本判斷「已安裝」失敗後又自己再包一層。
  window.SUBPAGE_TOKEN_BOOT_V1 = true;
  window.TT_STATUS_FETCH_DEDUPE_V1 = true;
  window.TronclassSubpageFetch = {
    native: nativeFetch,
    clearCache: clearCache,
    tryRefresh: refreshSession
  };
  if (!window.__tronTryRefresh) window.__tronTryRefresh = refreshSession;

  document.addEventListener('visibilitychange', function () {
    if (!document.hidden) clearCache();
  }, { passive: true });
})();
