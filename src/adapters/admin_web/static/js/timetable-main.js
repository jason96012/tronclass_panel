/* INLINE_SPLIT_V1：timetable-main.js 原本內嵌於 timetable.html 的 <body> 全部內聯 <script>（含 TT_WALLPAPER_V1 桌布匯出），抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: consolidatedAdminAuthBodyV1 (原 timetable.html:825) ── */
(function(){
  'use strict';
  if (window.CONSOLIDATED_ADMIN_AUTH_V1) return;
  window.CONSOLIDATED_ADMIN_AUTH_V1 = true;

  var LS_SESSION = 'tronclass_admin_session';
  var LS_SAVED_USER = 'tronclass_admin_saved_username_v1';
  var LS_LOGOUT = 'tronclass_dashboard_manual_logout';
  var nativeFetch = window.fetch.bind(window);

  function q(id){ return document.getElementById(id); }
  function params(){ try { return new URLSearchParams(location.search); } catch(_) { return new URLSearchParams(); } }
  function token(){try{if(window.__tronBootToken)return String(window.__tronBootToken).trim();
      var u = params().get('token');
      if (u) return String(u).trim();
      var keys = ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
      for(var i=0;i<keys.length;i++){
        var v = localStorage.getItem(keys[i]) || sessionStorage.getItem(keys[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){}
    return '';
  }
  function isLoginPage(){ return location.pathname === '/admin' || location.pathname === '/admin/' || location.pathname === '/admin/dashboard'; }
  function isAdminPath(){ return location.pathname === '/admin' || location.pathname.startsWith('/admin/'); }
  function currentPath(){ return location.pathname + location.search + location.hash; }
  function loginUrl(){ var u = new URL('/admin', location.origin); var cur = currentPath(); if (cur && cur !== '/admin' && cur.indexOf('/admin?logout=1') !== 0) u.searchParams.set('next', cur); u.searchParams.set('v','auth'+Date.now()); return u.pathname + u.search; }
  function withToken(path){ /* TOKEN_STORAGE_PURGE_V1: cookie-only（SECURITY-F03）——不再寫 token 進 storage，改清除遺留 key */ return path; }
  function setClass(state){
    document.documentElement.classList.remove('tron-auth-pending','tron-auth-ok','tron-auth-logged-out');
    if (document.body) document.body.classList.remove('auth-ok','dash-auth-ok','logged-in');
    if (state === 'ok') {
      document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','mobile-nav-final-auth');
      if (document.body) document.body.classList.add('auth-ok','dash-auth-ok','logged-in');
    } else if (state === 'pending') {
      document.documentElement.classList.add('tron-auth-pending');
    } else {
      document.documentElement.classList.add('tron-auth-logged-out');
    }
  }
  function setBrand(text){ var el=q('brandSub'); if(el) el.textContent = text || '請先登入'; }
  function setMsg(text, ok){ var el=q('msg'); if(!el)return; el.textContent=text||''; el.className='msg '+(ok?'ok':(text?'bad':'')); }
  function showLogin(reason){ setClass('out'); setBrand('請先登入'); setMsg(reason||'請先登入。', false); if(!isLoginPage() && isAdminPath()) location.replace(loginUrl()); }
  function showAuthed(data){ setClass('ok'); setBrand((data&&data.current_account_id)||(data&&data.account_id)||'已登入'); setMsg('', true); syncLinks(); }
  function logout(){ /* LOGOUT_SERVER_SIDE_V1 */ if (typeof window.tronSecureLogout === 'function') return window.tronSecureLogout(); fetch('/admin/logout',{method:'POST',credentials:'same-origin',cache:'no-store',redirect:'manual'}).catch(function(){}).then(function(){location.replace('/admin')}); }

  async function parseResponse(res){ var body=await res.json().catch(function(){return{}}); if(!res.ok){var e=new Error(body.error||('HTTP '+res.status));e.status=res.status;e.body=body;throw e;} return body; }
  async function statusApi(){ var res=await fetch('/admin/api/status',{cache:'no-store',credentials:'same-origin'}); return parseResponse(res); }

  async function verify(){
    try{localStorage.removeItem(LS_LOGOUT)}catch(_){} /* LOGOUT_SERVER_SIDE_V1：不再看 ?logout=1 / 手動登出旗標 */
    /* cookie session */
    try{var data=await statusApi();showAuthed(data);}catch(e){if(e.status===401){localStorage.removeItem(LS_SESSION);showLogin('登入狀態失效，請重新登入。');}else{setClass('ok');}}
  }
  function syncLinks(){
    var t=token(); if(!t)return;
    document.querySelectorAll('a[href^="/admin"]').forEach(function(a){
      var href=a.getAttribute('href')||'';
      if(href.indexOf('/admin/api/')===0)return;
      try{a.href=withToken(href);}catch(_){}
    });
  }
  window.tronAuth={token:token,verify:verify,logout:logout,syncLinks:syncLinks};
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',verify); else verify();
})();

/* ── inline block: main (原 timetable.html:894) ── */
(function(){
  'use strict';
  var PERIOD={1:['08:10','09:00'],2:['09:10','10:00'],3:['10:10','11:00'],4:['11:10','12:00'],5:['12:10','13:00'],6:['13:10','14:00'],7:['14:10','15:00'],8:['15:10','16:00'],9:['16:10','17:00'],10:['17:10','18:00']};
  var DAYS=[['mon','星期一'],['tue','星期二'],['wed','星期三'],['thu','星期四'],['fri','星期五'],['sat','星期六'],['sun','星期日']];
  var DAY_LABEL={mon:'星期一',tue:'星期二',wed:'星期三',thu:'星期四',fri:'星期五',sat:'星期六',sun:'星期日'};
  var TEMPLATE_KEY='tronclass_timetable_templates_mobilefix_v1';
  var SELECTED_KEY='tronclass_timetable_selected_account_mobilefix_v1';
  var entries=[],status=null,mode='view',loading=false;

  function q(id){return document.getElementById(id)}
  function esc(v){return String(v==null?'':v).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]})}
  function token(){try{if(window.__tronBootToken)return String(window.__tronBootToken).trim();
      var u=new URLSearchParams(location.search).get('token');
      if(u){return u.trim()}
      var keys=['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
      for(var i=0;i<keys.length;i++){
        var v=localStorage.getItem(keys[i])||sessionStorage.getItem(keys[i]);
        if(v&&String(v).trim())return String(v).trim()
      }
    }catch(_){}
    return ''
  }
  function storeToken(t){
    if(!t)return;
    ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'].forEach(function(k){
      try{localStorage.removeItem(k);sessionStorage.removeItem(k)}catch(_){}
    });
    try{
      var u=new URL(location.href);
      if(u.searchParams.has('token')){
        u.searchParams.delete('token');
        history.replaceState(null,'',u.pathname+u.search+u.hash)
      }
    }catch(_){}
  }
  function withToken(path){
    /* TOKEN_STORAGE_PURGE_V1: cookie-only */
    return path
  }
  async function parse(res){
    var b=await res.json().catch(function(){return{}});
    if(!res.ok){
      var e=new Error(b.error||b.message||('HTTP '+res.status));
      e.status=res.status;
      throw e
    }
    return b
  }
  async function api(path,opts){
    opts=opts||{};opts.credentials=opts.credentials||'same-origin';return fetch(withToken(path),opts).then(parse)
  }
  function toast(m,ok){
    if(!String(m||'').trim())return;
    var el=q('toast');
    if(!el)return;
    el.textContent=m;
    el.style.background=ok===false?'#991b1b':'#172033';
    el.classList.add('visible');
    clearTimeout(el._t);
    el._t=setTimeout(function(){el.classList.remove('visible')},2600)
  }
  function msg(m,ok){
    var el=q('msg');
    if(!el)return;
    el.textContent=m||'';
    el.className='msg '+(ok===true?'ok':ok===false?'bad':'')
  }
  function pStart(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][0]:''}
  function pEnd(p){return PERIOD[Number(p||0)]?PERIOD[Number(p||0)][1]:''}
  function normDay(v){
    v=String(v||'').trim().toLowerCase();
    var map={'星期一':'mon','週一':'mon','一':'mon','1':'mon','mon':'mon','monday':'mon','星期二':'tue','週二':'tue','二':'tue','2':'tue','tue':'tue','tuesday':'tue','星期三':'wed','週三':'wed','三':'wed','3':'wed','wed':'wed','wednesday':'wed','星期四':'thu','週四':'thu','四':'thu','4':'thu','thu':'thu','thursday':'thu','星期五':'fri','週五':'fri','五':'fri','5':'fri','fri':'fri','friday':'fri','星期六':'sat','週六':'sat','六':'sat','6':'sat','sat':'sat','saturday':'sat','星期日':'sun','星期天':'sun','週日':'sun','日':'sun','天':'sun','7':'sun','sun':'sun','sunday':'sun'};
    return map[v]||v||'mon'
  }
  function norm(e){
    var ps=Number(e.period_start||e.period||1)||1,pe=Number(e.period_end||ps)||ps;
    if(pe<ps)pe=ps;
    return{
      weekday:normDay(e.weekday||'mon'),
      period_start:ps,
      period_end:pe,
      start_time:e.start_time||pStart(ps),
      end_time:e.end_time||pEnd(pe),
      course_name:e.course_name||e.course||'',
      classroom:e.classroom||e.room||'',
      teacher:e.teacher||'',
      course_code:e.course_code||'',
      source:'manual'
    }
  }
  async function loadStatus(){
    try{
    status=await api('/admin/api/status',{cache:'no-store'});
    document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','mobile-nav-final-auth');
    if(document.body) document.body.classList.add('auth-ok','dash-auth-ok','logged-in');
    q('brandSub').textContent=status.current_account_id||'已登入';
    if(q('backDashboard')) q('backDashboard').href=withToken('/admin');
    if(q('ttPanelBackDashboard')) q('ttPanelBackDashboard').href=withToken('/admin');
    q('meLink').href=withToken('/admin/me');
    fillAccounts();
    return status
    }catch(e){
      msg('讀取帳號／權限失敗：'+(e.message||e),false);
      return null
    }
  }
  function selected(){
    var saved=localStorage.getItem(SELECTED_KEY)||'';
    if(saved)return saved;
    if(status&&status.current_account_id&&status.current_account_id!=='<token-admin>')return status.current_account_id;
    if(status&&status.accounts&&status.accounts[0])return status.accounts[0].account_id;
    return ''
  }
  function fillAccounts(){
    var sel=q('accountSelect'),old=sel.value||selected();
    sel.innerHTML='';
    if(status&&status.is_admin&&Array.isArray(status.accounts)&&status.accounts.length){
      status.accounts.forEach(function(a){
        if(a.account_id)sel.appendChild(new Option(accountLabel(a),a.account_id))
      });
      if(old&&Array.prototype.some.call(sel.options,function(o){return o.value===old}))sel.value=old;
      else if(sel.options.length)sel.selectedIndex=0;
      sel.disabled=false;
    }else{
      var me=(status&&status.current_account_id)||selected();
      sel.appendChild(new Option(accountLabel({account_id:me,user_name:status&&status.user_name}),me||''));
      sel.value=me||'';
      sel.disabled=false;
    }
    return sel.value;
  }
  async function loadEntries(){
    if(loading)return;
    loading=true;
    msg('讀取課表中...');
    try{
      var acct=fillAccounts();
      localStorage.setItem(SELECTED_KEY,acct||'');
      var b=await api('/admin/api/timetable'+(acct?'?account_id='+encodeURIComponent(acct):''),{cache:'no-store'});
      entries=(b.entries||[]).map(norm);
      msg('已顯示：'+(acct||'目前帳號')+' 的課表',true)
    }catch(e){
      entries=[];
      msg('讀取課表失敗：'+(e.message||e),false)
    }finally{
      loading=false
    }
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
      // MOBILE_TIMETABLE_EXPANDED_PERIODS_V1: always render periods 1–10.
      // A multi-period course gets one independent row per period instead of a
      // merged "6–7" row, while the desktop poster remains compact.
      for(var p=1;p<=10;p++){
        var courseBlock=null;
        for(var b=0;b<blocks.length;b++){
          if(blocks[b].day===d[0] && blocks[b].ps<=p && blocks[b].pe>=p){
            courseBlock=blocks[b];
            break;
          }
        }
        var tm=PERIOD[p]||['',''];
        if(courseBlock){
          var place=String(courseBlock.e.classroom||'').trim();
          var teacher=String(courseBlock.e.teacher||'').trim();
          var meta=place||teacher;
          html+='<div class="tt-mobile-slot"><div class="tt-mobile-period"><span class="n">'+p+'</span><span class="t">'+esc(tm[0])+'<br>'+esc(tm[1])+'</span></div><button type="button" class="tt-poster-course tt-mobile-course" data-course="'+esc(courseBlock.e.course_name)+'" title="'+esc((courseBlock.e.course_name||'')+(place?' ｜ '+place:'')+(teacher?' ｜ '+teacher:''))+'"><span class="tt-poster-name">'+esc(courseBlock.e.course_name)+'</span>'+(meta?'<span class="tt-poster-meta">'+esc(meta)+'</span>':'')+'</button></div>';
        }else{
          html+='<div class="tt-mobile-slot is-empty"><div class="tt-mobile-period"><span class="n">'+p+'</span><span class="t">'+esc(tm[0])+'<br>'+esc(tm[1])+'</span></div><div class="tt-mobile-empty">空堂</div></div>';
        }
      }
      html+='</div>';
    });
    html+='</div></div>';
    bindMobileWeek();
    return html;
  }
  function renderView(){
    mode='view';
    q('panel').classList.remove('mode-edit');
    q('pageTitle').textContent='課表';
    q('pageSub').textContent='查看課表，按「編輯課表」後可手動輸入；也可匯入 CSV 或套用模板。';
    var real=entries.filter(function(e){return String(e.course_name||'').trim()});
    q('viewer').innerHTML=real.length?renderPoster(real):'<div class="tt-poster-empty-msg">目前沒有課表資料，請按「編輯課表」新增。</div>';
    bindMobileWeek();
    if(window.enhanceCourses) setTimeout(window.enhanceCourses, 60);
  }
  function rowHtml(e,i){
    var ps=Math.max(1, Math.min(14, Number(e.period_start||1))), pe=Math.max(ps, Math.min(14, Number(e.period_end||ps)));
    return'<div class="row" data-i="'+i+'"><select class="day">'+DAYS.map(function(d){return'<option value="'+d[0]+'" '+(e.weekday===d[0]?'selected':'')+'>'+d[1]+'</option>'}).join('')+'</select><input class="ps" type="number" min="1" max="14" placeholder="起節" value="'+esc(ps)+'"><input class="pe" type="number" min="1" max="14" placeholder="訖節" value="'+esc(pe)+'"><input class="course" placeholder="課程名稱" value="'+esc(e.course_name||'')+'"><input class="room" placeholder="教室" value="'+esc(e.classroom||'')+'"><input class="teacher" placeholder="授課老師" value="'+esc(e.teacher||'')+'"><div class="time">'+esc((e.start_time||pStart(ps)||'-')+' - '+(e.end_time||pEnd(pe)||'-'))+'</div><button class="del danger" type="button">刪除</button></div>'
  }
  function syncRow(row){
    var i=Number(row.dataset.i),ps=Math.max(1,Math.min(14,Number(row.querySelector('.ps').value||1))),pe=Math.max(ps,Math.min(14,Number(row.querySelector('.pe').value||ps)));
    row.querySelector('.ps').value=ps;
    row.querySelector('.pe').value=pe;
    entries[i]=norm({
      weekday:row.querySelector('.day').value,
      period_start:ps,
      period_end:pe,
      start_time:pStart(ps),
      end_time:pEnd(pe),
      course_name:row.querySelector('.course').value,
      classroom:row.querySelector('.room').value,
      teacher:row.querySelector('.teacher').value
    });
    row.querySelector('.time').textContent=(pStart(ps)||'-')+' - '+(pEnd(pe)||'-')
  }
  function bindRows(){
    document.querySelectorAll('#rows .row').forEach(function(row){
      row.querySelectorAll('input,select').forEach(function(el){
        el.oninput=function(){syncRow(row)};
        el.onchange=function(){syncRow(row)}
      });
      row.querySelector('.del').onclick=function(){
        entries.splice(Number(row.dataset.i),1);
        renderEdit()
      }
    })
  }
  function templates(){try{return JSON.parse(localStorage.getItem(TEMPLATE_KEY)||'{}')}catch(_){return{}}}
  function saveTemplates(t){localStorage.setItem(TEMPLATE_KEY,JSON.stringify(t||{}))}
  function renderTemplates(){
    var t=templates(),names=Object.keys(t).sort();
    q('templateSelect').innerHTML='<option value="">模板課表</option>'+names.map(function(n){return'<option value="'+esc(n)+'">'+esc(n)+'</option>'}).join('')
  }
  function renderEdit(){
    mode='edit';
    if(!entries.length)entries=[norm({weekday:'mon',period_start:1,period_end:1})];
    q('panel').classList.add('mode-edit');
    q('pageTitle').textContent='編輯課表';
    q('pageSub').textContent='只要填第幾節，時間會自動帶入。可匯入 CSV、套用模板或自己新增課程。';
    renderTemplates();
    q('rows').innerHTML=entries.map(rowHtml).join('');
    bindRows()
  }
  async function saveEntries(){
    document.querySelectorAll('#rows .row').forEach(syncRow);
    var valid=entries.filter(function(e){return String(e.course_name||'').trim()});
    if(!valid.length){msg('請至少輸入一堂課的課程名稱。',false);return}
    msg('儲存中...');
    try{
      await api('/admin/api/timetable/import',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({account_id:q('accountSelect').value||undefined,entries:valid})});
      entries=valid;
      msg('課表已儲存。',true);
      toast('課表已儲存');
      renderView()
    }catch(e){
      msg(e.message||'儲存課表失敗',false);
      toast(e.message||'儲存課表失敗',false)
    }
  }
  function parseCsv(text){
    var rows=[],row=[],cur='',quote=false;
    for(var i=0;i<text.length;i++){
      var ch=text[i],nx=text[i+1];
      if(ch==='"'){
        if(quote&&nx==='"'){cur+='"';i++}else quote=!quote
      }else if(ch===','&&!quote){
        row.push(cur);cur=''
      }else if((ch==='\n'||ch==='\r')&&!quote){
        if(ch==='\r'&&nx==='\n')i++;
        row.push(cur);
        if(row.some(function(x){return String(x).trim()}))rows.push(row);
        row=[];cur=''
      }else cur+=ch
    }
    row.push(cur);
    if(row.some(function(x){return String(x).trim()}))rows.push(row);
    return rows
  }
  function parsePeriodNum(s){
    s = String(s || '').trim();
    if(!s) return null;
    var n = parseInt(s, 10);
    if(!isNaN(n)) return n;
    var zhMap = {
      '一':1,'二':2,'三':3,'四':4,'五':5,'六':6,'七':7,'八':8,'九':9,'十':10,
      '十一':11,'十二':12,'十三':13,'十四':14,'十五':15,
      '壹':1,'貳':2,'參':3,'肆':4,'伍':5,'陸':6,'柒':7,'捌':8,'玖':9,'拾':10,
      'A':1,'B':2,'C':3,'D':4,'E':5,'F':6,'G':7,'H':8,'I':9,'J':10,'N':11
    };
    var upper = s.toUpperCase();
    if(zhMap[upper] !== undefined) return zhMap[upper];
    if(upper.startsWith('第') && upper.endsWith('節')){
      var mid = upper.slice(1, -1).trim();
      if(zhMap[mid] !== undefined) return zhMap[mid];
      var midNum = parseInt(mid, 10);
      if(!isNaN(midNum)) return midNum;
    }
    return null;
  }

  function parsePeriod(v){
    if(v == null) return [1, 1];
    var s = String(v).trim();
    if(!s) return [1, 1];

    var parts = s.split(/[-~至到,、\s+]+/).filter(Boolean);
    if(parts.length > 0){
      var p1 = parsePeriodNum(parts[0]);
      var p2 = parts.length > 1 ? parsePeriodNum(parts[parts.length - 1]) : p1;
      if(p1 != null){
        var ps = Math.max(1, Math.min(14, p1));
        var pe = p2 != null ? Math.max(ps, Math.min(14, p2)) : ps;
        return [ps, pe];
      }
    }

    var nums = s.match(/\d+/g);
    if(nums && nums.length > 0){
      var ps = Math.max(1, Math.min(14, Number(nums[0])));
      var pe = nums.length > 1 ? Math.max(ps, Math.min(14, Number(nums[nums.length - 1]))) : ps;
      return [ps, pe];
    }

    return [1, 1];
  }
  function csvEntries(text){
    var rows=parseCsv(text);
    if(!rows.length)return[];
    
    var headerIdx = -1;
    for(var r=0; r<Math.min(5, rows.length); r++){
      var rowStr = rows[r].join(' ').toLowerCase();
      if(/星期|weekday|day|課程|course|節次|節數|period|教室|room|classroom|老師|教師|teacher/.test(rowStr)){
        headerIdx = r;
        break;
      }
    }

    var hasHeader = headerIdx >= 0;
    var header = hasHeader ? rows[headerIdx].map(function(x){ return String(x||'').trim().toLowerCase(); }) : [];
    var dataRows = hasHeader ? rows.slice(headerIdx + 1) : rows;

    function getCol(row, aliases, defaultIdx){
      if(hasHeader){
        for(var a=0; a<aliases.length; a++){
          var idx = header.indexOf(aliases[a]);
          if(idx >= 0 && row[idx] !== undefined) return String(row[idx]).trim();
        }
        for(var a=0; a<aliases.length; a++){
          for(var h=0; h<header.length; h++){
            if(header[h].indexOf(aliases[a]) >= 0 && row[h] !== undefined) return String(row[h]).trim();
          }
        }
      }
      return defaultIdx >= 0 && defaultIdx < row.length ? String(row[defaultIdx]).trim() : '';
    }

    return dataRows.map(function(r){
      if(!r || !r.some(function(x){ return String(x||'').trim(); })) return null;

      var day = getCol(r, ['星期','星期幾','weekday','day','週次','禮拜','week'], 0);
      var startPeriod = getCol(r, ['開始節次','起始節次','start_period','period_start','起節'], -1);
      var endPeriod = getCol(r, ['結束節次','訖節','end_period','period_end','止節'], -1);
      var periodStr = getCol(r, ['節次','節數','課堂節次','period','節','堂次'], 1);
      var timeRange = getCol(r, ['時間','上課時間','time','time_range'], -1);
      var course = getCol(r, ['課程名稱','課程','科目','course_name','course','subject'], 2);
      var room = getCol(r, ['教室','上課教室','地點','classroom','room','location'], 3);
      var teacher = getCol(r, ['授課教師','授課老師','教師','老師','teacher','instructor'], 4);

      var ps = 1, pe = 1;
      if(startPeriod){
        var prS = parsePeriod(startPeriod);
        ps = prS[0];
        pe = endPeriod ? parsePeriod(endPeriod)[0] : (prS[1] || ps);
      } else if(periodStr){
        var pr = parsePeriod(periodStr);
        ps = pr[0];
        pe = pr[1];
      }

      if(pe < ps) pe = ps;
      ps = Math.max(1, Math.min(14, ps));
      pe = Math.max(ps, Math.min(14, pe));

      var st = pStart(ps), et = pEnd(pe);
      if(timeRange){
        var tMatch = timeRange.match(/(\d{1,2}:\d{2})\s*[-~至到]\s*(\d{1,2}:\d{2})/);
        if(tMatch){ st = tMatch[1]; et = tMatch[2]; }
      }

      return norm({
        weekday: normDay(day),
        period_start: ps,
        period_end: pe,
        start_time: st,
        end_time: et,
        course_name: course,
        classroom: room,
        teacher: teacher
      });
    }).filter(function(e){ return e && (e.course_name || e.classroom || e.teacher); });
  }
  function handleCsvFile(ev){
    var f=ev.target.files&&ev.target.files[0];
    if(!f)return;
    var rd=new FileReader();
    rd.onload=function(){
      var im=csvEntries(String(rd.result||''));
      if(!im.length)return toast('CSV 沒有可匯入的課程',false);
      entries=entries.filter(function(e){return String(e.course_name||'').trim()}).concat(im);
      renderEdit();
      toast('已匯入 '+im.length+' 堂課')
    };
    rd.readAsText(f,'utf-8');
    ev.target.value=''
  }
  function downloadCsv(){
    var csv='星期,節次,課程名稱,教室,授課教師\n星期一,1-2,微積分,E304,王老師\n星期二,3-4,計算機概論,B101,李老師\n星期三,5-6,普通物理,S201,陳老師\n';
    var blob=new Blob([csv],{type:'text/csv;charset=utf-8'});
    var a=document.createElement('a');
    a.href=URL.createObjectURL(blob);
    a.download='timetable_template.csv';
    a.click();
    setTimeout(function(){URL.revokeObjectURL(a.href)},1000)
  }
  function saveTemplate(){
    document.querySelectorAll('#rows .row').forEach(syncRow);
    var name=prompt('模板名稱：');
    if(!name)return;
    var t=templates();
    t[name]=entries.filter(function(e){return String(e.course_name||'').trim()});
    saveTemplates(t);
    renderTemplates();
    toast('模板已儲存')
  }
  function applyTemplate(){
    var n=q('templateSelect').value;
    if(!n)return toast('請先選擇模板',false);
    entries=(templates()[n]||[]).map(norm);
    renderEdit();
    toast('已套用模板')
  }
  function delTemplate(){
    var n=q('templateSelect').value;
    if(!n)return toast('請先選擇模板',false);
    if(!confirm('刪除模板：'+n+'？'))return;
    var t=templates();
    delete t[n];
    saveTemplates(t);
    renderTemplates();
    toast('模板已刪除')
  }

  q('reloadBtn').onclick=function(){loadEntries().then(renderView)};
  q('editBtn').onclick=renderEdit;
  if(q('wallpaperBtn')) q('wallpaperBtn').onclick=function(){ window.exportTimetableWallpaper && window.exportTimetableWallpaper(entries, PERIOD); };
  q('addRowBtn').onclick=function(){entries.push(norm({weekday:'mon',period_start:1,period_end:1}));renderEdit()};
  q('saveBtn').onclick=saveEntries;
  q('reloadEditBtn').onclick=function(){loadEntries().then(renderEdit)};
  q('backViewBtn').onclick=renderView;
  q('importCsvBtn').onclick=function(){q('csvFile').click()};
  q('downloadCsvBtn').onclick=downloadCsv;
  q('saveTemplateBtn').onclick=saveTemplate;
  q('applyTemplateBtn').onclick=applyTemplate;
  q('deleteTemplateBtn').onclick=delTemplate;
  q('csvFile').onchange=handleCsvFile;
  q('accountSelect').onchange=function(){
    localStorage.setItem(SELECTED_KEY,q('accountSelect').value);
    loadEntries().then(function(){
      if(mode==='edit') renderEdit();
      else renderView();
      loadSyncStatus();
    });
  };

  // TIMETABLE_AUTO_SYNC_V1 —— 自動同步狀態列
  function syncBar(){ return q('ttSyncBar'); }
  function fmtTime(ts){
    if(!ts) return '尚未同步';
    try{
      var d=new Date(ts*1000);
      if(isNaN(d.getTime())) return '尚未同步';
      var p=function(n){return (n<10?'0':'')+n};
      return d.getFullYear()+'-'+p(d.getMonth()+1)+'-'+p(d.getDate())+' '+p(d.getHours())+':'+p(d.getMinutes());
    }catch(_){ return '尚未同步'; }
  }
  function renderSync(s){
    var bar=syncBar();
    if(!bar) return;
    // 學校不支援時整條隱藏，避免給出一個按了沒反應的開關。
    if(!s || !s.supported){ bar.hidden=true; return; }
    bar.hidden=false;

    var badge=q('ttSyncBadge'), detail=q('ttSyncDetail'), box=q('ttSyncEnabled');
    if(box) box.checked=!!s.enabled;

    var cls='sync-badge', label='未啟用';
    if(s.enabled){
      if(s.consecutive_failures>0){ cls+=' err'; label='同步異常'; }
      else { cls+=' on'; label='已啟用'; }
    }
    if(badge){ badge.className=cls; badge.textContent=label; }

    if(detail){
      var parts=[];
      if(s.enabled){
        parts.push('上次成功：'+fmtTime(s.last_success_at));
        if(s.last_summary) parts.push(s.last_summary);
        if(s.consecutive_failures>0){
          parts.push('連續失敗 '+s.consecutive_failures+' 次');
          // last_error 已在伺服器端清洗過，不含帳密／學號／cookie。
          if(s.last_error) parts.push(s.last_error);
        }
      }else{
        parts.push('開啟後每天自動從學校校務系統更新課表；手動輸入的課程不會被覆蓋。');
      }
      detail.textContent=parts.join('　·　');
    }

    updateSyncTargetLabel();
  }

  /* SYNC_TARGET_VISIBLE_V1
     「立即同步」永遠只同步一個帳號（後端 sync_account 為單數，不會動到其他人），
     但目標是帳號下拉選單目前選中的那一個。管理員切到別人的帳號後按下去，
     同步的就是那個人的課表——這在權限上是允許的，但按鈕上看不出來。
     因此在按鈕文字標出目標帳號，並在目標不是自己時加上警示樣式。 */
  function syncTargetId(){
    var sel=q('accountSelect');
    return (sel && sel.value) || (status && status.current_account_id) || '';
  }
  function syncTargetIsSelf(){
    var me=status && status.current_account_id;
    // 管理員 token 沒有真實帳號，一律視為「非自己」以保守顯示。
    if(!me || me==='<token-admin>') return false;
    return syncTargetId()===me;
  }
  function updateSyncTargetLabel(){
    var btn=q('ttSyncNowBtn');
    if(!btn) return;
    var id=syncTargetId();
    if(!id){ btn.textContent='立即同步'; btn.classList.remove('other-account'); btn.title=''; return; }

    var label=id;
    if(status && Array.isArray(status.accounts)){
      for(var i=0;i<status.accounts.length;i++){
        if(status.accounts[i].account_id===id){ label=accountLabel(status.accounts[i]); break; }
      }
    }
    var self=syncTargetIsSelf();
    btn.textContent='立即同步（'+label+'）';
    btn.classList.toggle('other-account',!self);
    btn.title=self
      ? '同步你自己的課表：'+label
      : '注意：將同步其他帳號的課表 → '+label;
  }
  async function loadSyncStatus(){
    try{
      var acct=q('accountSelect')?q('accountSelect').value:'';
      var s=await api('/admin/api/timetable/sync'+(acct?'?account_id='+encodeURIComponent(acct):''),{cache:'no-store'});
      renderSync(s);
    }catch(_){
      var bar=syncBar();
      if(bar) bar.hidden=true;
    }
  }
  if(q('ttSyncEnabled')) q('ttSyncEnabled').onchange=async function(){
    var bar=syncBar(), want=q('ttSyncEnabled').checked;
    if(bar) bar.classList.add('busy');
    try{
      await api('/admin/api/timetable/sync/toggle',{
        method:'POST',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({account_id:q('accountSelect').value||undefined,enabled:want})
      });
      toast(want?'已開啟自動同步':'已關閉自動同步',true);
    }catch(e){
      toast('設定失敗：'+(e.message||e),false);
      q('ttSyncEnabled').checked=!want;
    }finally{
      if(bar) bar.classList.remove('busy');
      loadSyncStatus();
    }
  };
  if(q('ttSyncNowBtn')) q('ttSyncNowBtn').onclick=async function(){
    /* SYNC_TARGET_VISIBLE_V1
       同步別人的帳號會連到學校系統、並覆寫該帳號的自動課表，
       誤觸的代價不小，因此目標不是自己時先確認一次。 */
    var targetId=syncTargetId();
    if(!syncTargetIsSelf()){
      var btn=q('ttSyncNowBtn');
      var shown=btn?btn.textContent.replace(/^立即同步（|）$/g,''):targetId;
      if(!confirm('這會同步「'+shown+'」的課表，不是你自己的帳號。\n\n確定要繼續嗎？')) return;
    }
    var bar=syncBar();
    if(bar) bar.classList.add('busy');
    msg('正在從學校系統同步課表...');
    try{
      var r=await api('/admin/api/timetable/sync/run',{
        method:'POST',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({account_id:targetId||undefined})
      });
      toast(r.summary||'同步完成',true);
      msg(r.summary||'同步完成',true);
      await loadEntries();
      if(mode==='edit') renderEdit(); else renderView();
    }catch(e){
      toast('同步失敗：'+(e.message||e),false);
      msg('同步失敗：'+(e.message||e),false);
    }finally{
      if(bar) bar.classList.remove('busy');
      loadSyncStatus();
    }
  };
  q('logoutBtn').onclick=function(){
    localStorage.removeItem('tronclass_admin_session');
    sessionStorage.removeItem('tronclass_admin_session');
    location.href='/admin?v=logout'
  };

  loadStatus().then(function(st){
    if(st) return loadEntries();
  }).then(function(){
    if(new URLSearchParams(location.search).get('edit')==='1') renderEdit();
    else renderView();
    loadSyncStatus();
  }).catch(function(e){
    msg(e.message||'初始化失敗',false);
  });
})();

