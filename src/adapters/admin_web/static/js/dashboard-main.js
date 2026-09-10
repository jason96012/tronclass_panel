/* INLINE_SPLIT_V1：dashboard-main.js 原本內嵌於 dashboard.html 的 <body> 主腳本與其後三個內聯 <script>，抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: main (原 dashboard.html:1775) ── */
(function(){'use strict';let status=null,entries=[],draft=[],view='overview',timer=null;const PERIOD={1:['08:10','09:00'],2:['09:10','10:00'],3:['10:10','11:00'],4:['11:10','12:00'],5:['12:10','13:00'],6:['13:10','14:00'],7:['14:10','15:00'],8:['15:10','16:00'],9:['16:10','17:00'],10:['17:10','18:00']};const DAY={mon:'星期一',tue:'星期二',wed:'星期三',thu:'星期四',fri:'星期五',sat:'星期六',sun:'星期日'},ORDER={sun:0,mon:1,tue:2,wed:3,thu:4,fri:5,sat:6};function q(id){return document.getElementById(id)}/* DASHBOARD_SAFE_BIND_V1：主腳本的 .onclick 綁定改走 qb()：元素不存在（被後續 patch 移除或改名）時回一顆未掛載的 button，讓賦值靜默略過，而不是 TypeError 讓整段主腳本（含 loadStatus／renderAll）中止。 過去 #refreshAll 只能用 CSS 隱藏就是這個原因。 */function qb(id){var el=document.getElementById(id);if(el)return el;try{console.warn('[dashboard] missing element #'+id+' (binding skipped)')}catch(_){}return document.createElement('button')}function token(){return''}function esc(v){return String(v==null?'':v).replace(/[&<>"']/g,ch=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]))}function toast(t,ok=true){const el=q('toast');el.textContent=t;el.className='toast visible '+(ok?'ok':'bad');clearTimeout(el._timer);el._timer=setTimeout(()=>el.classList.remove('visible'),3200)}function withToken(path){return path}async function parseResponse(res){const b=await res.json().catch(()=>({}));if(!res.ok){/* VIEWER_ROLE_V1：唯讀帳號的 403 顯示中文說明而非錯誤碼 */const e=new Error((b.error==='viewer_read_only'&&b.message)||b.error||('HTTP '+res.status));e.status=res.status;e.code=b.error||'';throw e}return b}async function api(path,opts={}){opts=opts||{};opts.credentials=opts.credentials||'same-origin';opts.cache=opts.cache||'no-store';return fetch(path,opts).then(parseResponse)}/* LOGIN_PAGE_STANDALONE_V1：Dashboard 內沒有登入卡；session 失效一律導回登入頁（帶 next）。 */function showLogin(msg){document.body.classList.remove('logged-in','auth-ok','dash-auth-ok');document.documentElement.classList.remove('tron-auth-ok','tron-auth-pending','dash-auth-ok');document.documentElement.classList.add('tron-auth-logged-out');window.__tronAuthState='out';if(window.__tronLoginRedirecting)return;window.__tronLoginRedirecting=true;try{var u=new URL('/admin',location.origin);var cur=location.pathname+location.search+location.hash;if(cur&&cur!=='/admin'&&cur!=='/admin/')u.searchParams.set('next',cur);u.searchParams.set('v','sessionexpired'+Date.now());location.replace(u.pathname+u.search)}catch(_){location.replace('/admin')}}function showApp(acct){document.body.classList.add('logged-in','auth-ok','dash-auth-ok');document.documentElement.classList.remove('tron-auth-pending','tron-auth-logged-out');document.documentElement.classList.add('tron-auth-ok','dash-auth-ok');window.__tronAuthState='ok';q('brandSub').textContent=acct||'管理中心';syncLinks()}function login(ev){return window.tronAuth&&typeof window.tronAuth.login==='function'?window.tronAuth.login(ev):undefined}function logout(){if(window.tronAuth&&typeof window.tronAuth.logout==='function')return window.tronAuth.logout();fetch('/admin/logout',{method:'POST',credentials:'same-origin',cache:'no-store',redirect:'manual'}).catch(function(){}).then(function(){location.replace('/admin')})}function syncLinks(){[['meLink','mMe'],['filesLink','mFiles'],['activityLink','mActivity'],['logsLink','mLogs']].forEach(function(group){var target=group[0]==='meLink'?'/admin/me':group[0]==='filesLink'?'/admin/files':'/admin/activity';group.forEach(function(id){var el=q(id);if(el)el.href=withToken(target)})})}/* STATUS_SLIM_V1：/admin/api/status 是全站最常打的 API。只有「點名」分頁需要完整 30 筆歷史；
   總覽只畫 8 筆；其他分頁完全不需要。依目前分頁決定查詢參數，12 個帳號時 payload 約縮 70%。 */
function statusHistoryQuery(withHistory){if(withHistory===false)return '?include_history=false';var v=(typeof view==='string')?view:'overview';if(v==='rollcall')return '';if(v==='overview'||!status)return '?history_limit=8';return '?include_history=false'}
async function fetchStatusSlim(withHistory){var hq=statusHistoryQuery(withHistory);var st=await api('/admin/api/status'+hq,{cache:'no-store'});if(st&&typeof st==='object'){try{Object.defineProperty(st,'__fullHistory',{value:hq==='',enumerable:false})}catch(_){}}return st}
async function loadStatus(withHistory){try{/* STATUS_SLIM_V1：精簡回應（無歷史或只有 8 筆）時，保留上一輪已拿到的較完整歷史，避免畫面閃空 */var prevAccounts=(status&&status.accounts)?status.accounts:null;var prevFull=!!(status&&status.__fullHistory);status=await fetchStatusSlim(withHistory);if(prevAccounts&&status&&status.accounts&&!status.__fullHistory){status.accounts=status.accounts.map(function(a){var p=prevAccounts.find(function(x){return x.account_id===a.account_id});if(p&&p.rollcall_history&&p.rollcall_history.length>((a.rollcall_history||[]).length)){a.rollcall_history=p.rollcall_history}return a});if(prevFull){try{Object.defineProperty(status,'__fullHistory',{value:true,enumerable:false})}catch(_){}}}if(status&&status.must_change_password){location.replace('/admin/password?first=1');return}showApp(status.current_account_id);renderAll();noteStatusSuccess()}catch(e){if(e.status===401){
/* ADMIN_PAGE_TRI_STATE_V1：頁面是伺服器驗證過 session 才送出的，所以這裡的 401 只會是
   「頁面存活期間 access 過期／被撤銷」。先用 refresh cookie 靜默續期一次（core-api.js 的
   __tronTryRefresh），成功就重抓；失敗才導回登入頁。不再需要 200/500ms 的盲目重試。 */
try{var refreshed=false;if(typeof window.__tronTryRefresh==='function'){try{refreshed=!!(await window.__tronTryRefresh())}catch(_){refreshed=false}}
if(refreshed){status=await fetchStatusSlim(withHistory);if(status&&status.must_change_password){location.replace('/admin/password?first=1');return}showApp(status.current_account_id);renderAll();noteStatusSuccess();return}
showLogin('登入狀態失效，請重新登入。')}catch(retryErr){if(retryErr.status===401){showLogin('登入狀態失效，請重新登入。')}else noteStatusFailure(retryErr)}}else noteStatusFailure(e)}}
/* STATUS_LOAD_FEEDBACK_V1：狀態抓取失敗（逾時／502／離線）時，原本只閃一個 3 秒 toast，之後畫面就停在
   「尚未更新」與「—」，看起來像卡住。改為：右上角 pill 常駐顯示「載入失敗（原因）重試中…」、
   toast 只在前兩次與之後每第 4 次顯示（不洗版），並以 3s→6s→12s→30s 退避自動重試
   （15 秒輪詢仍在；去重快取會把兩者合併成一趟請求）。成功後清掉標記。 */
var statusFailStreak=0,statusRetryTimer=null;
function noteStatusSuccess(){statusFailStreak=0;clearTimeout(statusRetryTimer);statusRetryTimer=null;var pill=q('lastUpdated');if(pill)pill.classList.remove('warn')}
function statusFailureLabel(e){var offline=(typeof navigator!=='undefined'&&navigator.onLine===false);if(offline)return{short:'離線',long:'目前離線，恢復連線後會自動重試'};if(e&&e.name==='TimeoutError')return{short:'逾時',long:'伺服器 20 秒內沒有回應（可能正在重新編譯或重啟），稍後自動重試'};if(e&&e.status)return{short:'HTTP '+e.status,long:'讀取狀態失敗（HTTP '+e.status+(e.message&&e.message!=='HTTP '+e.status?'：'+e.message:'')+'），稍後自動重試'};return{short:'網路錯誤',long:'讀取狀態失敗（'+((e&&e.message)||'網路錯誤')+'），稍後自動重試'}}
function noteStatusFailure(e){statusFailStreak++;var pill=q('lastUpdated');var hasData=!!(status&&status.accounts);var label=statusFailureLabel(e);if(pill){pill.textContent=(hasData?'更新失敗':'載入失敗')+'（'+label.short+'）重試中…';pill.classList.add('warn')}
if(statusFailStreak<=2||statusFailStreak%4===0)toast(label.long,false);
/* 回報到伺服器 system log（client:status_load_failed），管理員在「登入與檔案紀錄」就看得到是哪種失敗。 */
try{if((statusFailStreak===1||statusFailStreak===5)&&window.TronclassClientLog&&typeof window.TronclassClientLog.report==='function')window.TronclassClientLog.report('status_load_failed',label.short+' 第 '+statusFailStreak+' 次：'+label.long)}catch(_){}
clearTimeout(statusRetryTimer);var delay=Math.min(3000*Math.pow(2,Math.max(0,statusFailStreak-1)),30000);statusRetryTimer=setTimeout(function(){statusRetryTimer=null;if(!document.hidden)loadStatus()},delay)}
function acctList(){return(status&&status.accounts)||[]}function recent(ts,s){return!!ts&&Date.now()/1000-Number(ts)<(s||1800)}function done(a,r){return a&&r&&(String(a.last_success_rollcall_id||'')===String(r)||(a.rollcall_history||[]).some(h=>String(h.rollcall_id||'')===String(r)&&h.success))}function detected(){const map=new Map();acctList().forEach(a=>{if(!a.last_detected_timestamp||!recent(a.last_detected_timestamp))return;const key=(a.last_detected_rollcall_id||a.last_detected_course)+':'+(a.last_detected_course||'點名');const it=map.get(key)||{course:a.last_detected_course||'點名',rid:a.last_detected_rollcall_id||'',url:a.last_detected_url||'',ts:a.last_detected_timestamp,accounts:[],done:0,total:0};if(!it.url&&a.last_detected_url)it.url=a.last_detected_url;it.accounts.push(a.account_id);it.total++;if(done(a,it.rid))it.done++;map.set(key,it)});return Array.from(map.values())}function histories(){const out=[];acctList().forEach(a=>(a.rollcall_history||[]).forEach(h=>out.push(Object.assign({account_id:a.account_id},h))));return out.sort((a,b)=>Number(b.timestamp||0)-Number(a.timestamp||0))}function fmt(ts){if(!ts)return'尚未';return new Date(Number(ts)*1000).toLocaleString()}function renderStats(){const a=acctList(),run=a.filter(x=>x.is_running).length,det=detected().length,fail=a.reduce((s,x)=>s+Number(x.consecutive_failures||0),0);q('statTotal').textContent=a.length;/* REST_DAY_STAT_CARD_V1：休息日（週末／國定假日／寒暑假）沒有自動偵測，第二張卡直接顯示 */const restReason=status&&status.rest_day;const runCard=q('statRunning').closest('.stat');if(runCard)runCard.classList.toggle('rest-day',!!restReason);const runLabel=runCard?runCard.querySelector('.stat-label'):null;if(restReason){if(runLabel)runLabel.textContent='自動偵測';q('statRunning').textContent='未啟動';q('statPaused').textContent='今天是'+restReason+'・'+run+' 個帳號待上課日恢復'}else{if(runLabel)runLabel.textContent='運行中';q('statRunning').textContent=run;q('statPaused').textContent='暫停 '+(a.length-run)}q('statDetected').textContent=det;q('statFailures').textContent=fail;q('lastUpdated').textContent='更新：'+new Date().toLocaleTimeString();/* REST_DAY_NO_POLL_V1：休息日狀態不在總覽提示顯示（可到設定 → 其他設定 → 休息日與寒暑假查看） */}function renderDetected(id){const el=q(id),items=detected();el.innerHTML=items.length?items.map(it=>'<div class="item"><div class="item-top"><span>'+esc(it.course)+' #'+esc(it.rid||'-')+'</span><span class="pill warn">'+it.accounts.length+' 帳號</span></div><div class="item-meta">完成 '+it.done+'/'+it.total+'</div>'+(it.url?'<a class="button secondary" href="'+esc(it.url)+'" target="_blank">開啟點名頁</a>':'')+'</div>').join(''):'<div class="empty">目前沒有進行中的點名。</div>'}function renderRecent(){const list=histories().slice(0,8);q('recentResults').innerHTML=list.length?list.map(h=>'<div class="item"><div class="item-top"><span>'+esc(h.course_name||'-')+'</span><span class="pill '+(h.success?'good':'bad')+'">'+(h.success?'成功':'未成功')+'</span></div><div class="item-meta">'+esc(h.account_id)+'｜'+fmt(h.timestamp)+'</div></div>').join(''):'<div class="empty">目前沒有近期點名紀錄。</div>'}function renderAccounts(){let arr=acctList(),needle=q('accountSearch').value.trim().toLowerCase();if(needle)arr=arr.filter(a=>((a.account_id||'')+' '+(a.user_name||a.username||'')).toLowerCase().includes(needle));q('accountsSummary').textContent='顯示 '+arr.length+' / '+acctList().length+' 個帳號';q('accountsBody').innerHTML=arr.map(a=>'<tr><td><span class="pill '+(a.is_running?'good':'bad')+'">'+(a.is_running?'運行中':'暫停')+'</span></td><td><b>'+esc(a.account_id)+'</b><div class="small">'+esc(a.user_name||a.username||'')+'</div></td><td><span class="pill '+(a.account_admin?'good':'')+'">'+(a.account_admin?'管理員':a.account_viewer?'檢視者':'一般帳號')+'</span></td><td>'+fmt(a.last_poll_timestamp)+'</td><td>'+esc(a.last_success_course||'尚未')+'</td><td>'+esc(a.last_detected_course||'無近期點名')+'</td><td>'+esc(a.consecutive_failures||0)+failureHint(a)+reauthHint(a)+'</td><td><div class="actions-cell"><button data-action="force" data-account="'+esc(a.account_id)+'">檢查</button><button class="secondary" data-action="reauth" data-account="'+esc(a.account_id)+'">重登</button><button class="secondary" data-action="'+(a.is_running?'stop':'start')+'" data-account="'+esc(a.account_id)+'">'+(a.is_running?'暫停':'恢復')+'</button>'+(status&&status.is_admin?'<button class="secondary" data-action="'+(a.account_admin?'unset_admin':'set_admin')+'" data-account="'+esc(a.account_id)+'">'+(a.account_admin?'取消管理員':'設為管理員')+'</button>':'')+(status&&status.is_admin&&!a.account_admin?'<button class="secondary" data-action="'+(a.account_viewer?'unset_viewer':'set_viewer')+'" data-account="'+esc(a.account_id)+'">'+(a.account_viewer?'取消檢視者':'設為檢視者')+'</button>':'')+(status&&status.is_admin?'<button class="secondary" data-action="reset_password" data-account="'+esc(a.account_id)+'">重製密碼</button>':'')+'</div></td></tr>').join('')||'<tr><td colspan="8">沒有帳號</td></tr>';renderInactiveAccounts()}/* FAILURE_DIAGNOSIS_V1：連續失敗欄下方顯示分類與自助建議（成功後後端會清空） */function failureHint(a){const d=a&&a.last_failure;if(!d||!Number(a.consecutive_failures||0))return'';return'<div class="small" style="color:var(--bad,#f87171);max-width:260px;white-space:normal"><b>'+esc(d.label||'')+'</b><div>'+esc(d.advice||'')+'</div>'+(d.message?'<div style="opacity:.75;word-break:break-all">'+esc(d.message)+'</div>':'')+'</div>'}/* REAUTH_VISIBLE_V1：連續失敗欄下方顯示最近一次重新認證（自動／手動）的時間與結果 */function reauthHint(a){const r=a&&a.last_reauth;if(!r||!r.at)return'';const ok=!!r.ok;const who=r.trigger==='auto'?'自動重登':'手動重登';return'<div class="small reauth-hint '+(ok?'reauth-ok':'reauth-bad')+'"><span class="pill '+(ok?'good':'bad')+'">'+who+(ok?'成功':'失敗')+'</span> '+esc(fmt(r.at))+(!ok&&r.message?'<div class="reauth-msg">'+esc(r.message)+'</div>':'')+'</div>'}/* ACCOUNT_HOT_ADD_FEEDBACK_V1 */function renderInactiveAccounts(){const box=q('inactiveAccountsBox'),list=q('inactiveAccountsList');if(!box||!list)return;const items=(status&&status.is_admin&&Array.isArray(status.inactive_accounts))?status.inactive_accounts:[];if(!items.length){box.style.display='none';list.innerHTML='';return}box.style.display='';list.innerHTML=items.map(it=>{/* VIEWER_NO_SCHOOL_API_V1：檢視者依設計不監控，顯示中性徽章與「取消檢視者」 */if(it.is_viewer){return'<div class="item"><div class="item-top"><span><b>'+esc(it.account_id)+'</b> <span class="small">'+esc(it.username||'')+'｜'+esc(it.provider||'')+'</span></span><span class="pill">檢視者（不監控）</span></div><div class="item-meta">此帳號為唯讀檢視者：不登入學校系統、不做點名監控。取消檢視者後需按「啟動監控」或重啟服務才會開始監控。</div><button class="secondary" data-action="unset_viewer" data-account="'+esc(it.account_id)+'">取消檢視者</button></div>'}return'<div class="item"><div class="item-top"><span><b>'+esc(it.account_id)+'</b> <span class="small">'+esc(it.username||'')+'｜'+esc(it.provider||'')+'</span></span><span class="pill bad">未監控</span></div><div class="item-meta" style="color:var(--bad,#f87171)">'+(it.diagnosis?'<b>'+esc(it.diagnosis.label||'')+'</b>｜'+esc(it.diagnosis.advice||'')+'<br>':'')+esc(it.last_error||'尚未嘗試啟動（例如以 CLI 新增後尚未啟動）')+'</div><button data-action="start_monitor" data-account="'+esc(it.account_id)+'">啟動監控</button></div>'}).join('')}function renderRollcall(){renderDetected('rollcallDetectedList');const list=histories();q('rollcallHistoryList').innerHTML=list.length?list.slice(0,30).map(h=>'<div class="item"><div class="item-top"><span>'+esc(h.course_name||'-')+'</span><span class="pill '+(h.success?'good':'bad')+'">'+(h.success?'成功':'未成功')+'</span></div><div class="item-meta">'+esc(h.account_id)+'｜'+fmt(h.timestamp)+'｜#'+esc(h.rollcall_id||'-')+'</div></div>').join(''):'<div class="empty">目前沒有點名紀錄。</div>'}function pStart(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][0]:''}function pEnd(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][1]:''}function mins(t){const m=String(t||'').match(/(\d{1,2}):(\d{2})/);return m?Number(m[1])*60+Number(m[2]):null}function nextEntry(){const day={sun:0,mon:1,tue:2,wed:3,thu:4,fri:5,sat:6},now=new Date(),today=now.getDay(),cur=now.getHours()*60+now.getMinutes();let best=null;entries.forEach(e=>{const d=day[e.weekday],sm=mins(e.start_time||pStart(e.period_start));if(d==null||sm==null)return;let score=((d-today+7)%7)*1440+sm-cur;if(score<0)score+=10080;if(!best||score<best.score)best={score,entry:e}});return best}function renderNext(){const e=nextEntry()&&nextEntry().entry;if(!e){q('nextClassCard').innerHTML='<div class="next-card"><div class="next-grid"><div class="next-item next-time"><div class="next-k">開始時間</div><div class="next-v">尚無</div></div><div class="next-item"><div class="next-k">課程</div><div class="next-v">尚無課表資料</div></div><div class="next-item next-room"><div class="next-k">教室</div><div class="next-v">-</div></div></div></div>';return}q('nextClassCard').innerHTML='<div class="next-card"><div class="next-grid"><div class="next-item next-time"><div class="next-k">開始時間</div><div class="next-v">'+esc(e.start_time||pStart(e.period_start))+'</div></div><div class="next-item"><div class="next-k">課程</div><div class="next-v">'+esc(e.course_name)+'</div></div><div class="next-item next-room"><div class="next-k">教室</div><div class="next-v">'+esc(e.classroom||'-')+'</div></div></div><div class="next-meta"><span class="next-chip">'+esc(e.weekday)+'</span><span class="next-chip">第 '+esc(e.period_start)+(e.period_end&&e.period_end!==e.period_start?' - '+esc(e.period_end):'')+' 節</span><span class="next-chip">'+esc((e.start_time||pStart(e.period_start))+' - '+(e.end_time||pEnd(e.period_end||e.period_start)))+'</span><span class="next-chip">老師：'+esc(e.teacher||'-')+'</span></div></div>'}async function loadTimetable(){try{const account=status&&status.current_account_id&&status.current_account_id!=='<token-admin>'?status.current_account_id:(acctList()[0]&&acctList()[0].account_id)||'';if(!account)return;const body=await api('/admin/api/timetable?account_id='+encodeURIComponent(account),{cache:'no-store'});entries=body.entries||[];renderNext();renderTimetable()}catch(e){renderTimetable()}}function renderTimetable(){q('ttMain').innerHTML=entries.length?'<div class="table-wrap"><table class="tt-table"><thead><tr><th>星期</th><th>節次</th><th>時間</th><th>教室</th><th>老師</th><th>課程</th></tr></thead><tbody>'+entries.map(e=>'<tr><td>'+esc(e.weekday)+'</td><td>第 '+esc(e.period_start)+(e.period_end&&e.period_end!==e.period_start?' - '+esc(e.period_end):'')+' 節</td><td>'+esc((e.start_time||pStart(e.period_start))+' - '+(e.end_time||pEnd(e.period_end||e.period_start)))+'</td><td>'+esc(e.classroom||'-')+'</td><td>'+esc(e.teacher||'-')+'</td><td><b>'+esc(e.course_name||'-')+'</b></td></tr>').join('')+'</tbody></table></div>':'<div class="empty">目前沒有課表資料。</div>'}function renderAll(){renderStats();renderDetected('detectedList');renderRecent();renderAccounts();renderRollcall();loadTimetable()}async function doAction(action,account){if(action==='start_monitor'){/* ACCOUNT_HOT_ADD_FEEDBACK_V1 */toast('正在登入並啟動監控，請稍候…');try{const r=await api('/admin/api/action',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action:action,account_id:account})});toast((r&&r.message)||'已開始監控');await loadStatus()}catch(e){toast(e.message||'啟動監控失敗',false)}return}if(action==='reset_password'){if(!confirm('確定要重製「'+(account||'')+'」的 Dashboard 密碼？\n重製後此帳號需用 Tronclass 密碼登入，並重新設定 Dashboard 密碼。'))return}try{const r=await api('/admin/api/action',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({action,account_id:account||undefined})});if(action==='reset_password'){toast('已重製密碼，該帳號可用 Tronclass 密碼登入');await loadStatus();return}if(action==='set_admin'||action==='unset_admin'){const item=(status&&Array.isArray(status.accounts))?status.accounts.find(a=>a.account_id===account):null;if(item){item.account_admin=action==='set_admin';renderAccounts()}toast(action==='set_admin'?'已設為管理員':'已取消管理員');await loadStatus();return}/* VIEWER_ROLE_V1 */if(action==='set_viewer'||action==='unset_viewer'){const item=(status&&Array.isArray(status.accounts))?status.accounts.find(a=>a.account_id===account):null;if(item){item.account_viewer=action==='set_viewer';renderAccounts()}toast(action==='set_viewer'?'已設為檢視者（唯讀），該帳號需重新登入':'已取消檢視者，該帳號需重新登入');await loadStatus();return}/* ACTION_FEEDBACK_V1：伺服器回報「實際發生了什麼」（哪些帳號被啟動／原本就在跑…），直接顯示 */toast((r&&r.message)||'操作完成');await loadStatus()}catch(e){toast(e.message||'操作失敗',false)}}async function loadHealth(){try{const h=await api('/admin/api/health',{cache:'no-store'});q('healthBox').textContent=JSON.stringify(h,null,2)}catch(e){q('healthBox').textContent=e.message||'讀取失敗'}}function switchView(v){view=v;if(v==='materials'){location.href=withToken('/admin/files');return}document.querySelectorAll('.tab').forEach(b=>{const on=b.dataset.view===v;b.classList.toggle('active',on);/* A11Y_BASICS_V1 */if(b.getAttribute('role')==='tab')b.setAttribute('aria-selected',on?'true':'false')});document.querySelectorAll('.view').forEach(x=>x.classList.toggle('active',x.id==='view-'+v));history.replaceState(null,'','#'+v);if(v==='maintenance')loadHealth();if(v==='timetable')loadTimetable();/* STATUS_SLIM_V1：進點名分頁時若只有部分歷史，補抓完整 30 筆 */if(v==='rollcall'&&status&&!status.__fullHistory){loadStatus(true)}document.querySelectorAll('.mobile-nav button[data-view]').forEach(b=>b.classList.toggle('active',b.dataset.view===v))}qb('logoutBtn').onclick=function(){if(window.tronAuth&&window.tronAuth.logout)window.tronAuth.logout()};qb('refreshAll').onclick=loadStatus;qb('forceAll').onclick=()=>doAction('force');qb('reauthAll').onclick=()=>doAction('reauth');qb('startAll').onclick=()=>doAction('start');qb('forceAllRollcall').onclick=()=>doAction('force');qb('stopAll').onclick=()=>doAction('stop');q('accountSearch').oninput=renderAccounts;
if(q('quickPromoteAdminBtn')){
    qb('quickPromoteAdminBtn').onclick = async function(){
      var id = prompt('請輸入要設為管理員的帳號 ID：');
      if(!id || !id.trim()) return;
      try {
        await doAction('set_admin', id.trim());
        toast('已將帳號 ' + id.trim() + ' 設為管理員');
      } catch(e) {
        toast(e.message || '操作失敗', false);
      }
    };
  }
  if(q('quickAddAccountBtn') && q('addAccountModal')){
    function showAddAccountModal(){ q('addAccountModal').classList.add('visible'); q('addAccountMsg').textContent=''; q('addAccountMsg').style.color=''; }
    function hideAddAccountModal(){ q('addAccountModal').classList.remove('visible'); }
    qb('quickAddAccountBtn').onclick = showAddAccountModal;
    /* ADD_ACCOUNT_ROLE_V1：依角色更新提示與密碼欄說明 */
    if(q('addAccRole')){
      q('addAccRole').onchange = function(){
        var r = q('addAccRole').value, hint = q('addAccRoleHint'), pw = q('addAccPassword');
        if(r === 'viewer'){ if(hint) hint.textContent = '觀看使用者：只能登入儀表板查看，不會登入學校系統、不監控點名。這裡填的密碼只做為首次登入儀表板用（登入後會要求改成 Dashboard 密碼）。'; if(pw) pw.placeholder = '首次登入儀表板用的密碼'; }
        else if(r === 'admin'){ if(hint) hint.textContent = '管理員：會登入學校系統並監控點名，並可管理所有帳號與設定。'; if(pw) pw.placeholder = 'Tronclass 密碼'; }
        else { if(hint) hint.textContent = '一般使用者：會登入學校系統並自動監控點名。'; if(pw) pw.placeholder = 'Tronclass 密碼'; }
      };
    }
    qb('closeAddAccount').onclick = hideAddAccountModal;
    qb('addAccountCancel').onclick = hideAddAccountModal;
    qb('addAccountModal').onclick = function(e){ if(e.target === q('addAccountModal')) hideAddAccountModal(); };
    qb('addAccountSubmit').onclick = async function(){
      var id = (q('addAccId').value||'').trim();
      var provider = (q('addAccProvider').value||'').trim();
      var username = (q('addAccUsername').value||'').trim();
      var password = q('addAccPassword').value;
      var discord = (q('addAccDiscord').value||'').trim();
      var line = (q('addAccLine').value||'').trim();
      /* ADD_ACCOUNT_ROLE_V1 */
      var role = (q('addAccRole') && q('addAccRole').value) || 'user';
      var isAdmin = role === 'admin';
      var msg = q('addAccountMsg');
      if(!provider || !username || !password){ msg.textContent='請填寫 Provider、帳號/學號 與 密碼'; return; }
      /* ACCOUNT_HOT_ADD_V1：後端會先寫入 DB 再實際登入學校系統並啟動監控，可能需要數十秒 */
      msg.style.color = '';
      msg.textContent = role === 'viewer' ? '正在建立觀看使用者…' : '正在寫入並登入帳號、啟動監控，請稍候（可能需要數十秒）…';
      q('addAccountSubmit').disabled = true;
      try {
        var res = await api('/admin/api/action', { method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify({ action:'add_account', id:id||undefined, provider:provider, username:username, password:password, discord_user_id:discord||undefined, line_user_id:line||undefined, is_admin:isAdmin, role:role }) });
        if(res && res.ok && res.viewer){
          /* ADD_ACCOUNT_ROLE_V1：觀看使用者本來就不監控，不算警告 */
          msg.style.color = '';
          toast(res.message || '已新增觀看使用者');
          hideAddAccountModal();
          q('addAccId').value=''; q('addAccUsername').value=''; q('addAccPassword').value=''; q('addAccDiscord').value=''; q('addAccLine').value=''; if(q('addAccRole')) q('addAccRole').value='user';
          await loadStatus();
        } else if(res && res.ok && res.monitor_started === false){
          /* ACCOUNT_HOT_ADD_FEEDBACK_V1：已寫入資料庫但監控沒有啟動 —— 對話框留著、紅字顯示原因，
             另用紅色 toast 提醒；帳號會出現在「未在監控的帳號」區塊供重試 */
          msg.style.color = 'var(--bad,#f87171)';
          msg.textContent = res.warning || res.message || '帳號已新增，但監控未啟動';
          toast('帳號已新增，但監控未啟動，請看對話框內的原因', false);
          await loadStatus();
        } else if(res && res.ok){
          msg.style.color = '';
          toast(res.message || '已新增帳號');
          hideAddAccountModal();
          q('addAccId').value=''; q('addAccUsername').value=''; q('addAccPassword').value=''; q('addAccDiscord').value=''; q('addAccLine').value=''; if(q('addAccRole')) q('addAccRole').value='user';
          await loadStatus();
        } else {
          msg.textContent = (res && res.error) || '新增失敗';
        }
      } catch(e){ msg.style.color = 'var(--bad,#f87171)'; msg.textContent = (e && e.message) || '新增失敗'; toast((e && e.message) || '新增失敗', false); }
      finally { q('addAccountSubmit').disabled = false; }
    };
  }qb('refreshHealth').onclick=loadHealth;qb('closeHistory').onclick=()=>q('historyModal').classList.remove('visible');document.querySelectorAll('.tab').forEach(b=>b.onclick=()=>switchView(b.dataset.view));document.body.addEventListener('click',e=>{const sw=e.target.closest('[data-switch]');if(sw)return switchView(sw.dataset.switch);const a=e.target.closest('[data-action]');if(a)return doAction(a.dataset.action,a.dataset.account)});document.querySelectorAll('.mobile-nav button[data-view]').forEach(b=>b.onclick=()=>switchView(b.dataset.view));qb('mobileMore').onclick=()=>q('mobileSheet').classList.add('visible');qb('mobileSheet').onclick=e=>{if(e.target===q('mobileSheet'))q('mobileSheet').classList.remove('visible')};qb('mRefresh').onclick=()=>location.reload();qb('mLogout').onclick=function(){if(window.tronAuth&&window.tronAuth.logout)window.tronAuth.logout()};syncLinks();switchView((location.hash||'#overview').slice(1));
