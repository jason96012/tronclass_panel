/* TRONCLASS_CORE_API_V2_STATUS_DEDUPE */
(function(){
  'use strict';
  if(window.TronclassRuntime && window.TronclassRuntime.__statusDedupeV2) return;

  var STATUS_TTL_MS = 2000;
  var HIDDEN_STATUS_TTL_MS = 60000;
  var statusCacheByKey = Object.create(null);
  var statusRequestByKey = Object.create(null);

  function token(){
    return '';
  }

  function withToken(path){
    var u = new URL(path, location.origin);
    try{ u.searchParams.delete('token'); }catch(_){}
    return u.pathname + u.search + u.hash;
  }

  async function parseResponse(res){
    var b = await res.json().catch(function(){ return {}; });
    if(!res.ok){
      var e = new Error(b.error || b.message || ('HTTP ' + res.status));
      e.status = res.status;
      throw e;
    }
    return b;
  }

  function statusPath(options){
    options = options || {};
    var u = new URL('/admin/api/status', location.origin);
    var includeHistory = options.include_history;
    if(includeHistory == null) includeHistory = options.includeHistory;
    var historyLimit = options.history_limit;
    if(historyLimit == null) historyLimit = options.historyLimit;
    if(includeHistory != null) u.searchParams.set('include_history', includeHistory ? 'true' : 'false');
    if(historyLimit != null) u.searchParams.set('history_limit', String(historyLimit));
    try{ u.searchParams.sort(); }catch(_){}
    return u.pathname + u.search;
  }

  // Single in-flight refresh guard so concurrent 401s trigger one refresh call.
  var refreshPromise = null;

  function refreshSession(){
    if(!refreshPromise){
      refreshPromise = fetch('/admin/api/session/refresh', {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: { 'Accept': 'application/json' }
      }).then(function(res){
        if(res.status === 401 || res.status === 403) throw new Error('refresh_failed');
        if(!res.ok) throw new Error('refresh_failed');
        return res.json().catch(function(){ return {}; });
      }).finally(function(){ refreshPromise = null; });
    }
    return refreshPromise;
  }

  async function api(path, opts){
    opts = opts || {};
    opts.credentials = opts.credentials || 'same-origin';
    opts.cache = opts.cache || 'no-store';
    var res = await fetch(withToken(path), opts);
    // Access token expired (short-lived): try once to refresh, then retry once.
    if(res.status === 401 && !opts._refreshed){
      try{
        await refreshSession();
      }catch(_){
        // Refresh failed (no valid refresh cookie) — surface the original 401.
        return parseResponse(res);
      }
      var retryOpts = Object.assign({}, opts, { _refreshed: true });
      res = await fetch(withToken(path), retryOpts);
    }
    return parseResponse(res);
  }

  function clearStatusCache(){
    statusCacheByKey = Object.create(null);
    statusRequestByKey = Object.create(null);
    try{ window.dispatchEvent(new Event('tronclass:runtime:clear-status-cache')); }catch(_){}
  }

  function status(force, options){
    if(force && typeof force === 'object'){
      options = force;
      force = false;
    }
    options = options || {};
    var path = statusPath(options);
    var cached = statusCacheByKey[path];
    var ttl = document.hidden ? HIDDEN_STATUS_TTL_MS : STATUS_TTL_MS;
    if(!force && cached && (Date.now() - cached.at) < ttl) return Promise.resolve(cached.value);
    if(statusRequestByKey[path]) return statusRequestByKey[path];
    statusRequestByKey[path] = api(path, {cache:'no-store'}).then(function(value){
      statusCacheByKey[path] = {value:value, at:Date.now()};
      delete statusRequestByKey[path];
      return value;
    }, function(error){
      delete statusRequestByKey[path];
      throw error;
    });
    return statusRequestByKey[path];
  }

  function statusSummary(force){
    return status(!!force, {include_history:false, history_limit:0});
  }

  window.TronclassRuntime = {
    __statusDedupeV2: true,
    token: token,
    withToken: withToken,
    parseResponse: parseResponse,
    api: api,
    status: status,
    statusSummary: statusSummary,
    refreshStatus: function(options){ return status(true, options || {}); },
    clearStatusCache: clearStatusCache,
    // REFRESH_TOKEN_V1: expose the silent refresh for session-security.js to
    // attempt before declaring the session expired.
    tryRefresh: refreshSession,
    __tryRefresh: refreshSession
  };
  window.__tronTryRefresh = refreshSession;

  document.addEventListener('visibilitychange', function(){
    if(!document.hidden) clearStatusCache();
  }, {passive:true});
})();
