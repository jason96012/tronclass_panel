/* TIMETABLE_MODULE_AFTER_READY_PHASE4B2 */
(function(){
  function start(){
/* TIMETABLE_UI_MODULE_PHASE4B */

/* ttStableSingleManagerScriptV6 */
(function(){
  'use strict';
  if(window.TT_STABLE_SINGLE_MANAGER_V6) return;
  window.TT_STABLE_SINGLE_MANAGER_V6 = true;

  var PERIOD={1:['08:10','09:00'],2:['09:10','10:00'],3:['10:10','11:00'],4:['11:10','12:00'],5:['12:10','13:00'],6:['13:10','14:00'],7:['14:10','15:00'],8:['15:10','16:00'],9:['16:10','17:00'],10:['17:10','18:00']};
  var DAYS=[['mon','星期一'],['tue','星期二'],['wed','星期三'],['thu','星期四'],['fri','星期五'],['sat','星期六'],['sun','星期日']];
  var DAY_LABEL={mon:'星期一',tue:'星期二',wed:'星期三',thu:'星期四',fri:'星期五',sat:'星期六',sun:'星期日'};
  var TEMPLATE_KEY='tronclass_timetable_templates_stable_v6';
  var SELECTED_KEY='tronclass_timetable_selected_account_stable_v6';
  var entries=[]; var statusCache=null; var mode='view'; var mounted=false; var reqSeq=0; var isLoading=false; var lastLoadedAccount='';

  function q(id){return document.getElementById(id)}
  function esc(v){return String(v==null?'':v).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]})}
  function textOf(el){return (el&&(el.textContent||el.value||'')||'').replace(/\s+/g,' ').trim()}
  function pStart(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][0]:''}
  function pEnd(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][1]:''}
  function token(){try{var u=new URLSearchParams(location.search).get('token');if(u)return String(u).trim();var keys=['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];for(var i=0;i<keys.length;i++){var v=localStorage.getItem(keys[i])||sessionStorage.getItem(keys[i]);if(v&&String(v).trim())return String(v).trim()}}catch(_){}return''}
  function withToken(path){var t=token();if(!t)return path;var u=new URL(path,location.origin);if(!u.searchParams.get('token'))u.searchParams.set('token',t);return u.pathname+u.search+u.hash}
  async function parseResponse(res){var b=await res.json().catch(function(){return{}});if(!res.ok){var e=new Error(b.error||b.message||('HTTP '+res.status));e.status=res.status;throw e}return b}
  async function api(path,opts){return fetch(withToken(path),opts||{}).then(parseResponse)}
  function hideEmptyToasts(){document.querySelectorAll('#toast,.toast').forEach(function(el){if(el.classList.contains('visible')&&!textOf(el)){el.classList.remove('visible');el.classList.add('tt-stable-empty-toast')}})}
  function toast(msg,ok){if(!String(msg||'').trim()){hideEmptyToasts();return}var el=q('toast')||document.querySelector('.toast');if(el){el.classList.remove('tt-stable-empty-toast');el.style.display='';el.textContent=msg;el.style.background=ok===false?'#991b1b':'#172033';el.classList.add('visible');clearTimeout(el._ttStableTimer);el._ttStableTimer=setTimeout(function(){el.classList.remove('visible')},2500)}else console.log(msg)}
  function setMsg(msg,ok){var el=q('ttStableMsg');if(el){el.textContent=msg||'';el.className='tt-stable-msg '+(ok===true?'ok':ok===false?'bad':'')}}
  async function getStatus(){if(statusCache)return statusCache;try{statusCache=await (window.TronclassRuntime&&window.TronclassRuntime.statusSummary?window.TronclassRuntime.statusSummary(false):api('/admin/api/status?include_history=false&history_limit=0',{cache:'no-store'}));return statusCache}catch(_){return null}}
  function normalizeDay(v){v=String(v||'').trim().toLowerCase();var map={'星期一':'mon','週一':'mon','禮拜一':'mon','一':'mon','1':'mon','mon':'mon','monday':'mon','星期二':'tue','週二':'tue','二':'tue','2':'tue','tue':'tue','tuesday':'tue','星期三':'wed','週三':'wed','三':'wed','3':'wed','wed':'wed','wednesday':'wed','星期四':'thu','週四':'thu','四':'thu','4':'thu','thu':'thu','thursday':'thu','星期五':'fri','週五':'fri','五':'fri','5':'fri','fri':'fri','friday':'fri','星期六':'sat','週六':'sat','六':'sat','6':'sat','sat':'sat','saturday':'sat','星期日':'sun','星期天':'sun','週日':'sun','日':'sun','天':'sun','7':'sun','sun':'sun','sunday':'sun'};return map[v]||v||'mon'}
  function normalize(e){var ps=Number(e.period_start||e.period||1)||1;var pe=Number(e.period_end||ps)||ps;if(pe<ps)pe=ps;return{weekday:normalizeDay(e.weekday||'mon'),period_start:ps,period_end:pe,start_time:e.start_time||pStart(ps),end_time:e.end_time||pEnd(pe),course_name:e.course_name||e.course||'',classroom:e.classroom||e.room||'',teacher:e.teacher||'',course_code:e.course_code||'',source:'manual'}}
  function selectedAccount(st){var saved='';try{saved=localStorage.getItem(SELECTED_KEY)||''}catch(_){}if(saved)return saved;if(st&&st.current_account_id&&st.current_account_id!=='<token-admin>')return st.current_account_id;if(st&&Array.isArray(st.accounts)&&st.accounts[0])return st.accounts[0].account_id;return''}
  async function fillAccounts(force){var st=await getStatus();var sel=q('ttStableAccount');if(!sel)return'';if(!force&&sel.dataset.filled==='1')return sel.value;var old=sel.value||selectedAccount(st);sel.innerHTML='';if(st&&st.is_admin&&Array.isArray(st.accounts)){st.accounts.forEach(function(a){if(a.account_id)sel.appendChild(new Option(accountLabel(a),a.account_id))});if(old&&Array.prototype.some.call(sel.options,function(o){return o.value===old}))sel.value=old;else if(sel.options.length)sel.selectedIndex=0;sel.disabled=false}else{var me=selectedAccount(st);sel.appendChild(new Option(accountLabel({account_id:me,user_name:st&&st.user_name}),me));sel.value=me;sel.disabled=true}sel.dataset.filled='1';return sel.value}
  function ensureView(){var view=q('view-timetable');if(!view){view=document.createElement('section');view.id='view-timetable';view.className='view';(document.querySelector('.protected')||document.querySelector('main')||document.body).appendChild(view)}return view}
  function activateTab(){window.__TT_STABLE_ACTIVE__=true;var tab=document.querySelector('.tab[data-view="timetable"],button[data-view="timetable"],[data-switch="timetable"]');if(tab&&!tab.dataset.ttStableClicked){try{tab.dataset.ttStableClicked='1';tab.click()}catch(_){}try{delete tab.dataset.ttStableClicked}catch(_){}}document.querySelectorAll('.view').forEach(function(v){v.classList.toggle('active',v.id==='view-timetable')});document.querySelectorAll('.tab').forEach(function(b){if(b.dataset)b.classList.toggle('active',b.dataset.view==='timetable')});document.querySelectorAll('.mobile-nav button[data-view]').forEach(function(b){b.classList.toggle('active',b.dataset.view==='timetable')});try{history.replaceState(null,'','#timetable')}catch(_){}}
  function mount(){var view=ensureView();if(mounted&&q('ttStablePanel'))return;view.innerHTML='<div id="ttStablePanel" class="tt-stable-panel tt-stable-mode-view"><div class="tt-stable-head"><div class="tt-stable-title"><h2 id="ttStableTitle">課表</h2><div id="ttStableSub" class="tt-stable-sub">查看課表，按「編輯課表」後可手動輸入。</div></div><div class="tt-stable-actions"><select id="ttStableAccount"></select><button id="ttStableReload" type="button">重新讀取</button><button id="ttStableEdit" type="button" class="primary">編輯課表</button></div></div><div id="ttStableView"></div><div id="ttStableEditor"></div><div id="ttStableMsg" class="tt-stable-msg"></div><input id="ttStableCsvFile" type="file" accept=".csv,text/csv" style="display:none"></div>';mounted=true;q('ttStableReload').onclick=function(e){e.preventDefault();reload(false,true)};q('ttStableEdit').onclick=function(e){e.preventDefault();openEdit()};q('ttStableAccount').onchange=function(){try{localStorage.setItem(SELECTED_KEY,q('ttStableAccount').value)}catch(_){}reload(mode==='edit',true)};q('ttStableCsvFile').onchange=handleCsvFile}
  async function loadEntries(){var seq=++reqSeq;var acct=await fillAccounts(false);lastLoadedAccount=acct;var panel=q('ttStablePanel');if(panel)panel.classList.add('tt-stable-loading');isLoading=true;setMsg('讀取課表中...');try{var body=await api('/admin/api/timetable'+(acct?'?account_id='+encodeURIComponent(acct):''),{cache:'no-store'});if(seq!==reqSeq)return null;entries=(body.entries||[]).map(normalize);setMsg('已顯示：'+(acct||'目前帳號')+' 的課表',true);return entries}catch(e){if(seq===reqSeq){setMsg('讀取課表失敗：'+(e.message||e),false);entries=[]}return null}finally{if(seq===reqSeq){isLoading=false;if(panel)panel.classList.remove('tt-stable-loading');hideEmptyToasts()}}}
  async function reload(keepEdit,userAction){if(isLoading)return;await fillAccounts(true);await loadEntries();if(keepEdit)renderEdit();else showView();if(userAction)toast('課表已重新讀取')}
  function ensurePosterCss(){
    var el=document.getElementById('TT_POSTER_VIEW_V1');
    var css='/* Weekly timetable grid — matches dashboard tokens, readable type. */\n.tt-poster-wrap{\n  background:var(--soft,#eef3fb);\n  border:1px solid var(--line,#dbe3ef);\n  border-radius:28px;\n  padding:26px 20px 28px;\n  overflow:auto;\n}\n.tt-poster-title{\n  margin:0 0 16px;\n  text-align:center;\n  font-size:32px;\n  font-weight:850;\n  letter-spacing:.04em;\n  color:var(--text,#172033);\n}\n.tt-poster{\n  display:grid;\n  min-width:680px;\n  min-height:680px;\n  gap:10px;\n  grid-template-rows:48px repeat(10,minmax(68px,1fr));\n}\n.tt-poster-corner{min-height:1px}\n.tt-poster-day{\n  display:flex;\n  align-items:flex-end;\n  justify-content:center;\n  padding-bottom:6px;\n  font-size:16px;\n  font-weight:850;\n  letter-spacing:.08em;\n  text-transform:uppercase;\n  color:var(--text,#172033);\n}\n.tt-poster-period{\n  display:flex;\n  flex-direction:column;\n  align-items:center;\n  justify-content:center;\n  gap:4px;\n  background:var(--muted,#64748b);\n  color:#fff;\n  border-radius:16px;\n  padding:8px 6px;\n  min-height:68px;\n}\n.tt-poster-period .n{\n  font-size:22px;\n  font-weight:900;\n  line-height:1;\n  color:#fff;\n}\n.tt-poster-period .t{\n  font-size:12px;\n  font-weight:750;\n  letter-spacing:0;\n  opacity:1;\n  white-space:nowrap;\n  color:#fff;\n}\n.tt-poster-empty,\n.tt-poster-course{\n  background:var(--card,#fff);\n  border-radius:16px;\n  min-height:68px;\n  box-shadow:0 1px 2px rgba(15,23,42,.04);\n  border:1px solid var(--line2,#e7ebf1);\n}\n.tt-poster-course{\n  appearance:none;\n  display:flex;\n  flex-direction:column;\n  align-items:center;\n  justify-content:center;\n  gap:6px;\n  padding:10px 12px;\n  width:100%;\n  min-width:0;\n  box-sizing:border-box;\n  cursor:pointer;\n  color:var(--text,#172033);\n  text-align:center;\n  overflow:hidden;\n}\n.tt-poster-course:hover{\n  border-color:var(--primary,#2563eb);\n  box-shadow:0 0 0 3px rgba(37,99,235,.12);\n}\n.tt-poster-name{\n  writing-mode:horizontal-tb;\n  text-orientation:mixed;\n  font-family:inherit;\n  font-size:17px;\n  font-weight:850;\n  letter-spacing:.01em;\n  line-height:1.35;\n  color:var(--text,#172033);\n}\n.tt-poster .tt-poster-name{\n  display:-webkit-box;\n  -webkit-box-orient:vertical;\n  -webkit-line-clamp:3;\n  overflow:hidden;\n  overflow-wrap:anywhere;\n  word-break:break-word;\n  max-width:100%;\n}\n.tt-poster-meta{\n  position:static;\n  right:auto;\n  top:auto;\n  transform:none;\n  writing-mode:horizontal-tb;\n  max-height:none;\n  overflow:hidden;\n  font-size:13px;\n  font-weight:750;\n  letter-spacing:0;\n  line-height:1.3;\n  color:var(--muted,#64748b);\n  pointer-events:none;\n}\n.tt-poster .tt-poster-meta{\n  text-overflow:ellipsis;\n  white-space:nowrap;\n  max-width:100%;\n}\n.tt-poster-empty-msg{\n  border:1px dashed var(--line,#dbe3ef);\n  border-radius:18px;\n  min-height:220px;\n  display:grid;\n  place-items:center;\n  text-align:center;\n  color:var(--muted,#64748b);\n  background:var(--soft2,#f8fafc);\n  padding:20px;\n  font-size:16px;\n}\n.tt-mobile-week{display:none}\n\n@media(max-width:820px){\n  #pageSub,\n  .tt-stable-sub,\n  .tt-poster-title{display:none!important}\n  #pageTitle,\n  .tt-stable-title h2{font-size:22px!important;letter-spacing:-.02em}\n  .tt-poster-wrap{\n    background:transparent;\n    border:0;\n    border-radius:0;\n    padding:0;\n    overflow:visible;\n  }\n  .tt-poster{display:none!important}\n  .tt-mobile-week{display:block}\n  .tt-mobile-days{\n    display:grid;\n    gap:8px;\n    margin:0 0 14px;\n    position:sticky;\n    top:0;\n    z-index:8;\n    padding:4px 0 12px;\n    background:var(--card,#fff);\n  }\n  .tt-mobile-day{\n    appearance:none;\n    min-height:46px;\n    border-radius:14px;\n    border:1.5px solid rgba(148,163,184,.38);\n    background:transparent;\n    color:var(--muted,#64748b);\n    font-size:16px;\n    font-weight:750;\n    display:inline-flex;\n    align-items:center;\n    justify-content:center;\n    gap:4px;\n    padding:0 6px;\n    cursor:pointer;\n  }\n  .tt-mobile-day .now{\n    font-size:10px;\n    font-weight:900;\n    letter-spacing:.04em;\n    padding:2px 5px;\n    border-radius:999px;\n    background:rgba(37,99,235,.14);\n    color:var(--primary,#2563eb);\n  }\n  .tt-mobile-day.is-today:not(.is-active){\n    color:var(--text,#172033);\n    border-color:rgba(37,99,235,.4);\n  }\n  .tt-mobile-day.is-active{\n    background:var(--primary,#2563eb);\n    border-color:var(--primary,#2563eb);\n    color:#fff;\n    font-weight:900;\n    box-shadow:0 8px 18px rgba(37,99,235,.38);\n  }\n  .tt-mobile-day.is-active .now{\n    background:rgba(255,255,255,.22);\n    color:#fff;\n  }\n  .tt-mobile-panel{display:none}\n  .tt-mobile-panel.is-active{display:block}\n  .tt-mobile-slot{\n    display:grid;\n    grid-template-columns:78px minmax(0,1fr);\n    gap:10px;\n    margin:0 0 10px;\n    align-items:stretch;\n  }\n  .tt-mobile-period{\n    display:flex;\n    flex-direction:column;\n    align-items:center;\n    justify-content:center;\n    gap:4px;\n    background:#64748b;\n    color:#fff;\n    border-radius:16px;\n    padding:10px 4px;\n    min-height:76px;\n    text-align:center;\n  }\n  .tt-mobile-period .n{\n    font-size:20px;\n    font-weight:900;\n    line-height:1.1;\n    color:#fff;\n  }\n  .tt-mobile-period .t{\n    font-size:11px;\n    font-weight:750;\n    line-height:1.25;\n    letter-spacing:0;\n    white-space:normal;\n    color:#fff;\n    opacity:.95;\n  }\n  .tt-mobile-course.tt-poster-course{\n    align-items:flex-start;\n    justify-content:center;\n    text-align:left;\n    gap:6px;\n    padding:14px 16px;\n    min-height:76px;\n    border-radius:18px;\n    overflow:visible;\n    box-shadow:0 8px 22px rgba(15,23,42,.08);\n  }\n  .tt-mobile-course .tt-poster-name{\n    display:block;\n    -webkit-line-clamp:unset;\n    overflow:visible;\n    font-size:18px;\n    font-weight:850;\n    line-height:1.45;\n    letter-spacing:.01em;\n    word-break:break-word;\n    overflow-wrap:anywhere;\n    max-width:100%;\n  }\n  .tt-mobile-course .tt-poster-meta{\n    display:block;\n    font-size:14px;\n    font-weight:750;\n    line-height:1.35;\n    white-space:normal;\n    overflow:visible;\n    color:var(--muted,#64748b);\n  }\n  .tt-mobile-empty{\n    display:flex;\n    align-items:center;\n    justify-content:center;\n    min-height:48px;\n    border-radius:16px;\n    border:1px dashed var(--line,#dbe3ef);\n    background:transparent;\n    color:var(--muted,#64748b);\n    font-size:13px;\n    font-weight:750;\n  }\n  .tt-mobile-slot.is-empty{\n    opacity:.72;\n  }\n  .tt-mobile-slot.is-empty .tt-mobile-period{\n    min-height:48px;\n    padding:8px 4px;\n  }\n  .tt-mobile-slot.is-empty .tt-mobile-period .n{font-size:16px}\n  .tt-mobile-empty-day{\n    border:1px dashed var(--line,#dbe3ef);\n    border-radius:18px;\n    min-height:140px;\n    display:grid;\n    place-items:center;\n    color:var(--muted,#64748b);\n    font-size:16px;\n    font-weight:750;\n    padding:20px;\n  }\n  @media(prefers-color-scheme:dark){\n    .tt-mobile-days{background:var(--card,#182232)}\n    .tt-mobile-day:not(.is-active){background:transparent;border-color:#3d4d63;color:#94a3b8}\n    .tt-mobile-day.is-active{background:#3b82f6;border-color:#60a5fa;color:#fff}\n    .tt-mobile-period{background:#334155}\n    .tt-mobile-course.tt-poster-course{box-shadow:0 10px 24px rgba(0,0,0,.28)}\n  }\n  html[data-theme="dark"] .tt-mobile-days{background:var(--card,#182232)}\n  html[data-theme="dark"] .tt-mobile-day:not(.is-active){background:transparent;border-color:#3d4d63;color:#94a3b8}\n  html[data-theme="dark"] .tt-mobile-day.is-active{background:#3b82f6;border-color:#60a5fa;color:#fff}\n  html[data-theme="dark"] .tt-mobile-period{background:#334155}\n}\n\n@media(prefers-color-scheme:dark){\n  .tt-poster-period{background:#334155;color:#f1f5f9}\n  .tt-poster-period .n,.tt-poster-period .t{color:#f1f5f9}\n  .tt-poster-empty,.tt-poster-course{\n    background:var(--card,#182232);\n    border-color:var(--line,#334155);\n  }\n}\nhtml[data-theme="dark"] .tt-poster-period{background:#334155}\nhtml[data-theme="dark"] .tt-poster-period .n,\nhtml[data-theme="dark"] .tt-poster-period .t{color:#f1f5f9}\n';
    if(el){ el.textContent=css; return; }
    var s=document.createElement('style');
    s.id='TT_POSTER_VIEW_V1';
    s.textContent=css;
    document.head.appendChild(s);
  }
  function posterDays(real){
    var days=[['mon','MON.'],['tue','TUE.'],['wed','WED.'],['thu','THU.'],['fri','FRI.']];
    if(real.some(function(e){return e.weekday==='sat'})) days.push(['sat','SAT.']);
    if(real.some(function(e){return e.weekday==='sun'})) days.push(['sun','SUN.']);
    return days;
  }
      function accountLabel(a){
    var id=String((a&&a.account_id)||'').trim();
    var name=String((a&&(a.user_name||a.username))||'').trim();
    if(name && id && name!==id) return name;
    return name||id||'我的課表';
  }
  function paintMobileDay(btn, on){
    if(!btn) return;
    btn.classList.toggle('is-active', !!on);
    btn.setAttribute('aria-selected', on ? 'true' : 'false');
    if(on){
      btn.style.setProperty('background', '#2563eb', 'important');
      btn.style.setProperty('color', '#fff', 'important');
      btn.style.setProperty('border', '0', 'important');
      btn.style.setProperty('box-shadow', '0 8px 18px rgba(37,99,235,.4)', 'important');
    }else{
      btn.style.setProperty('background', '#4b5563', 'important');
      btn.style.setProperty('color', '#e5e7eb', 'important');
      btn.style.setProperty('border', '0', 'important');
      btn.style.setProperty('box-shadow', 'none', 'important');
    }
  }
  function bindMobileWeek(){
    if(window.TT_MOBILE_WEEK_BOUND) return;
    window.TT_MOBILE_WEEK_BOUND = true;
    document.addEventListener('click', function(ev){
      var btn = ev.target.closest && ev.target.closest('.tt-mobile-day');
      if(!btn) return;
      var wrap = btn.closest('.tt-mobile-week');
      if(!wrap) return;
      var day = btn.getAttribute('data-day');
      wrap.querySelectorAll('.tt-mobile-day').forEach(function(b){ paintMobileDay(b, b===btn); });
      wrap.querySelectorAll('.tt-mobile-panel').forEach(function(p){
        p.classList.toggle('is-active', p.getAttribute('data-day')===day);
      });
    }, true);
  }
  function shortZhDay(key){
    return {mon:'一',tue:'二',wed:'三',thu:'四',fri:'五',sat:'六',sun:'日'}[key]||key;
  }
  function todayWeekday(){
    return ['sun','mon','tue','wed','thu','fri','sat'][new Date().getDay()];
  }
  function renderPoster(real){
    ensurePosterCss();
    var days=posterDays(real);
    var occupied=Object.create(null);
    var blocks=[];
    real.slice().sort(function(a,b){
      var order=['mon','tue','wed','thu','fri','sat','sun'];
      var da=order.indexOf(a.weekday)-order.indexOf(b.weekday);
      if(da) return da;
      return Number(a.period_start||0)-Number(b.period_start||0);
    }).forEach(function(e){
      var ps=Math.max(1,Math.min(10,Number(e.period_start||1)||1));
      var pe=Math.max(ps,Math.min(10,Number(e.period_end||ps)||ps));
      var clash=false;
      for(var p=ps;p<=pe;p++) if(occupied[e.weekday+':'+p]) clash=true;
      if(clash) return;
      for(var p=ps;p<=pe;p++) occupied[e.weekday+':'+p]=true;
      blocks.push({day:e.weekday,ps:ps,pe:pe,e:e});
    });
    var html='<div class="tt-poster-wrap"><div class="tt-poster-title">課表</div><div class="tt-poster" style="grid-template-columns:92px repeat('+days.length+',minmax(0,1fr))">';
    html+='<div class="tt-poster-corner" style="grid-column:1;grid-row:1"></div>';
    days.forEach(function(d,i){ html+='<div class="tt-poster-day" style="grid-column:'+(i+2)+';grid-row:1">'+d[1]+'</div>'; });
    for(var p=1;p<=10;p++){
      var tm=PERIOD[p]||['',''];
      html+='<div class="tt-poster-period" style="grid-column:1;grid-row:'+(p+1)+'"><span class="n">'+p+'</span><span class="t">'+esc(tm[0]+'–'+tm[1])+'</span></div>';
      days.forEach(function(d,i){
        var start=null;
        for(var b=0;b<blocks.length;b++) if(blocks[b].day===d[0] && blocks[b].ps===p){ start=blocks[b]; break; }
        if(start){
          var place=String(start.e.classroom||'').trim();
          var teacher=String(start.e.teacher||'').trim();
          var meta=place||teacher;
          html+='<button type="button" class="tt-poster-course" data-course="'+esc(start.e.course_name)+'" title="'+esc((start.e.course_name||'')+(place?' ｜ '+place:'')+(teacher?' ｜ '+teacher:''))+'" style="grid-column:'+(i+2)+';grid-row:'+(p+1)+' / span '+(start.pe-start.ps+1)+'"><span class="tt-poster-name">'+esc(start.e.course_name)+'</span>'+(meta?'<span class="tt-poster-meta">'+esc(meta)+'</span>':'')+'</button>';
        }else if(!occupied[d[0]+':'+p]){
          html+='<div class="tt-poster-empty" style="grid-column:'+(i+2)+';grid-row:'+(p+1)+'"></div>';
        }
      });
    }
    html+='</div>';
    var today=todayWeekday();
    var defaultDay=days.some(function(d){return d[0]===today}) ? today : days[0][0];
    html+='<div class="tt-mobile-week"><div class="tt-mobile-days" role="tablist" style="grid-template-columns:repeat('+days.length+',minmax(0,1fr))">';
    days.forEach(function(d){
      var on=d[0]===defaultDay;
      html+='<button type="button" class="tt-mobile-day'+(on?' is-active':'')+(d[0]===today?' is-today':'')+'" data-day="'+d[0]+'" role="tab" aria-selected="'+(on?'true':'false')+'" style="'+(on?'background:#2563eb;color:#fff;border:0;box-shadow:0 8px 18px rgba(37,99,235,.4)':'background:#4b5563;color:#e5e7eb;border:0;box-shadow:none')+'">'+shortZhDay(d[0])+(d[0]===today?'<span class="now">今</span>':'')+'</button>';
    });
    html+='</div>';
    days.forEach(function(d){
      html+='<div class="tt-mobile-panel'+(d[0]===defaultDay?' is-active':'')+'" data-day="'+d[0]+'" role="tabpanel">';
      var slots=[];
      for(var p=1;p<=10;p++){
        var start=null;
        for(var b=0;b<blocks.length;b++) if(blocks[b].day===d[0] && blocks[b].ps===p){ start=blocks[b]; break; }
        if(start) slots.push({kind:'course',start:start});
        else if(!occupied[d[0]+':'+p]) slots.push({kind:'empty',p:p});
      }
      while(slots.length && slots[0].kind==='empty') slots.shift();
      while(slots.length && slots[slots.length-1].kind==='empty') slots.pop();
      if(!slots.length){
        html+='<div class="tt-mobile-empty-day">這天沒有課程</div>';
      }else{
        slots.forEach(function(slot){
          if(slot.kind==='course'){
            var start=slot.start;
            var place=String(start.e.classroom||'').trim();
            var teacher=String(start.e.teacher||'').trim();
            var meta=place||teacher;
            var nLab=start.ps===start.pe?String(start.ps):(start.ps+'–'+start.pe);
            var t0=(PERIOD[start.ps]||['',''])[0];
            var t1=(PERIOD[start.pe]||['',''])[1];
            html+='<div class="tt-mobile-slot"><div class="tt-mobile-period"><span class="n">'+esc(nLab)+'</span><span class="t">'+esc(t0)+'<br>'+esc(t1)+'</span></div><button type="button" class="tt-poster-course tt-mobile-course" data-course="'+esc(start.e.course_name)+'" title="'+esc((start.e.course_name||'')+(place?' ｜ '+place:'')+(teacher?' ｜ '+teacher:''))+'"><span class="tt-poster-name">'+esc(start.e.course_name)+'</span>'+(meta?'<span class="tt-poster-meta">'+esc(meta)+'</span>':'')+'</button></div>';
          }else{
            var tm=PERIOD[slot.p]||['',''];
            html+='<div class="tt-mobile-slot is-empty"><div class="tt-mobile-period"><span class="n">'+slot.p+'</span><span class="t">'+esc(tm[0])+'<br>'+esc(tm[1])+'</span></div><div class="tt-mobile-empty">空堂</div></div>';
          }
        });
      }
      html+='</div>';
    });
    html+='</div></div>';
    bindMobileWeek();
    return html;
  }
  function showView(){mode='view';var panel=q('ttStablePanel');if(panel){panel.classList.remove('tt-stable-mode-edit');panel.classList.add('tt-stable-mode-view')}q('ttStableTitle').textContent='課表';q('ttStableSub').textContent='查看課表，按「編輯課表」後可手動輸入。';var real=entries.filter(function(e){return String(e.course_name||'').trim()});q('ttStableView').innerHTML=real.length?renderPoster(real):'<div class="tt-poster-empty-msg">目前沒有課表資料，請按「編輯課表」新增。</div>';bindMobileWeek();}
  function templates(){try{return JSON.parse(localStorage.getItem(TEMPLATE_KEY)||'{}')}catch(_){return{}}}function saveTemplates(t){localStorage.setItem(TEMPLATE_KEY,JSON.stringify(t||{}))}
  function templateTools(){var t=templates(),names=Object.keys(t).sort();return'<div class="tt-stable-template"><select id="ttStableTemplate"><option value="">模板課表</option>'+names.map(function(n){return'<option value="'+esc(n)+'">'+esc(n)+'</option>'}).join('')+'</select><button id="ttStableTplApply" type="button">套用模板</button><button id="ttStableTplSave" type="button">另存模板</button><button id="ttStableTplDel" type="button">刪除模板</button><button id="ttStableCsvImport" type="button">匯入 CSV</button><button id="ttStableCsvSample" type="button">下載CSV範本</button></div>'}
  function rowHtml(e,i){var ps=Number(e.period_start||1),pe=Number(e.period_end||ps);return'<div class="tt-stable-row" data-i="'+i+'"><select class="tt-day">'+DAYS.map(function(d){return'<option value="'+d[0]+'" '+(e.weekday===d[0]?'selected':'')+'>'+d[1]+'</option>'}).join('')+'</select><input class="tt-ps" type="number" min="1" max="10" value="'+esc(ps)+'"><input class="tt-pe" type="number" min="1" max="10" value="'+esc(pe)+'"><input class="tt-course tt-stable-course" placeholder="課程" value="'+esc(e.course_name||'')+'"><input class="tt-room tt-stable-room" placeholder="教室" value="'+esc(e.classroom||'')+'"><input class="tt-teacher tt-stable-teacher" placeholder="老師" value="'+esc(e.teacher||'')+'"><div class="tt-stable-time">'+esc((e.start_time||pStart(ps)||'-')+' - '+(e.end_time||pEnd(pe)||'-'))+'</div><button type="button" class="tt-stable-del">刪除</button></div>'}
  function syncRow(row){var i=Number(row.dataset.i),ps=Math.max(1,Math.min(10,Number(row.querySelector('.tt-ps').value||1))),pe=Math.max(ps,Math.min(10,Number(row.querySelector('.tt-pe').value||ps)));row.querySelector('.tt-ps').value=ps;row.querySelector('.tt-pe').value=pe;entries[i]=normalize({weekday:row.querySelector('.tt-day').value,period_start:ps,period_end:pe,course_name:row.querySelector('.tt-course').value,classroom:row.querySelector('.tt-room').value,teacher:row.querySelector('.tt-teacher').value});row.querySelector('.tt-stable-time').textContent=(pStart(ps)||'-')+' - '+(pEnd(pe)||'-')}
  function bindRows(){document.querySelectorAll('#ttStableRows .tt-stable-row').forEach(function(row){row.querySelectorAll('input,select').forEach(function(el){el.oninput=function(){syncRow(row)};el.onchange=function(){syncRow(row)}});row.querySelector('.tt-stable-del').onclick=function(){entries.splice(Number(row.dataset.i),1);renderEdit()}})}
  function renderEdit(){mode='edit';var panel=q('ttStablePanel');if(panel){panel.classList.remove('tt-stable-mode-view');panel.classList.add('tt-stable-mode-edit')}q('ttStableTitle').textContent='編輯課表';q('ttStableSub').textContent='只要填第幾節，時間會自動帶入。可匯入 CSV、套用模板或自己新增課程。';q('ttStableEditor').innerHTML=templateTools()+'<div class="tt-stable-edit-actions"><button id="ttStableAdd" type="button">新增一堂課</button><button id="ttStableSave" type="button" class="primary">儲存課表</button><button id="ttStableReloadEdit" type="button">重新讀取</button><button id="ttStableBack" type="button">返回查看</button></div><div id="ttStableRows">'+entries.map(rowHtml).join('')+'</div>';q('ttStableAdd').onclick=function(){entries.push(normalize({weekday:'mon',period_start:1,period_end:1}));renderEdit()};q('ttStableSave').onclick=saveEntries;q('ttStableReloadEdit').onclick=function(){reload(true,true)};q('ttStableBack').onclick=showView;q('ttStableCsvImport').onclick=function(){q('ttStableCsvFile').click()};q('ttStableCsvSample').onclick=downloadCsvSample;q('ttStableTplSave').onclick=saveAsTemplate;q('ttStableTplApply').onclick=applyTemplate;q('ttStableTplDel').onclick=deleteTemplate;bindRows()}
  async function openEdit(){if(!entries.length)await loadEntries();if(!entries.length)entries=[normalize({weekday:'mon',period_start:1,period_end:1})];renderEdit();toast('已進入課表編輯模式')}
  async function saveEntries(){document.querySelectorAll('#ttStableRows .tt-stable-row').forEach(syncRow);var valid=entries.filter(function(e){return String(e.course_name||'').trim()});if(!valid.length){setMsg('請至少輸入一堂課的課程名稱。',false);return}setMsg('儲存中...');try{await api('/admin/api/timetable/import',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({account_id:q('ttStableAccount').value||undefined,entries:valid})});entries=valid;setMsg('課表已儲存。',true);toast('課表已儲存');showView()}catch(e){setMsg(e.message||'儲存課表失敗',false);toast(e.message||'儲存課表失敗',false)}}
  function parseCsv(text){var rows=[],row=[],cur='',quote=false;for(var i=0;i<text.length;i++){var ch=text[i],nx=text[i+1];if(ch==='"'){if(quote&&nx==='"'){cur+='"';i++}else quote=!quote}else if(ch===','&&!quote){row.push(cur);cur=''}else if((ch==='\n'||ch==='\r')&&!quote){if(ch==='\r'&&nx==='\n')i++;row.push(cur);if(row.some(function(x){return String(x).trim()}))rows.push(row);row=[];cur=''}else cur+=ch}row.push(cur);if(row.some(function(x){return String(x).trim()}))rows.push(row);return rows}
  function parsePeriod(v){var m=String(v||'').match(/(\d+)/g)||[];return[Number(m[0]||1),Number(m[1]||m[0]||1)]}
  function csvEntries(text){var rows=parseCsv(text);if(!rows.length)return[];var header=rows[0].map(function(x){return String(x||'').trim().toLowerCase()});var has=header.some(function(h){return /星期|weekday|day|課程|course|節|period|教室|room|老師|teacher/.test(h)});var data=has?rows.slice(1):rows;function val(r,names,idx){if(has){for(var i=0;i<names.length;i++){var j=header.indexOf(names[i]);if(j>=0)return r[j]||''}}return r[idx]||''}return data.map(function(r){var day=val(r,['星期','weekday','day'],0),ps=val(r,['開始節次','period_start','start_period'],1),pe=val(r,['結束節次','period_end','end_period'],2),period=val(r,['節次','period','節'],1),course=val(r,['課程','course','course_name'],3),room=val(r,['教室','room','classroom'],4),teacher=val(r,['老師','teacher'],5);if(!ps){var pr=parsePeriod(period);ps=pr[0];pe=pe||pr[1]}if(!pe)pe=ps;return normalize({weekday:day,period_start:ps,period_end:pe,course_name:course,classroom:room,teacher:teacher})}).filter(function(e){return e.course_name||e.classroom||e.teacher})}
  function handleCsvFile(ev){var f=ev.target.files&&ev.target.files[0];if(!f)return;var rd=new FileReader();rd.onload=function(){var im=csvEntries(String(rd.result||''));if(!im.length){toast('CSV 沒有可匯入的課程',false);return}entries=entries.filter(function(e){return String(e.course_name||'').trim()}).concat(im);renderEdit();toast('已匯入 '+im.length+' 堂課')};rd.readAsText(f,'utf-8');ev.target.value=''}
  function downloadCsvSample(){var csv='星期,開始節次,結束節次,課程,教室,老師\n星期一,1,2,課程名稱,E304,老師姓名\n';var blob=new Blob([csv],{type:'text/csv;charset=utf-8'});var a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download='timetable_template.csv';a.click();setTimeout(function(){URL.revokeObjectURL(a.href)},1000)}
  function saveAsTemplate(){document.querySelectorAll('#ttStableRows .tt-stable-row').forEach(syncRow);var name=prompt('模板名稱：');if(!name)return;var t=templates();t[name]=entries.filter(function(e){return String(e.course_name||'').trim()});saveTemplates(t);renderEdit();toast('模板已儲存')}
  function applyTemplate(){var name=q('ttStableTemplate').value;if(!name)return toast('請先選擇模板',false);var t=templates();entries=(t[name]||[]).map(normalize);renderEdit();toast('已套用模板')}
  function deleteTemplate(){var name=q('ttStableTemplate').value;if(!name)return toast('請先選擇模板',false);if(!confirm('刪除模板：'+name+'？'))return;var t=templates();delete t[name];saveTemplates(t);renderEdit();toast('模板已刪除')}
  function templates(){try{return JSON.parse(localStorage.getItem(TEMPLATE_KEY)||'{}')}catch(_){return{}}}function saveTemplates(t){localStorage.setItem(TEMPLATE_KEY,JSON.stringify(t||{}))}
  async function bootTimetable(){activateTab();mount();await fillAccounts(true);if(lastLoadedAccount!==q('ttStableAccount').value)await loadEntries();showView()}
  function isTimetableNav(n){return n&&n.dataset&&(n.dataset.view==='timetable'||n.dataset.switch==='timetable')}
  document.addEventListener('click',function(ev){var n=ev.target.closest&&ev.target.closest('[data-view],[data-switch]');if(isTimetableNav(n)){ev.preventDefault();ev.stopPropagation();if(ev.stopImmediatePropagation)ev.stopImmediatePropagation();setTimeout(bootTimetable,50)}},true);
  window.addEventListener('hashchange',function(){if(location.hash==='#timetable')bootTimetable();else window.__TT_STABLE_ACTIVE__=false});
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',function(){hideEmptyToasts();if(location.hash==='#timetable'||q('view-timetable')&&q('view-timetable').classList.contains('active'))bootTimetable()});else{hideEmptyToasts();if(location.hash==='#timetable'||q('view-timetable')&&q('view-timetable').classList.contains('active'))bootTimetable()}
})();

