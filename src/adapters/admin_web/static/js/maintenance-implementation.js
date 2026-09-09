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
  function token(){ return ''; }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g, function(ch){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]; }); }
  function withToken(path){ return path; }
  function toast(text, ok){ var el=q('toast'); if(el){el.textContent=text;el.className='toast visible '+(ok===false?'bad':'ok');clearTimeout(el._timer);el._timer=setTimeout(function(){el.classList.remove('visible')},3200)} else console.log(text); }
  async function parseResponse(res){ var b=await res.json().catch(function(){return{}}); if(!res.ok){var e=new Error(b.error||('HTTP '+res.status)); e.status=res.status; throw e;} return b; }
  // MAINTENANCE_CORE_API_PHASE4F3
  async function api(path){ if(window.TronclassRuntime&&window.TronclassRuntime.api)return window.TronclassRuntime.api(path,{cache:'no-store'}); return fetch(path,{credentials:'same-origin',cache:'no-store'}).then(parseResponse); }
  function pct(used,total){ used=Number(used||0); total=Number(total||0); return total>0?Math.max(0,Math.min(100,used/total*100)):0; }
  function levelByPct(p){ return p>=90?'bad':(p>=75?'warn':'ok'); }
  function parseDisk(out){ var lines=String(out||'').trim().split(/\n/); if(lines.length<2)return null; var c=lines[1].trim().split(/\s+/); return {size:c[1],used:c[2],avail:c[3],use:c[4],mount:c[5]}; }
  function card(k,v,h,cls,percent){ var cls2=cls==='bad'?'maint-final-bad':cls==='warn'?'maint-final-warn':cls==='ok'?'maint-final-ok':''; var bar=percent==null?'':'<div class="maint-final-bar"><div class="maint-final-fill '+(levelByPct(percent)==='bad'?'bad':levelByPct(percent)==='warn'?'warn':'')+'" style="width:'+Math.round(percent)+'%"></div></div>'; return '<div class="maint-final-card"><div class="maint-final-k">'+esc(k)+'</div><div class="maint-final-v '+cls2+'">'+esc(v)+'</div><div class="maint-final-h">'+esc(h||'')+'</div>'+bar+'</div>'; }
  function renderShell(){
    var root=q('view-maintenance'); if(!root)return;
    if(root.dataset.maintFinalReady==='1')return;
    root.dataset.maintFinalReady='1';
    root.innerHTML='<section id="maintFinalOverall" class="maint-final-overall"><div><div class="maint-final-title">健康狀態</div><div id="maintFinalSummary" class="maint-final-sub">尚未讀取</div></div><div class="maint-final-actions"><button id="maintRefresh" class="secondary" type="button">重新整理</button><button id="maintCheck" type="button">一鍵檢查</button><a id="maintLogs" class="button secondary" href="/admin/activity">登入與檔案紀錄</a><a id="maintBlacklist" class="button secondary" data-admin-only="1" href="/admin/blacklist">黑名單</a></div></section><section id="maintCards" class="maint-final-grid"></section><!-- METRICS_HISTORY_V1 --><section class="panel" style="margin-bottom:14px"><div class="toolbar"><div><h2>24 小時趨勢</h2><div id="maintTrendSub" class="small">每分鐘一筆，重啟後重新累積</div></div><div class="toolbar-right"><select id="maintTrendRange"><option value="60">最近 1 小時</option><option value="360">最近 6 小時</option><option value="1440" selected>最近 24 小時</option></select></div></div><div id="maintTrend" class="maint-trend-grid"><div class="small">讀取中…</div></div></section><section class="grid two"><div class="panel"><div class="toolbar"><h2>一鍵檢查結果</h2></div><div id="maintChecks" class="maint-final-checks"><div class="small">按「一鍵檢查」可測 SQLite、教材目錄、磁碟、RAM、Cloudflare Tunnel、最近錯誤。</div></div></div><div class="panel"><div class="toolbar"><h2>維護操作</h2></div><div class="quick"><button id="maintReloadAll" class="secondary" type="button">重新整理全部</button><button id="maintCopyDiag" class="secondary" type="button">複製診斷摘要</button><button id="maintStopAll" class="danger" type="button">全部暫停</button></div></div></section><section class="panel" style="margin-top:14px"><div class="toolbar"><h2>原始資料</h2><button id="maintToggleRaw" class="secondary" type="button">顯示/隱藏</button></div><pre id="maintRaw" class="maint-final-raw" style="display:none">{}</pre></section>';
    // MAINTENANCE_BUTTONS_MODULE_PHASE4A3: maintenance.js owns API invocation.
    function maintenanceEvent(name){window.dispatchEvent(new CustomEvent('tronclass:maintenance:'+name));}
    q('maintRefresh').onclick=function(){maintenanceEvent('refresh')};
    q('maintCheck').onclick=function(){maintenanceEvent('check')};
    q('maintReloadAll').onclick=function(){maintenanceEvent('refresh'); if(window.tronDashboardRefreshStrategy)window.tronDashboardRefreshStrategy.force(4000);};
    q('maintCopyDiag').onclick=function(){maintenanceEvent('copy')};
    q('maintStopAll').onclick=function(){maintenanceEvent('stop')};
    q('maintToggleRaw').onclick=function(){var r=q('maintRaw');r.style.display=r.style.display==='none'?'block':'none'};
    q('maintLogs').href=withToken('/admin/activity');
    q('maintBlacklist').href=withToken('/admin/blacklist');
  }
  function renderHealth(h){
    lastHealth=h;
    var acc=h.accounts||{}, mem=h.memory||{}, db=h.database||{}, cmd=h.commands||{}, wd=cmd.watchdog||{}, disk=parseDisk(cmd.disk);
    var wdServices=wd.services||[], cf=cmd.cloudflare||{}, wdDown=wdServices.filter(function(x){return x.id!=='dashboard'&&!x.alive;});
    /* TUNNEL_RESTART_LOOP_FIX_V1：Connector 的 log 錯誤不再算成「服務停止」——它的 PID 明明活著；
       只有真的一直在重啟（restart_loop）或致命錯誤才升級為 bad */
    var cfBad=!!(cf.fatal_error||cf.restart_loop), cfWarn=!!(cf.recent_error&&!cfBad);
    if(cfBad)wdDown.push({label:'Cloudflare Connector'});
    var memPct=pct(mem.mem_used,mem.mem_total), swapPct=pct(mem.swap_used,mem.swap_total), diskPct=disk?Number(String(disk.use||'0').replace('%','')):null;
    // Containers (Pterodactyl) have no systemd; the backend then reports
    // "running (container, ...)". Treat that as healthy instead of flagging
    // "systemd 非 active" on a service that is demonstrably alive.
    var svc=String(cmd.systemctl||'').trim();
    var active=svc==='active'||svc.indexOf('running')===0;
    var bad=[]; var warn=[];
    if(db.ok===false)bad.push('SQLite 異常'); if(!active)bad.push('服務非執行中'); if(wdDown.length)bad.push('Watchdog 有服務停止'); if(memPct>=90)bad.push('RAM 過高'); if(diskPct!=null&&diskPct>=90)bad.push('磁碟過高');
    if((acc.paused||0)>0)warn.push('有帳號暫停'); if((acc.detected||0)>0)warn.push('有點名偵測'); if((acc.total_failures||0)>0)warn.push('有失敗紀錄'); if(memPct>=75&&memPct<90)warn.push('RAM 偏高'); if(diskPct!=null&&diskPct>=75&&diskPct<90)warn.push('磁碟偏高');
    var overall=q('maintFinalOverall'); overall.className='maint-final-overall '+(bad.length?'bad':warn.length?'warn':'');
    q('maintFinalSummary').textContent=bad.length?bad.join('、'):(warn.length?warn.join('、'):'服務、SQLite、記憶體、磁碟看起來正常');
    q('maintCards').innerHTML=[
      card('帳號服務',(acc.running||0)+'/'+(acc.total||0),'暫停 '+(acc.paused||0)+'｜偵測 '+(acc.detected||0),(acc.paused||acc.total_failures)?'warn':'ok'),
      card('SQLite',db.ok===false?'異常':'正常',db.error||('帳號列 '+(db.account_rows||0)),db.ok===false?'bad':'ok'),
      card('RAM',mem.mem_used_label||'-','可用 '+(mem.mem_available_label||'-')+'｜總量 '+(mem.mem_total_label||'-'),levelByPct(memPct),memPct),
      card('Swap',mem.swap_used_label||'-','總量 '+(mem.swap_total_label||'-')+'｜可用 '+(mem.swap_free_label||'-'),levelByPct(swapPct),swapPct),
      card('磁碟',disk?disk.use:'-',disk?(disk.used+' / '+disk.size+'｜可用 '+disk.avail):'讀取失敗',diskPct==null?'warn':levelByPct(diskPct),diskPct),
      card('服務',active?(svc.indexOf('running')===0?'執行中':'active'):String(cmd.systemctl||'-'),'PID '+(h.pid||'-')+'｜Uptime '+((h.uptime&&h.uptime.label)||'-'),active?'ok':'bad'),
      card('最後輪詢',acc.latest_poll_timestamp?new Date(Number(acc.latest_poll_timestamp)*1000).toLocaleString():'尚未','失敗 '+(acc.total_failures||0),(acc.total_failures||0)>0?'warn':'ok'),
      card('Load',h.loadavg||'-','系統負載'),
      /* LOG_DISK_GUARD_V1：log／設定檔占用；超過 300 MB 視為異常（正常應在數十 MB 內） */
      (function(){ var st=h.storage||{}, lg=st.logs||{}, cf=st.config||{}; var lb=Number(lg.bytes||0); return card('Log 占用', lg.label||'-', 'logs/ '+(lg.files||0)+' 檔（最大 '+((lg.largest&&lg.largest.name+' '+lg.largest.label)||'-')+'）｜每檔上限 '+(lg.per_log_cap_label||'-')+'｜config/ '+(cf.label||'-'), lb>300*1024*1024?'bad':lb>100*1024*1024?'warn':'ok'); })(),
      card('Cloudflare Connector',cf.restart_loop?('一小時內重啟 '+cf.restarts_last_hour+' 次'):cf.fatal_error?'Token / Tunnel 設定錯誤':cf.recent_error?'連線中斷、重連中':(cf.pid_alive?'程序正常':'未啟動'),cf.restart_loop?('最後一次 '+(cf.last_restart||'-')+'｜請看 logs/cloudflared-watchdog.log'):cf.fatal_error?'請檢查 config/cf.token 與 Cloudflare 上的 Tunnel 是否仍存在':cf.recent_error?('log 失敗計數 '+(cf.log_failures||0)+'｜HTTP 失敗計數 '+(cf.http_failures||0)):('PID + 新增 log 檢查'+(cf.restarts_last_hour?('｜一小時內重啟 '+cf.restarts_last_hour+' 次'):'')),cfBad?'bad':cfWarn?'warn':(cf.pid_alive?'ok':'warn')),
      card('Watchdog',wd.enabled?(wdDown.length?'有服務停止':'監控中'):'未啟用','PID + HTTP 檢查｜間隔 '+(wd.interval_secs||15)+' 秒｜'+(wdServices.map(function(x){return x.label+': '+(x.alive?'正常':'停止')}).join('、')||'尚無 PID'),wd.enabled?(wdDown.length?'bad':'ok'):'warn')
    ].join('');
    q('maintRaw').textContent=JSON.stringify({health:lastHealth,check:lastCheck},null,2);
  }
  function renderCheck(c){
    lastCheck=c;
    var checks=c.checks||[];
    q('maintChecks').innerHTML=checks.length?checks.map(function(x){var level=x.level||(x.ok?'ok':'bad');return '<div class="maint-final-check"><div class="maint-final-check-top"><span>'+esc(x.name||'-')+'</span><span class="pill '+(level==='ok'?'good':level==='warn'?'warn':'bad')+'">'+(level==='ok'?'正常':level==='warn'?'注意':'異常')+'</span></div><div class="small" style="margin-top:6px">'+esc(x.message||'')+'</div></div>'}).join(''):'<div class="small">沒有檢查結果</div>';
    q('maintRaw').textContent=JSON.stringify({health:lastHealth,check:lastCheck},null,2);
  }
  /* METRICS_HISTORY_V1：純 SVG 折線（CSP 不允許外部圖表庫），電腦 2 欄、手機 1 欄 */
  var lastHistory=null;
  function trendSvg(points,opts){
    var W=600,H=150,padL=38,padR=8,padT=10,padB=22;
    var vals=points.map(function(p){return p.v}).filter(function(v){return v!=null});
    if(!vals.length)return '<div class="maint-trend-empty">尚無資料</div>';
    var max=opts.max!=null?opts.max:Math.max.apply(null,vals), min=opts.min!=null?opts.min:0;
    if(max<=min)max=min+1;
    var n=points.length, xs=function(i){return padL+(n<=1?0:(i/(n-1))*(W-padL-padR))}, ys=function(v){return padT+(1-(v-min)/(max-min))*(H-padT-padB)};
    var d='',started=false;
    points.forEach(function(p,i){if(p.v==null){started=false;return}d+=(started?'L':'M')+xs(i).toFixed(1)+' '+ys(p.v).toFixed(1)+' ';started=true});
    var area='';
    if(opts.fill){var segs=d.trim().split('M').filter(Boolean);area=segs.map(function(seg){var pts=seg.trim().split(/L|\s+/).filter(Boolean);var first=pts[0],last=pts[pts.length-2];return 'M'+first+' '+(H-padB)+' L'+seg.trim()+' L'+last+' '+(H-padB)+' Z'}).join(' ')}
    var grid='';[0,.5,1].forEach(function(f){var y=padT+f*(H-padT-padB);var val=max-(max-min)*f;grid+='<line x1="'+padL+'" x2="'+(W-padR)+'" y1="'+y.toFixed(1)+'" y2="'+y.toFixed(1)+'" class="mt-grid"/><text x="'+(padL-4)+'" y="'+(y+4).toFixed(1)+'" class="mt-lbl" text-anchor="end">'+esc(opts.fmt?opts.fmt(val):Math.round(val))+'</text>'});
    var t0=points[0].ts,t1=points[n-1].ts,fmtT=function(ts){var d=new Date(ts*1000);return ('0'+d.getHours()).slice(-2)+':'+('0'+d.getMinutes()).slice(-2)};
    grid+='<text x="'+padL+'" y="'+(H-6)+'" class="mt-lbl">'+fmtT(t0)+'</text><text x="'+(W-padR)+'" y="'+(H-6)+'" class="mt-lbl" text-anchor="end">'+fmtT(t1)+'</text>';
    var warn='';if(opts.warnAt!=null&&opts.warnAt>min&&opts.warnAt<max){var wy=ys(opts.warnAt);warn='<line x1="'+padL+'" x2="'+(W-padR)+'" y1="'+wy.toFixed(1)+'" y2="'+wy.toFixed(1)+'" class="mt-warn"/>'}
    return '<svg viewBox="0 0 '+W+' '+H+'" preserveAspectRatio="none" class="mt-svg" role="img" aria-label="'+esc(opts.title)+'">'+grid+warn+(area?'<path d="'+area+'" class="mt-area '+(opts.cls||'')+'"/>':'')+'<path d="'+d.trim()+'" class="mt-line '+(opts.cls||'')+'"/></svg>';
  }
  function trendCard(title,cur,sub,svg){return '<div class="maint-trend-card"><div class="maint-trend-head"><span class="maint-trend-title">'+esc(title)+'</span><span class="maint-trend-cur">'+esc(cur)+'</span></div><div class="small">'+esc(sub||'')+'</div>'+svg+'</div>'}
  function renderHistory(hist){
    lastHistory=hist; var root=q('maintTrend'); if(!root)return;
    var all=(hist&&hist.samples)||[]; var range=Number((q('maintTrendRange')||{}).value||1440); var pts=all.slice(-range);
    if(!pts.length){root.innerHTML='<div class="small">尚無資料——服務啟動後每分鐘累積一筆。</div>';return}
    var last=pts[pts.length-1];
    var upSecs=Math.max(0,Math.floor(Date.now()/1000)-Number(hist.started_at||0));
    var upLabel=(upSecs>=86400?Math.floor(upSecs/86400)+' 天 ':'')+Math.floor((upSecs%86400)/3600)+' 小時 '+Math.floor((upSecs%3600)/60)+' 分';
    var totalPolls=pts.reduce(function(a,p){return a+(p.poll_count||0)},0), totalErr=pts.reduce(function(a,p){return a+(p.poll_errors||0)},0);
    var okPct=totalPolls?((totalPolls-totalErr)/totalPolls*100):null;
    var sub=q('maintTrendSub'); if(sub)sub.textContent='每分鐘一筆｜本次啟動已運行 '+upLabel+'｜區間內輪詢 '+totalPolls+' 次'+(okPct!=null?('，成功率 '+okPct.toFixed(1)+'%'):'');
    var latencyPts=pts.map(function(p){return {ts:p.ts,v:p.poll_avg_ms}}), maxPts=pts.map(function(p){return {ts:p.ts,v:p.poll_max_ms}});
    var latMax=Math.max.apply(null,pts.map(function(p){return p.poll_max_ms||0}).concat([500]));
    root.innerHTML=[
      trendCard('輪詢延遲（平均）',last.poll_avg_ms!=null?last.poll_avg_ms+' ms':'—','每分鐘平均；虛線＝最大值。持續超過 3000 ms 表示學校端變慢或網路不穩',
        trendSvg(latencyPts,{title:'輪詢延遲',min:0,max:latMax,fmt:function(v){return Math.round(v)+'ms'},fill:true,cls:'c-lat',warnAt:3000})+trendSvg(maxPts,{title:'最大延遲',min:0,max:latMax,fmt:function(v){return Math.round(v)+'ms'},cls:'c-max'})),
      trendCard('運行中帳號',last.running+' / '+last.total,'掉到 0 但總數不是 0 → 休息日或全部暫停；忽然下降 → 有帳號登入失敗',
        trendSvg(pts.map(function(p){return {ts:p.ts,v:p.running}}),{title:'運行中帳號',min:0,max:Math.max(last.total,1),fill:true,cls:'c-run'})),
      trendCard('RAM 使用率',last.mem_pct!=null?last.mem_pct.toFixed(0)+'%':'—','系統整體；本程序 RSS '+(last.rss_bytes?(last.rss_bytes/1048576).toFixed(0)+' MB':'—'),
        trendSvg(pts.map(function(p){return {ts:p.ts,v:p.mem_pct}}),{title:'RAM',min:0,max:100,fmt:function(v){return Math.round(v)+'%'},fill:true,cls:'c-mem',warnAt:90})),
      trendCard('輪詢失敗／連續失敗',(last.poll_errors||0)+' 次／分・累計 '+(last.failures||0),'折線＝每分鐘失敗次數；任何非 0 都值得看一下 log',
        trendSvg(pts.map(function(p){return {ts:p.ts,v:p.poll_errors||0}}),{title:'輪詢失敗',min:0,max:Math.max.apply(null,pts.map(function(p){return p.poll_errors||0}).concat([3])),fill:true,cls:'c-err'}))
    ].join('');
  }
  async function loadHistory(){try{renderHistory(await api('/admin/api/health/history'))}catch(e){var r=q('maintTrend');if(r)r.innerHTML='<div class="small">趨勢讀取失敗：'+esc(e.message||e)+'</div>'}}
  document.addEventListener('change',function(ev){if(ev.target&&ev.target.id==='maintTrendRange'&&lastHistory)renderHistory(lastHistory)});
  async function loadHealth(check){
    renderShell();
    try{var h=await api(check?'/admin/api/health/check':'/admin/api/health'); if(h.checks)renderCheck(h); else renderHealth(h);}
    catch(e){q('maintFinalSummary').textContent='讀取失敗：'+(e.message||e); toast(e.message||'健康資訊讀取失敗',false)}
    if(!check)loadHistory();
  }
  async function doAction(action){
    if(action==='stop'&&!confirm('確定要暫停全部帳號監控？'))return;
    try{var r=await fetch('/admin/api/action',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action:action})}).then(parseResponse);/* ACTION_FEEDBACK_V1 */toast((r&&r.message)||'操作完成');loadHealth(false)}catch(e){toast(e.message||'操作失敗',false)}
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

  /* NO_POLL_ENSURE_V1：cleanupMobileNav 只在桌機寬度隱藏手機底欄；底欄由其他腳本 appendChild 到 body，
     改用 body 直屬 childList 的 MutationObserver＋resize 取代 20 次 × 500 ms 輪詢。 */
  function boot(){renderShell();cleanupMobileNav(); if(location.hash==='#maintenance'||(q('view-maintenance')&&q('view-maintenance').classList.contains('active')))loadHealth(false); try{new MutationObserver(function(){cleanupMobileNav()}).observe(document.body||document.documentElement,{childList:true})}catch(_){} window.addEventListener('resize',cleanupMobileNav,{passive:true})}
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot);else boot();
  document.addEventListener('click',function(ev){if(ev.target.closest&&ev.target.closest('[data-view="maintenance"],.tab'))setTimeout(function(){renderShell();loadHealth(false);cleanupMobileNav()},100)},true);
})();