/* ── inline block: timetableCourseRollcallHistoryLinkScriptV1 (原 timetable.html:1660) ── */
(function(){
  'use strict';
  if(window.TIMETABLE_COURSE_ROLLCALL_HISTORY_LINK_V1) return;
  window.TIMETABLE_COURSE_ROLLCALL_HISTORY_LINK_V1 = true;

  var FILTER_KEY = 'tronclass_rollcall_history_course_filter_v1';
  var TOKEN_KEYS = ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
  var lastStatus = null;

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];}); }
  function textOf(el){ return (el && (el.textContent || el.value || '') || '').replace(/\s+/g,' ').trim(); }
  function norm(v){ return String(v || '').replace(/\s+/g,'').toLowerCase(); }
  function token(){try{if(window.__tronBootToken)return String(window.__tronBootToken).trim();
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
    /* TOKEN_STORAGE_PURGE_V1: cookie-only */
    return path;
  }
  async function parse(res){
    var b = await res.json().catch(function(){ return {}; });
    if(!res.ok){
      var e = new Error(b.error || b.message || ('HTTP ' + res.status));
      e.status = res.status;
      throw e;
    }
    return b;
  }
  async function api(path){
    return fetch(withToken(path), {cache:'no-store'}).then(parse);
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
    document.addEventListener('keydown', function(ev){ if(ev.key === 'Escape') modal.classList.remove('visible'); });
    return modal;
  }
  async function loadStatus(){
    if(window.status && typeof window.status === 'object' && Array.isArray(window.status.accounts)){
      lastStatus = window.status;
      return lastStatus;
    }
    if(lastStatus) return lastStatus;
    lastStatus = await api('/admin/api/status');
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
    if(!ts) return '尚未';
    try{ return new Date(Number(ts)*1000).toLocaleString(); }catch(_){ return String(ts); }
  }
  function historyHtml(list){
    if(!list || !list.length) return '<div class="tt-history-empty">這門課目前在系統內沒有點名紀錄。</div>';
    return list.map(function(h){
      var ok = !!h.success;
      var label = ok ? '成功' : '未成功';
      var pillClass = ok ? 'tt-history-pill ok' : 'tt-history-pill bad';
      return '<div class="tt-history-item"><div class="tt-history-item-top"><span>'+esc(h.course_name || '課程')+'</span><span class="'+pillClass+'">'+esc(label)+'</span></div><div class="tt-history-meta">'+esc(fmt(h.timestamp))+'｜'+esc(h.attendance_type || '簽到')+'｜#'+esc(h.rollcall_id || '-')+'</div>'+(h.result ? '<div class="tt-history-detail">'+esc(h.result)+'</div>' : '')+'</div>';
    }).join('');
  }
  function courseMatches(rawCourse, targetCourse){
    var a = norm(rawCourse), b = norm(targetCourse);
    if(!a || !b) return false;
    return a === b || a.indexOf(b) >= 0 || b.indexOf(a) >= 0;
  }
  async function showCourseHistory(course, explicitAccountId){
    if(!course) return;
    var modal = ensureModal();
    modal.classList.add('visible');
    q('ttHistoryTitle').textContent = '點名紀錄：' + course;
    q('ttHistoryMeta').textContent = '讀取紀錄中...';
    q('ttHistoryBody').innerHTML = '<div class="tt-history-empty">正在搜尋這門課的點名紀錄...</div>';
    try{
      var status = await loadStatus();
      var acctId = explicitAccountId || selectedAccount(status);
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
      if(btn){
        btn.onclick = function(ev){
          ev.preventDefault();
          ev.stopPropagation();
          if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
          showCourseHistory(course);
        };
      }
    });
  }
  function enhanceCourses(){
    var scopes = [];
    var tv = q('view-timetable') || q('viewer');
    if(tv) scopes.push(tv);
    if(location.pathname.indexOf('/admin/timetable') === 0) scopes.push(document);
    if(!scopes.length) return;
    scopes.forEach(function(scope){
      Array.prototype.forEach.call(scope.querySelectorAll('table'), enhanceTable);
    });
  }
  window.enhanceCourses = enhanceCourses;

  function start(){
    enhanceCourses();
    var timer = null;
    try{
      new MutationObserver(function(){
        clearTimeout(timer);
        timer = setTimeout(enhanceCourses, 120);
      }).observe(document.documentElement, {childList:true, subtree:true});
    }catch(_){ }
    document.addEventListener('visibilitychange', function(){ if(!document.hidden) setTimeout(enhanceCourses, 100); });
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();

/* ── inline block: courseHistoryModalCloseOnJumpV1 (原 timetable.html:1842) ── */
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

/* ── inline block: RETURN_DASHBOARD_HREF_V1 (原 timetable.html:1908) ── */
(function(){
  function tok(){
    try{ return (window.__tronBootToken||new URLSearchParams(location.search).get('token')||'').trim(); }catch(_){ return ''; }
  }
  function patch(){
    var t=tok();
    document.querySelectorAll('a[href="/admin"],a[href="/admin/"],a[href="/admin#overview"],a[href^="/admin?"]').forEach(function(a){
      try{
        var u=new URL(a.getAttribute('href'), location.origin);
        if(!(u.pathname==='/admin'||u.pathname==='/admin/'||u.pathname==='/admin/dashboard')) return;
        /* TOKEN_STORAGE_PURGE_V1: cookie-only——連結不帶 token */
        void t;
        if(!u.hash) u.hash='overview';
        a.setAttribute('href', u.pathname+u.search+u.hash);
      }catch(_){}
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', patch);
  else patch();
  setTimeout(patch, 200);
})();

/* ── inline block: TT_MOBILE_HEADER_BACK_V1 (原 timetable.html:1933) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="TT_MOBILE_HEADER_BACK_V1">，瀏覽器會把有 id 的元素暴露成
     window.TT_MOBILE_HEADER_BACK_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.TT_MOBILE_HEADER_BACK_V1) return;
  window.TT_MOBILE_HEADER_BACK_V1 = true;
  function tok(){
    try{ return (window.__tronBootToken||new URLSearchParams(location.search).get('token')||'').trim(); }catch(_){ return ''; }
  }
  function href(){
    var t=tok();
    return t ? '/admin?token='+encodeURIComponent(t)+'#overview' : '/admin#overview';
  }
  function tidy(){
    var top=document.querySelector('.top-actions');
    if(top){
      Array.prototype.slice.call(top.children).forEach(function(n){
        if(n.id==='backDashboard') return;
        n.style.display='none';
        n.hidden=true;
      });
      var a=document.getElementById('backDashboard');
      if(!a){
        a=document.createElement('a');
        a.id='backDashboard';
        a.className='button primary';
        top.appendChild(a);
      }
      if(a.textContent!=='回控制台') a.textContent='回控制台'; /* INLINE_SPLIT_DEAD_BLOCK_V1：冪等寫入 */
      a.href=href();
      a.style.display='inline-flex';
      a.hidden=false;
    }
    document.querySelectorAll('#ttPanelBackDashboard,.actions a[href="/admin"],.actions a[href^="/admin?"]').forEach(function(el){
      if(el.id==='backDashboard') return;
      if(el.closest && el.closest('.top-actions')) return;
      el.style.display='none';
      el.hidden=true;
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', tidy);
  else tidy();
  setTimeout(tidy, 80);
  setTimeout(tidy, 400);
  try{ new MutationObserver(tidy).observe(document.documentElement,{childList:true,subtree:true}); }catch(_){}
})();

/* ── inline block: TT_WALLPAPER_V1 (原 timetable.html:1978) ── */
/* TT_WALLPAPER_V1 — iPhone 直式課表桌布 */
(function (global) {
  'use strict';
  var W = 1290;
  var H = 2796;

  function wallpaperName() {
    var el = document.getElementById('brandSub');
    var n = el && String(el.textContent || '').trim();
    if (n && n !== '載入中' && n !== '已登入') return n;
    var sel = document.getElementById('accountSelect') || document.getElementById('ttStableAccount');
    if (sel && sel.options && sel.selectedIndex >= 0) {
      n = String(sel.options[sel.selectedIndex].text || '').trim();
      if (n) return n;
    }
    return '我的課表';
  }

  function isDark() {
    var t = document.documentElement.getAttribute('data-theme');
    if (t === 'dark') return true;
    if (t === 'light') return false;
    return !!(window.matchMedia && window.matchMedia('(prefers-color-scheme:dark)').matches);
  }

  function rr(ctx, x, y, w, h, r) {
    var rad = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + rad, y);
    ctx.arcTo(x + w, y, x + w, y + h, rad);
    ctx.arcTo(x + w, y + h, x, y + h, rad);
    ctx.arcTo(x, y + h, x, y, rad);
    ctx.arcTo(x, y, x + w, y, rad);
    ctx.closePath();
  }

  function wrapText(ctx, text, maxW, maxLines) {
    var chars = Array.from(String(text || ''));
    var lines = [];
    var cur = '';
    for (var i = 0; i < chars.length; i++) {
      var t = cur + chars[i];
      if (cur && ctx.measureText(t).width > maxW) {
        lines.push(cur);
        cur = chars[i];
        if (lines.length >= maxLines) return lines;
      } else cur = t;
    }
    if (cur && lines.length < maxLines) lines.push(cur);
    return lines;
  }

  function collectBlocks(list) {
    var occupied = Object.create(null);
    var blocks = [];
    (list || []).forEach(function (e) {
      var name = String(e.course_name || '').trim();
      if (!name) return;
      var ps = Math.max(1, Math.min(10, Number(e.period_start || 1) || 1));
      var pe = Math.max(ps, Math.min(10, Number(e.period_end || ps) || ps));
      var clash = false;
      for (var p = ps; p <= pe; p++) if (occupied[e.weekday + ':' + p]) clash = true;
      if (clash) return;
      for (var q = ps; q <= pe; q++) occupied[e.weekday + ':' + q] = true;
      blocks.push({ day: e.weekday, ps: ps, pe: pe, name: name, room: String(e.classroom || e.teacher || '').trim() });
    });
    return { occupied: occupied, blocks: blocks };
  }

  function drawWallpaper(entries, periodMap) {
    var PERIOD = periodMap || {};
    var dark = isDark();
    var bg = dark ? '#0b1220' : '#eef3fb';
    var card = dark ? '#182232' : '#ffffff';
    var soft = dark ? '#121a28' : '#f8fafc';
    var text = dark ? '#f1f5f9' : '#172033';
    var muted = dark ? '#94a3b8' : '#64748b';
    var line = dark ? '#2a3a52' : '#dbe3ef';
    var periodBg = dark ? '#334155' : '#64748b';
    var accent = '#3b82f6';
    var courseBg = dark ? '#243656' : '#e8eef9';

    var data = collectBlocks(entries);
    var days = [['mon', '一'], ['tue', '二'], ['wed', '三'], ['thu', '四'], ['fri', '五']];
    if (data.blocks.some(function (b) { return b.day === 'sat'; })) days.push(['sat', '六']);
    if (data.blocks.some(function (b) { return b.day === 'sun'; })) days.push(['sun', '日']);

    var canvas = document.createElement('canvas');
    canvas.width = W;
    canvas.height = H;
    var ctx = canvas.getContext('2d');
    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, W, H);
    var g = ctx.createLinearGradient(0, 0, W, H);
    if (dark) {
      g.addColorStop(0, 'rgba(37,99,235,.18)');
      g.addColorStop(1, 'rgba(15,23,42,0)');
    } else {
      g.addColorStop(0, 'rgba(37,99,235,.08)');
      g.addColorStop(1, 'rgba(255,255,255,0)');
    }
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);

    var padX = 72;
    var topSafe = 260;
    var bottomSafe = 480;
    ctx.fillStyle = text;
    ctx.font = '800 42px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.textAlign = 'center';
    ctx.fillText('WEEKLY', W / 2, topSafe - 70);
    ctx.font = '900 86px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText('課表', W / 2, topSafe + 20);
    ctx.fillStyle = muted;
    ctx.font = '700 36px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText(wallpaperName(), W / 2, topSafe + 78);

    var gridX = padX;
    var gridY = topSafe + 130;
    var gridW = W - padX * 2;
    var gridH = H - gridY - bottomSafe;
    rr(ctx, gridX - 16, gridY - 24, gridW + 32, gridH + 48, 48);
    ctx.fillStyle = soft;
    ctx.fill();

    var leftW = 150;
    var colW = (gridW - leftW) / days.length;
    var rowH = gridH / 11;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    days.forEach(function (d, i) {
      var x = gridX + leftW + i * colW;
      ctx.fillStyle = muted;
      ctx.font = '800 32px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillText(d[1], x + colW / 2, gridY + rowH / 2);
    });

    for (var p = 1; p <= 10; p++) {
      var y = gridY + p * rowH;
      rr(ctx, gridX + 8, y + 8, leftW - 20, rowH - 16, 22);
      ctx.fillStyle = periodBg;
      ctx.fill();
      ctx.fillStyle = '#fff';
      ctx.font = '900 34px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillText(String(p), gridX + leftW / 2 - 2, y + rowH / 2 - 14);
      var tm = PERIOD[p] || ['', ''];
      ctx.font = '700 18px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillStyle = 'rgba(255,255,255,.88)';
      ctx.fillText(String(tm[0] || ''), gridX + leftW / 2 - 2, y + rowH / 2 + 16);
    }

    days.forEach(function (d, i) {
      for (var p = 1; p <= 10; p++) {
        var x = gridX + leftW + i * colW;
        var y = gridY + p * rowH;
        var start = null;
        for (var b = 0; b < data.blocks.length; b++) {
          if (data.blocks[b].day === d[0] && data.blocks[b].ps === p) { start = data.blocks[b]; break; }
        }
        if (start) {
          var span = start.pe - start.ps + 1;
          rr(ctx, x + 8, y + 8, colW - 16, rowH * span - 16, 24);
          ctx.fillStyle = courseBg;
          ctx.fill();
          ctx.strokeStyle = line;
          ctx.lineWidth = 2;
          ctx.stroke();
          ctx.fillStyle = text;
          ctx.font = '800 28px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
          var lines = wrapText(ctx, start.name, colW - 36, span >= 2 ? 3 : 2);
          var startY = y + (rowH * span) / 2 - ((lines.length + (start.room ? 1 : 0) - 1) * 18);
          lines.forEach(function (ln, li) {
            ctx.fillText(ln, x + colW / 2, startY + li * 36);
          });
          if (start.room) {
            ctx.fillStyle = muted;
            ctx.font = '700 22px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
            ctx.fillText(start.room, x + colW / 2, startY + lines.length * 36);
          }
        } else if (!data.occupied[d[0] + ':' + p]) {
          rr(ctx, x + 8, y + 8, colW - 16, rowH - 16, 22);
          ctx.fillStyle = card;
          ctx.fill();
        }
      }
    });

    ctx.fillStyle = muted;
    ctx.font = '600 24px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText('長按圖片 → 加入照片 → 設為桌布', W / 2, H - 200);
    return canvas;
  }

  function exportTimetableWallpaper(entries, periodMap) {
    var real = (entries || []).filter(function (e) { return String(e.course_name || '').trim(); });
    if (!real.length) {
      alert('目前沒有課表可以匯出');
      return;
    }
    var canvas = drawWallpaper(real, periodMap);
    var name = '課表桌布-' + wallpaperName() + '.png';
    canvas.toBlob(function (blob) {
      if (!blob) return;
      var file = new File([blob], name, { type: 'image/png' });
      if (navigator.share && navigator.canShare && navigator.canShare({ files: [file] })) {
        navigator.share({ files: [file], title: '課表桌布' }).catch(function () { saveBlob(blob, name); });
        return;
      }
      saveBlob(blob, name);
    }, 'image/png');
  }

  function saveBlob(blob, name) {
    var url = URL.createObjectURL(blob);
    var a = document.createElement('a');
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(function () { URL.revokeObjectURL(url); }, 4000);
  }

  global.exportTimetableWallpaper = exportTimetableWallpaper;
})(window);
