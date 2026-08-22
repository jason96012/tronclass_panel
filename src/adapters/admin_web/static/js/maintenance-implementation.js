/* MAINTENANCE_IMPLEMENTATION_PHASE4A5 */
(function(){
  'use strict';
  if (window.DASHBOARD_DESKTOP_NAV_MAINTENANCE_FIX_V1) return;
  window.DASHBOARD_DESKTOP_NAV_MAINTENANCE_FIX_V1 = true;

  var LS_SESSION = 'tronclass_admin_session';
  var lastHealth = null;
  var lastCheck = null;

  function q(id){ return document.getElementById(id); }
  function isMobile(){ return window.matchMedia && matchMedia('(max-width:820px)').matches; }
  function token(){ try { return (new URLSearchParams(location.search).get('token') || localStorage.getItem(LS_SESSION) || '').trim(); } catch(_) { return ''; } }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g, function(ch){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]; }); }
  function withToken(path){ var t=token(); var u=new URL(path, location.origin); if(t)u.searchParams.set('token',t); return u.pathname+u.search+u.hash; }
  function toast(text, ok){ var el=q('toast'); if(el){el.textContent=text;el.className='toast visible '+(ok===false?'bad':'ok');clearTimeout(el._timer);el._timer=setTimeout(function(){el.classList.remove('visible')},3200)} else console.log(text); }
  async function parseResponse(res){ var b=await res.json().catch(function(){return{}}); if(!res.ok){var e=new Error(b.error||('HTTP '+res.status)); e.status=res.status; throw e;} return b; }
  // MAINTENANCE_CORE_API_PHASE4F3
  async function api(path){ if(window.TronclassRuntime&&window.TronclassRuntime.api)return window.TronclassRuntime.api(path,{cache:'no-store'}); var sep=path.indexOf('?')>=0?'&':'?'; return fetch(path+sep+'token='+encodeURIComponent(token()),{cache:'no-store'}).then(parseResponse); }
  function pct(used,total){ used=Number(used||0); total=Number(total||0); return total>0?Math.max(0,Math.min(100,used/total*100)):0; }
  function levelByPct(p){ return p>=90?'bad':(p>=75?'warn':'ok'); }
  function parseDisk(out){ var lines=String(out||'').trim().split(/\n/); if(lines.length<2)return null; var c=lines[1].trim().split(/\s+/); return {size:c[1],used:c[2],avail:c[3],use:c[4],mount:c[5]}; }
  function card(k,v,h,cls,percent){ var cls2=cls==='bad'?'maint-final-bad':cls==='warn'?'maint-final-warn':cls==='ok'?'maint-final-ok':''; var bar=percent==null?'':'<div class="maint-final-bar"><div class="maint-final-fill '+(levelByPct(percent)==='bad'?'bad':levelByPct(percent)==='warn'?'warn':'')+'" style="width:'+Math.round(percent)+'%"></div></div>'; return '<div class="maint-final-card"><div class="maint-final-k">'+esc(k)+'</div><div class="maint-final-v '+cls2+'">'+esc(v)+'</div><div class="maint-final-h">'+esc(h||'')+'</div>'+bar+'</div>'; }
  function renderShell(){
    var root=q('view-maintenance'); if(!root)return;
    if(root.dataset.maintFinalReady==='1')return;
    root.dataset.maintFinalReady='1';
    root.innerHTML='<section id="maintFinalOverall" class="maint-final-overall"><div><div class="maint-final-title">健康狀態</div><div id="maintFinalSummary" class="maint-final-sub">尚未讀取</div></div><div class="maint-final-actions"><button id="maintRefresh" class="secondary" type="button">重新整理</button><button id="maintCheck" type="button">一鍵檢查</button><a id="maintLogs" class="button secondary" href="/admin/activity">登入與檔案紀錄</a></div></section><section id="maintCards" class="maint-final-grid"></section><section class="grid two"><div class="panel"><div class="toolbar"><h2>一鍵檢查結果</h2></div><div id="maintChecks" class="maint-final-checks"><div class="small">按「一鍵檢查」可測 SQLite、教材目錄、磁碟、RAM、systemd、Cloudflare Tunnel、最近錯誤。</div></div></div><div class="panel"><div class="toolbar"><h2>維護操作</h2></div><div class="quick"><button id="maintReloadAll" class="secondary" type="button">重新整理全部</button><button id="maintCopyDiag" class="secondary" type="button">複製診斷摘要</button><button id="maintStopAll" class="danger" type="button">全部暫停</button></div></div></section><section class="panel" style="margin-top:14px"><div class="toolbar"><h2>原始資料</h2><button id="maintToggleRaw" class="secondary" type="button">顯示/隱藏</button></div><pre id="maintRaw" class="maint-final-raw" style="display:none">{}</pre></section>';
    // MAINTENANCE_BUTTONS_MODULE_PHASE4A3: maintenance.js owns API invocation.
    function maintenanceEvent(name){window.dispatchEvent(new CustomEvent('tronclass:maintenance:'+name));}
    q('maintRefresh').onclick=function(){maintenanceEvent('refresh')};
    q('maintCheck').onclick=function(){maintenanceEvent('check')};
    q('maintReloadAll').onclick=function(){maintenanceEvent('refresh'); if(window.tronDashboardRefreshStrategy)window.tronDashboardRefreshStrategy.force(4000);};
    q('maintCopyDiag').onclick=function(){maintenanceEvent('copy')};
    q('maintStopAll').onclick=function(){maintenanceEvent('stop')};
    q('maintToggleRaw').onclick=function(){var r=q('maintRaw');r.style.display=r.style.display==='none'?'block':'none'};
    q('maintLogs').href=withToken('/admin/activity');
  }
  function renderHealth(h){
    lastHealth=h;
    var acc=h.accounts||{}, mem=h.memory||{}, db=h.database||{}, cmd=h.commands||{}, disk=parseDisk(cmd.disk);
    var memPct=pct(mem.mem_used,mem.mem_total), swapPct=pct(mem.swap_used,mem.swap_total), diskPct=disk?Number(String(disk.use||'0').replace('%','')):null;
    var active=String(cmd.systemctl||'').trim()==='active';
    var bad=[]; var warn=[];
    if(db.ok===false)bad.push('SQLite 異常'); if(!active)bad.push('systemd 非 active'); if(memPct>=90)bad.push('RAM 過高'); if(diskPct!=null&&diskPct>=90)bad.push('磁碟過高');
    if((acc.paused||0)>0)warn.push('有帳號暫停'); if((acc.detected||0)>0)warn.push('有點名偵測'); if((acc.total_failures||0)>0)warn.push('有失敗紀錄'); if(memPct>=75&&memPct<90)warn.push('RAM 偏高'); if(diskPct!=null&&diskPct>=75&&diskPct<90)warn.push('磁碟偏高');
    var overall=q('maintFinalOverall'); overall.className='maint-final-overall '+(bad.length?'bad':warn.length?'warn':'');
    q('maintFinalSummary').textContent=bad.length?bad.join('、'):(warn.length?warn.join('、'):'服務、SQLite、記憶體、磁碟看起來正常');
    q('maintCards').innerHTML=[
      card('帳號服務',(acc.running||0)+'/'+(acc.total||0),'暫停 '+(acc.paused||0)+'｜偵測 '+(acc.detected||0),(acc.paused||acc.total_failures)?'warn':'ok'),
      card('SQLite',db.ok===false?'異常':'正常',db.error||('帳號列 '+(db.account_rows||0)),db.ok===false?'bad':'ok'),
      card('RAM',mem.mem_used_label||'-','可用 '+(mem.mem_available_label||'-')+'｜總量 '+(mem.mem_total_label||'-'),levelByPct(memPct),memPct),
      card('Swap',mem.swap_used_label||'-','總量 '+(mem.swap_total_label||'-')+'｜可用 '+(mem.swap_free_label||'-'),levelByPct(swapPct),swapPct),
      card('磁碟',disk?disk.use:'-',disk?(disk.used+' / '+disk.size+'｜可用 '+disk.avail):'讀取失敗',diskPct==null?'warn':levelByPct(diskPct),diskPct),
      card('服務',active?'active':String(cmd.systemctl||'-'),'PID '+(h.pid||'-')+'｜Uptime '+((h.uptime&&h.uptime.label)||'-'),active?'ok':'bad'),
      card('最後輪詢',acc.latest_poll_timestamp?new Date(Number(acc.latest_poll_timestamp)*1000).toLocaleString():'尚未','失敗 '+(acc.total_failures||0),(acc.total_failures||0)>0?'warn':'ok'),
      card('Load',h.loadavg||'-','系統負載')
    ].join('');
    q('maintRaw').textContent=JSON.stringify({health:lastHealth,check:lastCheck},null,2);
  }
  function renderCheck(c){
    lastCheck=c;
    var checks=c.checks||[];
    q('maintChecks').innerHTML=checks.length?checks.map(function(x){var level=x.level||(x.ok?'ok':'bad');return '<div class="maint-final-check"><div class="maint-final-check-top"><span>'+esc(x.name||'-')+'</span><span class="pill '+(level==='ok'?'good':level==='warn'?'warn':'bad')+'">'+(level==='ok'?'正常':level==='warn'?'注意':'異常')+'</span></div><div class="small" style="margin-top:6px">'+esc(x.message||'')+'</div></div>'}).join(''):'<div class="small">沒有檢查結果</div>';
    q('maintRaw').textContent=JSON.stringify({health:lastHealth,check:lastCheck},null,2);
  }
  async function loadHealth(check){
    renderShell();
    try{var h=await api(check?'/admin/api/health/check':'/admin/api/health'); if(h.checks)renderCheck(h); else renderHealth(h);}
    catch(e){q('maintFinalSummary').textContent='讀取失敗：'+(e.message||e); toast(e.message||'健康資訊讀取失敗',false)}
  }
  async function doAction(action){
    if(action==='stop'&&!confirm('確定要暫停全部帳號監控？'))return;
    try{await fetch('/admin/api/action?token='+encodeURIComponent(token()),{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action:action})}).then(parseResponse);toast('操作完成');loadHealth(false)}catch(e){toast(e.message||'操作失敗',false)}
  }
  function copyDiag(){var text=JSON.stringify({health:lastHealth,check:lastCheck,time:new Date().toISOString()},null,2); if(navigator.clipboard)navigator.clipboard.writeText(text).then(function(){toast('已複製診斷摘要')}); else prompt('請手動複製：',text)}
  function cleanupMobileNav(){
    if(window.matchMedia&&matchMedia('(min-width:821px)').matches){
      document.querySelectorAll('#mobileNav,.mobile-nav,#mobileNavFinalV2,#mobileStableDashboardNav,#mobileDashboardNavForce,#mobileDashboardNav,#mobileDashboardFinalNav,.mobile-dashboard-nav,.mobile-dashboard-nav-force,.mobile-dashboard-nav-integrated,#mobileSheet,.mobile-sheet,#mobileMoreFinalV2,#mobileStableDashboardSheet,#mobileDashboardFinalSheet,#mobileDashboardSheetForce,#mobileDashboardSheetBackdrop,.mobile-dashboard-sheet-backdrop,.mobile-dashboard-sheet-force').forEach(function(el){el.style.display='none';el.style.visibility='hidden';el.style.pointerEvents='none'});
    }
  }
  // TRONCLASS_MAINTENANCE_API_V1: stable bridge for external maintenance.js.
  window.TronclassDashboard=window.TronclassDashboard||{};
  window.TronclassDashboard.maintenance={
    loadHealth:function(){return loadHealth(false)},
    loadHealthCheck:function(){return loadHealth(true)},
    copyDiagnostic:copyDiag,
    refreshAll:function(){return loadHealth(false)},
    stopAll:function(){return doAction('stop')}
  };

  function boot(){renderShell();cleanupMobileNav(); if(location.hash==='#maintenance'||(q('view-maintenance')&&q('view-maintenance').classList.contains('active')))loadHealth(false); var n=0,t=setInterval(function(){cleanupMobileNav();n++;if(n>=20)clearInterval(t)},500)}
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot);else boot();
  document.addEventListener('click',function(ev){if(ev.target.closest&&ev.target.closest('[data-view="maintenance"],.tab'))setTimeout(function(){renderShell();loadHealth(false);cleanupMobileNav()},100)},true);
})();