/* timetableCourseRollcallHistoryLinkScriptV1 */
(function(){
  'use strict';
  if(window.TIMETABLE_COURSE_ROLLCALL_HISTORY_LINK_V1) return;
  window.TIMETABLE_COURSE_ROLLCALL_HISTORY_LINK_V1 = true;

  var FILTER_KEY = 'tronclass_rollcall_history_course_filter_v1';
  var TOKEN_KEYS = ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
  var applying = false;
  var lastStatus = null;

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];}); }
  function textOf(el){ return (el && (el.textContent || el.value || '') || '').replace(/\s+/g,' ').trim(); }
  function norm(v){ return String(v || '').replace(/\s+/g,'').toLowerCase(); }
  function token(){
    try{
      var u = new URLSearchParams(location.search).get('token');
      if(u) return String(u).trim();
      for(var i=0;i<TOKEN_KEYS.length;i++){
        var v = localStorage.getItem(TOKEN_KEYS[i]) || sessionStorage.getItem(TOKEN_KEYS[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){ }
    return '';
  }
  function withToken(path){
    var t = token();
    if(!t) return path;
    var u = new URL(path, location.origin);
    if(!u.searchParams.get('token')) u.searchParams.set('token', t);
    return u.pathname + u.search + u.hash;
  }
  async function parseResponse(res){
    var body = await res.json().catch(function(){ return {}; });
    if(!res.ok){ var e = new Error(body.error || body.message || ('HTTP '+res.status)); e.status = res.status; throw e; }
    return body;
  }
  async function api(path, opts){ return fetch(withToken(path), opts || {}).then(parseResponse); }
  async function loadStatus(){
    if(window.status && typeof window.status === 'object' && Array.isArray(window.status.accounts)){
      lastStatus = window.status;
      return lastStatus;
    }
    if(lastStatus) return lastStatus;
    lastStatus = await (window.TronclassRuntime&&window.TronclassRuntime.statusSummary?window.TronclassRuntime.statusSummary(false):api('/admin/api/status?include_history=false&history_limit=0', {cache:'no-store'}));
    return lastStatus;
  }
  function selectedAccount(status){
    var ids = ['accountSelect','ttStableAccount','ttClean5Account','ttCleanAccount','ttFullAccount','ttRestoredAccount','ttAccount','timetableAccount'];
    for(var i=0;i<ids.length;i++){
      var sel = q(ids[i]);
      if(sel && sel.value) return sel.value;
    }
    try{
      var saved = localStorage.getItem('tronclass_timetable_selected_account_standalone_v2') || localStorage.getItem('tronclass_timetable_selected_account_clean_v5') || localStorage.getItem('tronclass_timetable_selected_account_v1') || '';
      if(saved) return saved;
    }catch(_){ }
    if(status && status.current_account_id && status.current_account_id !== '<token-admin>') return status.current_account_id;
    if(status && Array.isArray(status.accounts) && status.accounts[0]) return status.accounts[0].account_id;
    return '';
  }
  function accountById(status, accountId){
    var list = status && status.accounts || [];
    if(accountId){
      for(var i=0;i<list.length;i++) if(list[i].account_id === accountId) return list[i];
    }
    return list[0] || null;
  }
  function fmt(ts){
    if(!ts) return '時間未知';
    try{ return new Date(Number(ts)*1000).toLocaleString(); }catch(_){ return String(ts); }
  }
  function ensureModal(){
    var modal = q('ttCourseHistoryModal');
    if(modal) return modal;
    modal = document.createElement('div');
    modal.id = 'ttCourseHistoryModal';
    modal.className = 'tt-history-modal-backdrop';
    modal.innerHTML = '<div class="tt-history-modal"><div class="tt-history-modal-head"><div><h2 id="ttHistoryTitle">課程點名紀錄</h2><div id="ttHistoryMeta" class="tt-history-meta"></div></div><button id="ttHistoryClose" class="tt-history-close" type="button">關閉</button></div><div id="ttHistoryBody" class="tt-history-list"></div><div class="tt-history-actions"><a id="ttHistoryOpenRollcall" href="/admin#rollcall">前往點名分頁</a><button id="ttHistoryReload" type="button">重新整理紀錄</button></div></div>';
    document.body.appendChild(modal);
    q('ttHistoryClose').onclick = function(){ modal.classList.remove('visible'); };
    modal.addEventListener('click', function(ev){ if(ev.target === modal) modal.classList.remove('visible'); });
    return modal;
  }
  function courseMatches(historyCourse, course){
    var a = norm(historyCourse);
    var b = norm(course);
    if(!a || !b) return false;
    return a === b || a.indexOf(b) >= 0 || b.indexOf(a) >= 0;
  }
  function historyHtml(items){
    if(!items.length){
      return '<div class="tt-history-empty">目前沒有找到這堂課的歷史點名紀錄。<br>如果這堂課還沒有發生過點名，這是正常的。</div>';
    }
    return items.map(function(h){
      var ok = !!h.success;
      return '<div class="tt-history-item"><div class="tt-history-item-top"><span>'+esc(h.course_name || '-')+'</span><span class="tt-history-pill '+(ok?'ok':'bad')+'">'+(ok?'成功':'未成功')+'</span></div><div class="tt-history-detail">時間：'+esc(fmt(h.timestamp))+'\n點名 ID：'+esc(h.rollcall_id || '-')+'\n類型：'+esc(h.attendance_type || '-')+'\n結果：'+esc(h.result || '-')+'</div></div>';
    }).join('');
  }
  async function showCourseHistory(course, accountId){
    var modal = ensureModal();
    q('ttHistoryTitle').textContent = course + '｜點名紀錄';
    q('ttHistoryMeta').textContent = '讀取中...';
    q('ttHistoryBody').innerHTML = '<div class="tt-history-empty">讀取點名紀錄中...</div>';
    modal.classList.add('visible');
    try{
      lastStatus = null;
      var status = await loadStatus();
      var acctId = accountId || selectedAccount(status);
      var account = accountById(status, acctId);
      var list = (account && account.rollcall_history || []).filter(function(h){ return courseMatches(h.course_name, course); }).sort(function(a,b){ return Number(b.timestamp||0)-Number(a.timestamp||0); });
      q('ttHistoryMeta').textContent = '帳號：' + (account ? account.account_id : (acctId || '-')) + '｜課程：' + course + '｜共 ' + list.length + ' 筆';
      q('ttHistoryBody').innerHTML = historyHtml(list);
      var payload = {course:course, account_id:acctId || '', ts:Date.now()};
      try{ localStorage.setItem(FILTER_KEY, JSON.stringify(payload)); }catch(_){ }
      q('ttHistoryOpenRollcall').href = withToken('/admin#rollcall');
      q('ttHistoryReload').onclick = function(){ lastStatus = null; showCourseHistory(course, acctId); };
    }catch(e){
      q('ttHistoryMeta').textContent = '讀取失敗';
      q('ttHistoryBody').innerHTML = '<div class="tt-history-empty">無法讀取歷史點名紀錄：'+esc(e.message || e)+'</div>';
    }
  }
  function isLikelyTimetableTable(table){
    if(!table) return false;
    var txt = textOf(table.querySelector('thead') || table).toLowerCase();
    return txt.indexOf('課程') >= 0 && (txt.indexOf('節次') >= 0 || txt.indexOf('時間') >= 0 || txt.indexOf('星期') >= 0);
  }
  function enhanceTable(table){
    if(!isLikelyTimetableTable(table)) return;
    var rows = table.querySelectorAll('tbody tr');
    Array.prototype.forEach.call(rows, function(row){
      if(row.querySelector('input,select,textarea')) return;
      var cells = row.children || [];
      if(cells.length < 4) return;
      var cell = cells[cells.length - 1];
      var course = textOf(cell);
      if(!course || course === '-' || cell.dataset.ttHistoryLinked === '1') return;
      cell.dataset.ttHistoryLinked = '1';
      cell.innerHTML = '<button type="button" class="tt-course-history-link" data-course="'+esc(course)+'">'+esc(course)+'</button>';
      var btn = cell.querySelector('button');
      btn.onclick = function(ev){
        ev.preventDefault();
        ev.stopPropagation();
        if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
        showCourseHistory(course);
      };
    });
  }
  function enhanceCourses(){
    var scopes = [];
    var tv = q('view-timetable');
    if(tv) scopes.push(tv);
    if(location.pathname.indexOf('/admin/timetable') === 0) scopes.push(document);
    if(!scopes.length) return;
    scopes.forEach(function(scope){
      Array.prototype.forEach.call(scope.querySelectorAll('table'), enhanceTable);
    });
  }
  function renderDashboardFilterPanel(){
    if(location.pathname !== '/admin' && location.pathname !== '/admin/' && location.pathname !== '/admin/dashboard') return;
    var raw = '';
    try{ raw = localStorage.getItem(FILTER_KEY) || ''; }catch(_){ }
    if(!raw) return;
    var payload = null;
    try{ payload = JSON.parse(raw); }catch(_){ }
    if(!payload || !payload.course) return;
    if(location.hash !== '#rollcall') return;
    showCourseHistory(payload.course, payload.account_id);
  }

  function start(){
    enhanceCourses();
    renderDashboardFilterPanel();
    var timer = null;
    try{
      new MutationObserver(function(){
        clearTimeout(timer);
        timer = setTimeout(enhanceCourses, 120);
      }).observe(document.documentElement, {childList:true, subtree:true});
    }catch(_){ }
    document.addEventListener('visibilitychange', function(){ if(!document.hidden) setTimeout(enhanceCourses, 100); });
    window.addEventListener('hashchange', function(){ setTimeout(function(){ enhanceCourses(); renderDashboardFilterPanel(); }, 120); });
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();

/* courseHistoryModalCloseOnJumpV1 */
(function(){
  'use strict';
  if(window.COURSE_HISTORY_MODAL_CLOSE_ON_JUMP_V1) return;
  window.COURSE_HISTORY_MODAL_CLOSE_ON_JUMP_V1 = true;

  var FILTER_KEY = 'tronclass_rollcall_history_course_filter_v1';

  function q(id){ return document.getElementById(id); }
  function textOf(el){ return (el && (el.textContent || el.value || '') || '').replace(/\s+/g,' ').trim(); }
  function closeHistoryModal(){
    var modal = q('ttCourseHistoryModal');
    if(modal){
      modal.classList.remove('visible','open','show','active');
      modal.style.display = 'none';
    }
    document.querySelectorAll('.tt-history-modal-backdrop.visible,.tt-history-modal-backdrop.open,.tt-history-modal-backdrop.show').forEach(function(el){
      el.classList.remove('visible','open','show','active');
      el.style.display = 'none';
    });
    try{ localStorage.removeItem(FILTER_KEY); }catch(_){ }
  }
  function isJumpToRollcall(el){
    if(!el) return false;
    if(el.id === 'ttHistoryOpenRollcall') return true;
    var href = '';
    try{ href = el.getAttribute && (el.getAttribute('href') || ''); }catch(_){ }
    if(href.indexOf('#rollcall') >= 0) return true;
    return textOf(el).indexOf('前往點名分頁') >= 0 || textOf(el).indexOf('點名分頁') >= 0;
  }
  function targetFrom(ev){
    if(!ev || !ev.target || !ev.target.closest) return null;
    return ev.target.closest('#ttHistoryOpenRollcall,a,button,[role="button"]');
  }

  document.addEventListener('click', function(ev){
    var el = targetFrom(ev);
    if(!isJumpToRollcall(el)) return;

    var href = '';
    try{ href = el.getAttribute && el.getAttribute('href'); }catch(_){ }
    if(!href || href === '#') href = '/admin#rollcall';

    ev.preventDefault();
    ev.stopPropagation();
    if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();

    closeHistoryModal();

    setTimeout(function(){
      try{
        location.href = href;
      }catch(_){
        location.hash = '#rollcall';
      }
    }, 40);
  }, true);

  window.addEventListener('hashchange', function(){
    if(location.hash === '#rollcall') closeHistoryModal();
  });
})();

  }
  if(window.TronclassDashboard&&window.TronclassDashboard.ready) start();
  else window.addEventListener('tronclass:dashboard:ready',start,{once:true});
})();