// TRONCLASS_DASHBOARD_RUNTIME_API_V1: external modules use only this stable surface.
window.TronclassDashboard={q:q,token:token,withToken:withToken,api:api,parseResponse:parseResponse,toast:toast,loadStatus:loadStatus,switchView:switchView,getStatus:function(){return status},refresh:function(){return loadStatus()}};
// TRONCLASS_ACCOUNTS_API_V1: bridge used by the later accounts.js module.
window.TronclassDashboard.accounts={render:renderAccounts,action:doAction,list:acctList};
/* STATUS_LOAD_FEEDBACK_V1：伺服器已把帳號注入 <html data-account>，標頭不必等 /admin/api/status 回來才從「請先登入」換成帳號。 */
try{var __bootAcct=document.documentElement.getAttribute('data-account');if(__bootAcct&&q('brandSub'))q('brandSub').textContent=__bootAcct}catch(_){}
loadStatus();window.addEventListener('tronclass:auth-state',function(ev){if(ev&&ev.detail==='ok')loadStatus();});
/* STATUS_LOAD_FEEDBACK_V1：網路恢復／回到前景時若上一輪失敗，立刻重試（不等退避計時）。 */
window.addEventListener('online',function(){if(statusFailStreak>0){clearTimeout(statusRetryTimer);statusRetryTimer=null;loadStatus()}});
document.addEventListener('visibilitychange',function(){if(!document.hidden&&statusFailStreak>0){clearTimeout(statusRetryTimer);statusRetryTimer=null;loadStatus()}},{passive:true});var statusPollTick=0;timer=setInterval(()=>{if(!document.hidden&&(window.__tronAuthState==='ok'||document.body.classList.contains('logged-in'))){statusPollTick=(statusPollTick+1)%4;/* DASHBOARD_LIVE_EVENTS_V1：SSE 連線健康時（60 秒內有事件／心跳）改由事件驅動刷新，輪詢只保留每 60 秒一次的完整校正 */var live=window.TronclassLiveEvents;if(live&&live.connected()&&(Date.now()-live.lastEventAt())<60000&&statusPollTick!==0)return;loadStatus(statusPollTick===0)}},15000);
// TRONCLASS_DASHBOARD_MODULE_READY_V1: deferred external modules wait for this.
window.setTimeout(function(){
  window.TronclassDashboard=window.TronclassDashboard||{};
  window.TronclassDashboard.ready=true;
  window.dispatchEvent(new Event('tronclass:dashboard:ready'));
},0);
})();

