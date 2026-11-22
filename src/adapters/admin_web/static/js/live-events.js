/* DASHBOARD_LIVE_EVENTS_V1：全站共用的即時事件客戶端（由 inject_theme 注入每個 admin 頁）。
 *
 * 連到 /admin/api/events（SSE），做兩件事：
 * 1. 自動刷新資料：收到 `status` 事件時，若頁面提供 window.TronclassDashboard.loadStatus
 *    就呼叫它（Dashboard），同時廣播 DOM 事件 `tronclass:live-status` 給其他頁自行決定要不要重抓。
 * 2. 自動重載版本：`hello` 事件帶前端 build id；重連後若與第一次不同（代表已重新部署），
 *    在「安全時機」自動 location.reload()（不打斷輸入中的表單、不在隱藏分頁重載、同版本 60 秒內只重載一次）。
 *
 * 未登入時 /admin/api/events 回 401，瀏覽器的 EventSource 會直接放棄（不會重連），
 * 這裡再自行以退避重試；登入成功時（tronclass:auth-state=ok）立即重連。
 */
(function(){'use strict';
  if(window.DASHBOARD_LIVE_EVENTS_V1)return;window.DASHBOARD_LIVE_EVENTS_V1=true;
  if(!('EventSource' in window))return;
  if(location.pathname.indexOf('/admin')!==0)return;
  // 首次登入強制改密碼頁：bootstrap session 對 /admin/api/events 一律 403，不必連
  if(location.pathname.indexOf('/admin/password')===0)return;

  var source=null,firstVersion=null,retryDelay=5000,retryTimer=null,statusTimer=null,pendingReload=false,idleRetries=0;
  var RELOAD_KEY='tronclass_live_reload_v1';

  function nowSec(){return Math.floor(Date.now()/1000)}
  function markLive(kind){window.__tronLiveLastEventAt=Date.now();window.__tronLiveKind=kind||'';}

  function loggedInHint(){
    if(window.__tronAuthState==='ok')return true;
    if(document.body&&document.body.classList.contains('logged-in'))return true;
    // 非 /admin 登入殼的頁面只有登入後才會被伺服器送出
    return location.pathname!=='/admin'&&location.pathname!=='/admin/';
  }

  function safeToReload(){
    if(document.hidden)return false;
    var ae=document.activeElement;
    if(ae&&(ae.tagName==='INPUT'||ae.tagName==='TEXTAREA'||ae.tagName==='SELECT'||ae.isContentEditable))return false;
    if(document.querySelector('.modal.visible,.sheet-backdrop.open,dialog[open],[aria-modal="true"].visible'))return false;
    return true;
  }
  function tryReload(version){
    try{
      var last=JSON.parse(sessionStorage.getItem(RELOAD_KEY)||'null');
      if(last&&last.version===version&&(Date.now()-last.at)<60000)return; // 防止反覆重載
    }catch(_){}
    if(!safeToReload()){pendingReload=version;return}
    try{sessionStorage.setItem(RELOAD_KEY,JSON.stringify({version:version,at:Date.now()}))}catch(_){}
    location.reload();
  }
  document.addEventListener('visibilitychange',function(){if(pendingReload&&!document.hidden&&safeToReload()){var v=pendingReload;pendingReload=false;tryReload(v)}});
  document.addEventListener('focusout',function(){if(pendingReload)setTimeout(function(){if(pendingReload&&safeToReload()){var v=pendingReload;pendingReload=false;tryReload(v)}},800)});

  function onStatus(detail){
    markLive('status');
    try{window.dispatchEvent(new CustomEvent('tronclass:live-status',{detail:detail}))}catch(_){}
    var dash=window.TronclassDashboard;
    if(!dash||typeof dash.loadStatus!=='function')return;
    if(document.hidden)return;
    clearTimeout(statusTimer);
    statusTimer=setTimeout(function(){try{var p=dash.loadStatus();if(p&&p.catch)p.catch(function(){})}catch(_){}},400);
  }

  function scheduleRetry(){
    clearTimeout(retryTimer);
    retryTimer=setTimeout(connect,retryDelay);
    retryDelay=Math.min(retryDelay*2,120000);
  }

  function connect(){
    if(source){try{source.close()}catch(_){}source=null}
    // 未登入（登入殼頁）：最多重試 3 次就停，之後靠 tronclass:auth-state=ok 事件喚醒
    if(!loggedInHint()){if(idleRetries++<3)scheduleRetry();return}
    idleRetries=0;
    var es;
    try{es=new EventSource('/admin/api/events')}catch(_){scheduleRetry();return}
    source=es;window.__tronLiveEventSource=es;
    es.onopen=function(){retryDelay=5000;markLive('open')};
    es.onerror=function(){
      markLive('error');
      // readyState CLOSED = 瀏覽器已放棄（例如 401）；CONNECTING = 它自己會重連
      if(es.readyState===EventSource.CLOSED){source=null;scheduleRetry()}
    };
    es.addEventListener('hello',function(ev){
      var d={};try{d=JSON.parse(ev.data||'{}')}catch(_){}
      markLive('hello');
      var v=String(d.version||'');
      if(!v)return;
      if(firstVersion===null){firstVersion=v;window.__tronBuildId=v;return}
      if(v!==firstVersion){tryReload(v)}
    });
    es.addEventListener('status',function(ev){var d={};try{d=JSON.parse(ev.data||'{}')}catch(_){}onStatus(d)});
    es.addEventListener('ping',function(){markLive('ping')});
  }

  window.addEventListener('tronclass:auth-state',function(ev){
    if(ev&&ev.detail==='ok'){retryDelay=5000;idleRetries=0;clearTimeout(retryTimer);if(!source)connect()}
    else if(ev&&ev.detail==='out'&&source){try{source.close()}catch(_){}source=null}
  });
  window.addEventListener('pagehide',function(){if(source){try{source.close()}catch(_){}}});

  /* LIVE_RESUME_RECONNECT_V1：iOS 主畫面 App／手機瀏覽器回到前景時，被系統凍結過的 EventSource
     常常已經斷了但 readyState 仍是 OPEN（不會觸發 onerror）。Dashboard 的輪詢看到
     connected() 為 true 就退到每 60 秒一次 → 畫面回來了但資料不更新。
     回前景時：距離上一個事件超過 STALE_MS（伺服器每 15 秒至少送一個 ping／keepalive）
     就視為殭屍連線，直接重連並廣播一次 status 讓 Dashboard 立刻補抓。 */
  var STALE_MS=45000;
  function resumeCheck(){
    if(document.hidden)return;
    if(!loggedInHint())return;
    var last=window.__tronLiveLastEventAt||0;
    var stale=!source||source.readyState===EventSource.CLOSED||(Date.now()-last)>STALE_MS;
    if(!stale)return;
    retryDelay=5000;clearTimeout(retryTimer);
    connect();
    onStatus({reason:'resume'});
  }
  document.addEventListener('visibilitychange',function(){if(!document.hidden)resumeCheck()},{passive:true});
  window.addEventListener('pageshow',function(ev){if(ev&&ev.persisted)resumeCheck()},{passive:true});

  window.TronclassLiveEvents={
    connected:function(){return !!(source&&source.readyState===EventSource.OPEN)},
    lastEventAt:function(){return window.__tronLiveLastEventAt||0},
    reconnect:connect
  };

  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',function(){setTimeout(connect,300)});
  else setTimeout(connect,300);
})();