/* ── inline block: hideMobileMoreWhenLoggedOutScriptV1 (原 dashboard.html:1876) ── */
(function(){
  'use strict';
  if (window.HIDE_MOBILE_MORE_WHEN_LOGGED_OUT_V1) return;
  window.HIDE_MOBILE_MORE_WHEN_LOGGED_OUT_V1 = true;

  function isMobile(){ return window.matchMedia && matchMedia('(max-width:820px)').matches; }
  function hasAuthClass(){
    return document.documentElement.classList.contains('dash-auth-ok') ||
      document.body.classList.contains('logged-in') ||
      document.body.classList.contains('auth-ok') ||
      document.body.classList.contains('dash-auth-ok') ||
      document.documentElement.classList.contains('mobile-buttons-ok');
  }
  function sheets(){
    return Array.prototype.slice.call(document.querySelectorAll([
      '#mobileSheet',
      '.mobile-sheet',
      '#mobileDashboardFinalSheet',
      '#mobileStableDashboardSheet',
      '#mobileDashboardSheetForce',
      '#mobileDashboardSheetBackdrop',
      '.mobile-dashboard-sheet-backdrop',
      '.mobile-dashboard-sheet-force'
    ].join(',')));
  }
  function cleanup(){
    var allow = isMobile() && hasAuthClass();
    sheets().forEach(function(el){
      if (!allow) {
        el.classList.remove('visible');
        el.style.display = 'none';
        el.style.visibility = 'hidden';
        el.style.pointerEvents = 'none';
      } else {
        // Let CSS decide visible/non-visible when authenticated mobile.
        el.style.display = '';
        el.style.visibility = '';
        el.style.pointerEvents = '';
      }
    });
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', cleanup);
  else cleanup();
  window.addEventListener('resize', cleanup);
  window.addEventListener('pageshow', cleanup);
  /* NO_POLL_ENSURE_V1：原 20 次 × 500 ms 輪詢改為事件驅動——sheet 是由其他腳本以
     document.body.appendChild 建立，MutationObserver（只看 body 直屬子節點）＋登入狀態事件
     就涵蓋所有會讓 cleanup 結果改變的時機。 */
  try {
    new MutationObserver(function(){ cleanup(); }).observe(document.body || document.documentElement, {childList:true});
  } catch(_) {}
  window.addEventListener('tronclass:auth-state', cleanup);
  window.addEventListener('tronclass:dashboard:ready', cleanup);
})();

/* ── inline block: mobileDashboardNavMoreLayoutScriptV2 (原 dashboard.html:1937) ── */
(function(){
  'use strict';
  if (window.MOBILE_DASHBOARD_NAV_MORE_LAYOUT_V2) return;
  window.MOBILE_DASHBOARD_NAV_MORE_LAYOUT_V2 = true;
  if (!(location.pathname === '/admin' || location.pathname === '/admin/' || location.pathname === '/admin/dashboard' || location.pathname === '/admin')) return;

  function q(id){ return document.getElementById(id); }
  function isMobile(){ return window.matchMedia && matchMedia('(max-width:820px)').matches; }
  function token(){ return ''; }
  function withToken(path){ return path; }
  function icon(name){
    var icons = {
      overview:'<svg viewBox="0 0 24 24"><path d="M4 13h6V4H4z"></path><path d="M14 20h6V4h-6z"></path><path d="M4 20h6v-3H4z"></path></svg>',
      accounts:'<svg viewBox="0 0 24 24"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"></path><circle cx="9" cy="7" r="4"></circle></svg>',
      rollcall:'<svg viewBox="0 0 24 24"><path d="M9 11l3 3L22 4"></path><path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11"></path></svg>',
      timetable:'<svg viewBox="0 0 24 24"><path d="M4 5h16v15H4z"></path><path d="M8 3v4"></path><path d="M16 3v4"></path><path d="M4 10h16"></path></svg>',
      more:'<svg viewBox="0 0 24 24"><circle cx="5" cy="12" r="1.5"></circle><circle cx="12" cy="12" r="1.5"></circle><circle cx="19" cy="12" r="1.5"></circle></svg>'
    };
    return icons[name] || '';
  }
  function currentView(){
    var tab = document.querySelector('.tab.active[data-view]');
    if (tab) return tab.dataset.view;
    var view = document.querySelector('.view.active[id^="view-"]');
    if (view) return view.id.replace(/^view-/, '');
    return (location.hash || '#overview').slice(1) || 'overview';
  }
  function switchView(view){
    var tab = document.querySelector('.tab[data-view="' + view + '"]');
    if (tab) tab.click();
    else {
      document.querySelectorAll('.view').forEach(function(v){ v.classList.toggle('active', v.id === 'view-' + view); });
      document.querySelectorAll('.tab').forEach(function(t){ t.classList.toggle('active', t.dataset.view === view); });
      history.replaceState(null, '', '#' + view);
    }
    updateActive();
  }
  function updateActive(){
    var v = currentView();
    document.querySelectorAll('#mobileNavFinalV2 [data-view]').forEach(function(btn){ btn.classList.toggle('active', btn.dataset.view === v); });
  }
  function logout(){
    /* LOGOUT_SERVER_SIDE_V1：統一走 session-security.js 的 secureLogout（POST /admin/logout）。 */
    if (typeof window.tronSecureLogout === 'function') return window.tronSecureLogout();
    if (window.tronAuth && typeof window.tronAuth.logout === 'function') return window.tronAuth.logout();
  }
  function closeMore(){ var el=q('mobileMoreFinalV2'); if(el) el.classList.remove('visible'); }
  function openMore(){ var el=q('mobileMoreFinalV2'); if(el) el.classList.add('visible'); }
  async function ensureAuthClass(){
    if (!token()) { document.documentElement.classList.remove('mobile-final-nav-auth'); return; }
    if (document.documentElement.classList.contains('dash-auth-ok') || document.body.classList.contains('auth-ok') || document.body.classList.contains('logged-in') || document.body.classList.contains('dash-auth-ok')) {
      document.documentElement.classList.add('mobile-final-nav-auth'); return;
    }
    try {
      var res = await fetch('/admin/api/status', {cache:'no-store'});
      if (res.ok) document.documentElement.classList.add('mobile-final-nav-auth');
      else document.documentElement.classList.remove('mobile-final-nav-auth');
    } catch(_) {}
  }
  function hideOld(){
    ['mobileDashboardNavForce','mobileDashboardNav','mobileDashboardFinalNav','mobileStableDashboardNav','mobileSheet','mobileDashboardFinalSheet','mobileStableDashboardSheet','mobileDashboardSheetForce','mobileDashboardSheetBackdrop','mobileCloudDriveFabV1'].forEach(function(id){ var el=q(id); if(el){ el.style.display='none'; el.style.visibility='hidden'; el.style.pointerEvents='none'; } });
    document.querySelectorAll('.mobile-dashboard-nav,.mobile-dashboard-nav-force,.mobile-dashboard-nav-integrated,.mobile-dashboard-sheet-backdrop,.mobile-dashboard-sheet-force,.mobile-sheet').forEach(function(el){ el.style.display='none'; el.style.visibility='hidden'; el.style.pointerEvents='none'; });
  }
  function ensureNav(){
    if (!isMobile()) { hideOld(); return; }
    hideOld();
    if (!q('mobileNavFinalV2')) {
      var nav = document.createElement('nav');
      nav.id = 'mobileNavFinalV2';
      nav.innerHTML = '' +
        '<button data-view="overview">' + icon('overview') + '<span>總覽</span></button>' +
        '<button data-view="accounts">' + icon('accounts') + '<span>帳號</span></button>' +
        '<button data-view="rollcall">' + icon('rollcall') + '<span>點名</span></button>' +
        '<button data-view="timetable">' + icon('timetable') + '<span>課表</span></button>' +
        '<button id="mobileMoreBtnFinalV2">' + icon('more') + '<span>更多</span></button>';
      document.body.appendChild(nav);
      nav.querySelectorAll('[data-view]').forEach(function(btn){ btn.onclick = function(){ switchView(btn.dataset.view); }; });
      q('mobileMoreBtnFinalV2').onclick = openMore;
    }
    if (!q('mobileMoreFinalV2')) {
      var sheet = document.createElement('div');
      sheet.id = 'mobileMoreFinalV2';
      sheet.innerHTML = '<div class="sheet"><h3>更多功能</h3><div class="more-grid"><a id="moreSettingsFinalV2" class="button secondary" href="/admin/settings">設定</a><a id="moreHomeworkFinalV2" class="button secondary" href="/admin/homework">作業截止時間</a><a id="moreDriveFinalV2" class="button secondary" href="/admin/files">雲端硬碟</a><a id=\"moreBulletinsFinalV2\" class=\"button secondary\" href=\"/admin/bulletins\">學校公告</a><a id=\"moreAiFinalV2\" class=\"button secondary\" href=\"/admin/ai\">AI 對話</a><a id="moreActivityFinalV2" class="button secondary" data-admin-only="1" href="/admin/activity">登入/檔案紀錄</a><a id="moreBlacklistFinalV2" class="button secondary" data-admin-only="1" href="/admin/blacklist">黑名單</a><button id="moreRefreshFinalV2" class="secondary" type="button">重新整理</button><a id="moreDetectionScheduleFinalV2" class="button secondary" data-detection-schedule-entry="1" href="/admin/detection-schedule">點名排程</a><button id="moreLogoutFinalV2" class="danger" type="button">登出</button></div></div>';
      document.body.appendChild(sheet);
      sheet.onclick = function(ev){ if (ev.target === sheet) closeMore(); };
      sheet.querySelectorAll('[data-sheet-view]').forEach(function(btn){ btn.onclick = function(){ closeMore(); switchView(btn.dataset.sheetView); }; });
      q('moreRefreshFinalV2').onclick = function(){ location.reload(); };
      q('moreLogoutFinalV2').onclick = logout;
    }
    if(q('moreDriveFinalV2')) q('moreDriveFinalV2').href = withToken('/admin/files');
    if(q('moreBulletinsFinalV2')) q('moreBulletinsFinalV2').href = withToken('/admin/bulletins');
    if(q('moreAiFinalV2')) q('moreAiFinalV2').href = withToken('/admin/ai');
    if(q('moreActivityFinalV2')) q('moreActivityFinalV2').href = withToken('/admin/activity');
    if(q('moreMeFinalV2')) q('moreMeFinalV2').href = withToken('/admin/me');
    if(q('moreLogsFinalV2')) q('moreLogsFinalV2').href = withToken('/admin/logs');
    if(q('moreBlacklistFinalV2')) q('moreBlacklistFinalV2').href = withToken('/admin/blacklist');
    updateActive();
    ensureAuthClass();
  }
  function boot(){
    ensureNav();
    /* NO_POLL_ENSURE_V1：原 20 次 × 500 ms 輪詢只是為了等「登入狀態 class」與晚載入的模組
       就位；改為監聽狀態事件（auth-state／dashboard:ready／pageshow）與 body 的 class 變化
       （showApp 會加 logged-in／auth-ok）。不觀察 html 元素：ensureAuthClass 自己會改它的
       class，觀察會自我觸發。 */
    window.addEventListener('tronclass:auth-state', ensureNav);
    window.addEventListener('tronclass:dashboard:ready', ensureNav);
    window.addEventListener('pageshow', ensureNav);
    try {
      if (document.body) new MutationObserver(function(){ ensureNav(); }).observe(document.body, {attributes:true, attributeFilter:['class']});
    } catch(_) {}
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
  window.addEventListener('resize', ensureNav);
  window.addEventListener('hashchange', updateActive);
  document.addEventListener('click', function(ev){ if (ev.target.closest && ev.target.closest('.tab,[data-view],#mobileMoreBtnFinalV2')) setTimeout(function(){ ensureNav(); updateActive(); }, 100); }, true);
})();

/* ── inline block: consolidatedAdminAuthBodyV2 (原 dashboard.html:2069) ── */
(function(){
  'use strict';
  /* CONSOLIDATED_ADMIN_AUTH_V2：Dashboard 端不再內建登入表單。
     - login()：導到登入頁（帶 next）。
     - logout()：POST /admin/logout（伺服器清 cookie 後 302 /admin → 登入頁）。
     - verify()：以伺服器注入的狀態為準；session 失效由 session-security.js 處理。 */
  if (window.CONSOLIDATED_ADMIN_AUTH_V1) return;
  window.CONSOLIDATED_ADMIN_AUTH_V1 = true;
  function currentPath(){ return location.pathname + location.search + location.hash; }
  function loginUrl(){ var u = new URL('/admin', location.origin); var cur = currentPath(); if (cur && cur !== '/admin' && cur !== '/admin/') u.searchParams.set('next', cur); return u.pathname + u.search; }
  function login(){ location.replace(loginUrl()); }
  function logout(){ if (typeof window.tronSecureLogout === 'function') return window.tronSecureLogout(); fetch('/admin/logout',{method:'POST',credentials:'same-origin',cache:'no-store',redirect:'manual'}).catch(function(){}).then(function(){location.replace('/admin')}); }
  function verify(){ return Promise.resolve(window.__tronAuthStatus || {}); }
  function syncLinks(){}
  window.tronAuth = { token: function(){ return ''; }, verify: verify, login: login, logout: logout, syncLinks: syncLinks };
})();
