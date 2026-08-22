/* NOTIFICATIONS_STATUS_CORE_RUNTIME_PHASE4F7B */
/* NOTIFICATIONS_CORE_API_PHASE4F4B
 * All notification API calls now use the shared token/response handler. */
/* NOTIFICATIONS_MODULE_AFTER_READY_PHASE4C2 */
(function(){
  function start(){
/* moveDiscordNotifyToTabScriptV2 */
(function(){
  'use strict';
  if(window.MOVE_DISCORD_NOTIFY_TO_TAB_V2) return;
  window.MOVE_DISCORD_NOTIFY_TO_TAB_V2 = true;

  var lastStatus = null;

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g, function(ch){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]; }); }
  function getToken(){
    try{
      var urlToken = new URLSearchParams(location.search).get('token');
      var keys = ['tronclass_admin_session','tronclass_admin_token','adminToken','token'];
      if(urlToken) return String(urlToken).trim();
      for(var i=0;i<keys.length;i++){
        var v = localStorage.getItem(keys[i]) || sessionStorage.getItem(keys[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){ }
    return '';
  }
  function withToken(path){
    var t = getToken();
    if(!t) return path;
    return path + (path.indexOf('?') >= 0 ? '&' : '?') + 'token=' + encodeURIComponent(t);
  }
  async function parseResponse(res){
    var body = await res.json().catch(function(){ return {}; });
    if(!res.ok){
      var e = new Error(body.error || body.message || ('HTTP ' + res.status));
      e.status = res.status;
      throw e;
    }
    return body;
  }
  async function api(path, opts){
    return fetch(withToken(path), opts || {}).then(parseResponse);
  }
  function showToast(text, ok){
    var el = q('toast');
    if(el){
      el.textContent = text;
      el.className = 'toast visible ' + (ok === false ? 'bad' : 'ok');
      clearTimeout(el._notifyTimer);
      el._notifyTimer = setTimeout(function(){ el.classList.remove('visible'); }, 3000);
    }else{
      console.log(text);
    }
  }

  function removeStrayOldDiscordPanel(){
    // 舊 DISCORD_NOTIFICATION_SETTINGS_UI_V1 會建立 #discordNotifyPanel；若瀏覽器還載到舊 DOM，直接清掉錯位面板。
    Array.prototype.slice.call(document.querySelectorAll('#discordNotifyPanel,.discord-notify-panel')).forEach(function(el){
      if(!el.closest('#view-notifications')) el.remove();
    });
  }

  function manualSwitchView(view){
    document.querySelectorAll('.tab').forEach(function(b){ b.classList.toggle('active', b.dataset.view === view); });
    document.querySelectorAll('.view').forEach(function(x){ x.classList.toggle('active', x.id === 'view-' + view); });
    document.querySelectorAll('.mobile-nav button[data-view]').forEach(function(b){ b.classList.toggle('active', b.dataset.view === view); });
    try{ history.replaceState(null, '', '#' + view); }catch(_){ }
  }

  function openNotifyView(){
    if(typeof window.switchView === 'function'){
      try{ window.switchView('notifications'); }catch(_){ manualSwitchView('notifications'); }
    }else{
      manualSwitchView('notifications');
    }
    setTimeout(loadNotifyPanel, 60);
  }

  function ensureNotifyTab(){
    removeStrayOldDiscordPanel();

    var tabs = document.querySelector('.tabs');
    if(tabs && !document.querySelector('.tab[data-view="notifications"]')){
      var b = document.createElement('button');
      b.className = 'tab';
      b.dataset.view = 'notifications';
      b.type = 'button';
      b.textContent = '通知';
      var ref = document.querySelector('.tab[data-view="maintenance"]') || document.querySelector('.tab[data-view="settings"]');
      tabs.insertBefore(b, ref || null);
    }

    var tab = document.querySelector('.tab[data-view="notifications"]');
    if(tab && !tab._notifyMoveBound){
      tab._notifyMoveBound = true;
      tab.addEventListener('click', function(ev){ ev.preventDefault(); openNotifyView(); }, true);
    }

    if(!q('view-notifications')){
      var sec = document.createElement('section');
      sec.id = 'view-notifications';
      sec.className = 'view';
      sec.innerHTML = notifyPanelHtml();
      var host = q('view-maintenance') || q('view-settings') || q('view-materials') || document.querySelector('.protected') || document.querySelector('main');
      if(host && host.parentNode) host.parentNode.insertBefore(sec, host);
      else (document.querySelector('main') || document.body).appendChild(sec);
    }else if(!q('notifyList')){
      q('view-notifications').innerHTML = notifyPanelHtml();
    }

    bindNotifyControls();
    ensureMobileNotifyEntry();
  }

  function notifyPanelHtml(){
    return ''+
      '<div class="panel notify-page-panel">'+
        '<div class="toolbar">'+
          '<div><h2>通知</h2><div class="small">Discord 通知開關已集中到這裡；可單獨設定點名、QR、數字點名、完成結果等通知。</div></div>'+
          '<div class="toolbar-right"><button id="notifyReload" class="secondary" type="button">重新整理</button></div>'+
        '</div>'+
        '<div class="notify-panel-grid">'+
          '<aside class="notify-side-card">'+
            '<label class="small" for="notifyAccount">設定帳號</label>'+
            '<select id="notifyAccount" class="notify-account-select"></select>'+
            '<div id="notifyInfo" class="small" style="margin-top:8px">載入中...</div>'+
            '<div class="notify-badge" style="margin-top:10px">Discord 通知</div>'+
          '</aside>'+
          '<main>'+
            '<div id="notifyList" class="notify-pref-list"><div class="small">載入中...</div></div>'+
            '<div class="notify-actions">'+
              '<button id="notifySave" type="button">儲存通知設定</button>'+
              '<button id="notifyAllOn" class="secondary" type="button">全部開啟</button>'+
              '<button id="notifyAllOff" class="secondary" type="button">全部關閉</button>'+
            '</div>'+
          '</main>'+
        '</div>'+
      '</div>';
  }

  function bindNotifyControls(){
    var reload = q('notifyReload');
    if(reload && !reload._notifyBound){ reload._notifyBound = true; reload.addEventListener('click', loadNotifyPanel); }
    var save = q('notifySave');
    if(save && !save._notifyBound){ save._notifyBound = true; save.addEventListener('click', saveNotifyPrefs); }
    var on = q('notifyAllOn');
    if(on && !on._notifyBound){ on._notifyBound = true; on.addEventListener('click', function(){ setAllNotify(true); }); }
    var off = q('notifyAllOff');
    if(off && !off._notifyBound){ off._notifyBound = true; off.addEventListener('click', function(){ setAllNotify(false); }); }
    var account = q('notifyAccount');
    if(account && !account._notifyBound){ account._notifyBound = true; account.addEventListener('change', loadNotifyPrefs); }
  }

  function ensureMobileNotifyEntry(){
    var sheets = Array.prototype.slice.call(document.querySelectorAll('#mobileSheet .sheet-grid,.sheet-grid,.mobile-sheet .sheet'));
    sheets.forEach(function(host){
      if(host.querySelector('[data-view="notifications"],#mobileNotifySettings')) return;
      var btn = document.createElement('button');
      btn.id = 'mobileNotifySettings';
      btn.type = 'button';
      btn.dataset.view = 'notifications';
      btn.textContent = '通知設定';
      btn.addEventListener('click', function(){
        var back = btn.closest('.visible');
        if(back) back.classList.remove('visible');
        openNotifyView();
      });
      var logout = host.querySelector('.danger,#mLogout,#mobileDashLogout,#mobileLogoutForce,#moreLogoutFinalV2');
      host.insertBefore(btn, logout || null);
    });
  }

  async function loadStatusOnce(){
    if(lastStatus) return lastStatus;
    lastStatus = await (window.TronclassRuntime.statusSummary ? window.TronclassRuntime.statusSummary(false) : window.TronclassRuntime.status(false, {include_history:false, history_limit:0}));
    return lastStatus;
  }

  async function loadNotifyPanel(){
    ensureNotifyTab();
    var list = q('notifyList');
    var sel = q('notifyAccount');
    if(!list || !sel) return;
    try{
      var st = await loadStatusOnce();
      var old = sel.value;
      sel.innerHTML = '';
      sel.disabled = false;
      var accounts = Array.isArray(st.accounts) ? st.accounts : [];
      if(st.is_admin){
        accounts.forEach(function(a){
          var id = a.account_id || a.id || a.username || '';
          if(!id) return;
          var name = a.user_name || a.name || a.display_name || a.username || '';
          sel.appendChild(new Option((id + (name ? '  ' + name : '')).trim(), id));
        });
        var preferred = (st.current_account_id && st.current_account_id !== '<token-admin>') ? st.current_account_id : '';
        if(old) sel.value = old;
        if(preferred && Array.prototype.some.call(sel.options, function(o){return o.value === preferred;})) sel.value = preferred;
        if(!sel.value && sel.options.length) sel.selectedIndex = 0;
      }else{
        var me = st.current_account_id || st.account_id || '';
        sel.appendChild(new Option(me || '目前帳號', me));
        sel.disabled = true;
      }
      await loadNotifyPrefs();
    }catch(e){
      list.innerHTML = '<div class="bad">通知設定載入失敗：' + esc(e.message || e) + '</div><div class="small">請確認已登入，且後端有 /admin/api/notifications/preferences。</div>';
      var info = q('notifyInfo');
      if(info) info.textContent = '讀取失敗';
    }
  }

  async function loadNotifyPrefs(){
    var list = q('notifyList');
    var sel = q('notifyAccount');
    if(!list || !sel) return;
    var account = sel.value || '';
    list.innerHTML = '<div class="small">載入中...</div>';
    try{
      var path = '/admin/api/notifications/preferences' + (account ? '?account_id=' + encodeURIComponent(account) : '');
      var body = await window.TronclassRuntime.api(path, {cache:'no-store'});
      var info = q('notifyInfo');
      if(info) info.textContent = '目前設定帳號：' + (body.account_id || account || '-');
      renderNotifyPrefs(body.preferences || []);
    }catch(e){
      list.innerHTML = '<div class="bad">讀取失敗：' + esc(e.message || e) + '</div>';
    }
  }

  function renderNotifyPrefs(prefs){
    var list = q('notifyList');
    if(!list) return;
    if(!prefs.length){
      list.innerHTML = '<div class="small">沒有通知項目。</div>';
      return;
    }
    list.innerHTML = prefs.map(function(p){
      return '<label class="notify-pref-item">'+
        '<input type="checkbox" data-notify-key="' + esc(p.key) + '" ' + (p.enabled ? 'checked' : '') + '>'+
        '<span><div class="notify-pref-title">' + esc(p.label) + '</div><div class="notify-pref-desc">' + esc(p.description || '') + '</div></span>'+
      '</label>';
    }).join('');
  }

  function setAllNotify(value){
    document.querySelectorAll('#view-notifications [data-notify-key]').forEach(function(cb){ cb.checked = value; });
  }

  async function saveNotifyPrefs(){
    var sel = q('notifyAccount');
    var account = sel && sel.value ? sel.value : '';
    var prefs = Array.prototype.slice.call(document.querySelectorAll('#view-notifications [data-notify-key]')).map(function(cb){
      return {key: cb.dataset.notifyKey, enabled: !!cb.checked};
    });
    try{
      await window.TronclassRuntime.api('/admin/api/notifications/preferences', {
        method: 'POST',
        headers: {'content-type':'application/json'},
        body: JSON.stringify({account_id: account || undefined, preferences: prefs})
      });
      showToast('Discord 通知設定已儲存');
    }catch(e){
      showToast(e.message || '儲存通知設定失敗', false);
    }
  }

  function boot(){
    ensureNotifyTab();
    // 若使用者直接開 /admin#notifications，立即切過去並載入。
    if((location.hash || '').replace('#','') === 'notifications'){
      setTimeout(openNotifyView, 80);
    }
    var n = 0;
    var timer = setInterval(function(){
      ensureNotifyTab();
      removeStrayOldDiscordPanel();
      n++;
      if(n >= 16) clearInterval(timer);
    }, 500);
    document.addEventListener('click', function(ev){
      var t = ev.target && ev.target.closest && ev.target.closest('[data-view="notifications"],#mobileNotifySettings');
      if(t) setTimeout(loadNotifyPanel, 80);
    }, true);
  }

  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', boot);
  else boot();
})();
/* classReminderNotifyScriptV1 */
(function(){
  'use strict';
  if(window.CLASS_REMINDER_NOTIFY_UI_V1) return;
  window.CLASS_REMINDER_NOTIFY_UI_V1 = true;

  var lastStatus = null;
  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g,function(ch){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch];}); }
  function token(){
    try{
      var u = new URLSearchParams(location.search).get('token');
      if(u) return String(u).trim();
      var keys=['tronclass_admin_session','tronclass_admin_token','adminToken','token'];
      for(var i=0;i<keys.length;i++){
        var v=localStorage.getItem(keys[i]) || sessionStorage.getItem(keys[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){ }
    return '';
  }
  function withToken(path){ var t=token(); return t ? path+(path.indexOf('?')>=0?'&':'?')+'token='+encodeURIComponent(t) : path; }
  async function parseResponse(res){ var b=await res.json().catch(function(){return {};}); if(!res.ok){var e=new Error(b.error||b.message||('HTTP '+res.status)); e.status=res.status; throw e;} return b; }
  async function api(path,opts){ return fetch(withToken(path),opts||{}).then(parseResponse); }
  function toast(msg, ok){ var el=q('toast'); if(el){ el.textContent=msg; el.className='toast visible '+(ok===false?'bad':'ok'); clearTimeout(el._classReminderTimer); el._classReminderTimer=setTimeout(function(){el.classList.remove('visible');},3000); } else alert(msg); }

  function manualSwitchView(view){
    document.querySelectorAll('.tab').forEach(function(b){ b.classList.toggle('active', b.dataset.view===view); });
    document.querySelectorAll('.view').forEach(function(x){ x.classList.toggle('active', x.id==='view-'+view); });
    document.querySelectorAll('.mobile-nav button[data-view]').forEach(function(b){ b.classList.toggle('active', b.dataset.view===view); });
    try{ history.replaceState(null,'','#'+view); }catch(_){ }
  }
  function openNotifications(){
    if(typeof window.switchView==='function'){ try{ window.switchView('notifications'); }catch(_){ manualSwitchView('notifications'); } }
    else manualSwitchView('notifications');
    setTimeout(loadClassReminderPanel,80);
  }

  function ensureNotificationView(){
    var tabs=document.querySelector('.tabs');
    if(tabs && !document.querySelector('.tab[data-view="notifications"]')){
      var b=document.createElement('button'); b.className='tab'; b.type='button'; b.dataset.view='notifications'; b.textContent='通知';
      var ref=document.querySelector('.tab[data-view="maintenance"]') || document.querySelector('.tab[data-view="settings"]');
      tabs.insertBefore(b, ref || null);
    }
    var tab=document.querySelector('.tab[data-view="notifications"]');
    if(tab && !tab._classReminderBound){ tab._classReminderBound=true; tab.addEventListener('click',function(ev){ ev.preventDefault(); openNotifications(); },true); }
    if(!q('view-notifications')){
      var sec=document.createElement('section'); sec.id='view-notifications'; sec.className='view';
      var host=q('view-maintenance') || q('view-settings') || q('view-materials') || document.querySelector('.protected') || document.querySelector('main');
      if(host && host.parentNode) host.parentNode.insertBefore(sec,host); else (document.querySelector('main')||document.body).appendChild(sec);
    }
    ensureMobileEntry();
  }

  function ensureMobileEntry(){
    var hosts=Array.prototype.slice.call(document.querySelectorAll('#mobileSheet .sheet-grid,.sheet-grid,.mobile-sheet .sheet'));
    hosts.forEach(function(host){
      if(host.querySelector('[data-view="notifications"],#mobileNotifySettings,#mobileClassReminderNotify')) return;
      var btn=document.createElement('button'); btn.type='button'; btn.id='mobileClassReminderNotify'; btn.dataset.view='notifications'; btn.textContent='通知設定';
      btn.onclick=function(){ var back=btn.closest('.visible'); if(back)back.classList.remove('visible'); openNotifications(); };
      var logout=host.querySelector('.danger,#mLogout,#mobileDashLogout,#mobileLogoutForce,#moreLogoutFinalV2');
      host.insertBefore(btn, logout||null);
    });
  }

  function ensurePanel(){
    ensureNotificationView();
    var view=q('view-notifications'); if(!view) return;
    if(q('classReminderPanel')) return;
    var panel=document.createElement('div');
    panel.id='classReminderPanel';
    panel.className='panel';
    panel.innerHTML=''+
      '<div class="toolbar"><div><h2>上課前 Discord 提醒</h2><div class="small">每個人可以自己設定上課前幾分鐘由 Discord DM 提醒。需要先填好課表並綁定 Discord User ID。</div></div><button id="classReminderReload" class="secondary" type="button">重新整理</button></div>'+ 
      '<div class="class-reminder-grid">'+
        '<aside class="class-reminder-side"><label class="small" for="classReminderAccount">設定帳號</label><select id="classReminderAccount" class="class-reminder-select"></select><div id="classReminderInfo" class="class-reminder-note">載入中...</div></aside>'+ 
        '<main class="class-reminder-card">'+
          '<label class="class-reminder-toggle"><input id="classReminderEnabled" type="checkbox"><span><div>開啟 Discord 上課提醒</div><div class="class-reminder-note">關閉後不會發送上課前提醒，但不影響點名通知。</div></span></label>'+ 
          '<div class="class-reminder-row"><div><label for="classReminderMinutes" style="font-weight:900">上課前幾分鐘提醒</label><div class="class-reminder-note">可輸入 0～180 分鐘；0 代表上課當下提醒。</div></div><input id="classReminderMinutes" class="class-reminder-input" type="number" min="0" max="180" step="1" value="10"></div>'+ 
          '<div class="class-reminder-presets"><button type="button" class="secondary" data-class-min="5">5 分鐘</button><button type="button" class="secondary" data-class-min="10">10 分鐘</button><button type="button" class="secondary" data-class-min="15">15 分鐘</button><button type="button" class="secondary" data-class-min="30">30 分鐘</button></div>'+ 
          '<div class="class-reminder-actions"><button id="classReminderSave" type="button">儲存上課提醒設定</button><a class="button secondary" id="classReminderTimetableLink" href="/admin#timetable">前往課表</a></div>'+ 
        '</main>'+ 
      '</div>';
    view.insertBefore(panel, view.firstChild);
    q('classReminderReload').onclick=loadClassReminderPanel;
    q('classReminderSave').onclick=saveClassReminder;
    q('classReminderAccount').onchange=loadClassReminderPrefs;
    Array.prototype.slice.call(panel.querySelectorAll('[data-class-min]')).forEach(function(btn){ btn.onclick=function(){ q('classReminderMinutes').value=btn.dataset.classMin; }; });
  }

  async function loadStatus(){ if(lastStatus) return lastStatus; lastStatus=await (window.TronclassRuntime.statusSummary?window.TronclassRuntime.statusSummary(false):window.TronclassRuntime.status(false,{include_history:false,history_limit:0})); return lastStatus; }

  async function loadClassReminderPanel(){
    ensurePanel();
    var sel=q('classReminderAccount'); if(!sel) return;
    try{
      var st=await loadStatus();
      var old=sel.value; sel.innerHTML=''; sel.disabled=false;
      var accounts=Array.isArray(st.accounts)?st.accounts:[];
      if(st.is_admin){
        accounts.forEach(function(a){ var id=a.account_id||a.id||a.username||''; if(!id)return; var name=a.user_name||a.name||a.display_name||a.username||''; sel.appendChild(new Option((id+(name?'  '+name:'')).trim(),id)); });
        if(old) sel.value=old;
        var cur=(st.current_account_id&&st.current_account_id!=='<token-admin>')?st.current_account_id:'';
        if(cur && Array.prototype.some.call(sel.options,function(o){return o.value===cur;})) sel.value=cur;
        if(!sel.value && sel.options.length) sel.selectedIndex=0;
      }else{
        var me=st.current_account_id||st.account_id||''; sel.appendChild(new Option(me||'目前帳號',me)); sel.disabled=true;
      }
      await loadClassReminderPrefs();
    }catch(e){ var info=q('classReminderInfo'); if(info) info.textContent='載入失敗：'+(e.message||e); }
  }

  async function loadClassReminderPrefs(){
    var sel=q('classReminderAccount'); if(!sel)return; var account=sel.value||'';
    try{
      var body=await window.TronclassRuntime.api('/admin/api/class_reminder'+(account?'?account_id='+encodeURIComponent(account):''),{cache:'no-store'});
      q('classReminderEnabled').checked=!!body.discord_enabled;
      q('classReminderMinutes').value=Number(body.minutes_before==null?10:body.minutes_before);
      q('classReminderInfo').textContent='目前設定帳號：'+(body.account_id||account||'-');
      var tt=q('classReminderTimetableLink'); if(tt)tt.href=withToken('/admin#timetable');
    }catch(e){ q('classReminderInfo').textContent='讀取失敗：'+(e.message||e); }
  }

  async function saveClassReminder(){
    var sel=q('classReminderAccount'); var account=sel&&sel.value?sel.value:'';
    var minutes=parseInt(q('classReminderMinutes').value||'10',10); if(!Number.isFinite(minutes)) minutes=10; minutes=Math.max(0,Math.min(180,minutes));
    try{
      await window.TronclassRuntime.api('/admin/api/class_reminder',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({account_id:account||undefined,discord_enabled:!!q('classReminderEnabled').checked,minutes_before:minutes})});
      q('classReminderMinutes').value=minutes;
      toast('上課前 Discord 提醒設定已儲存');
      await loadClassReminderPrefs();
    }catch(e){ toast(e.message||'儲存上課提醒設定失敗',false); }
  }

  function boot(){
    ensureNotificationView(); ensurePanel();
    if((location.hash||'').replace('#','')==='notifications') setTimeout(loadClassReminderPanel,120);
    var n=0,t=setInterval(function(){ ensureNotificationView(); ensurePanel(); n++; if(n>=12)clearInterval(t); },600);
    document.addEventListener('click',function(ev){ var x=ev.target&&ev.target.closest&&ev.target.closest('[data-view="notifications"],#mobileClassReminderNotify,#mobileNotifySettings'); if(x)setTimeout(loadClassReminderPanel,120); },true);
  }
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot); else boot();
})();
  }
  if(window.TronclassDashboard&&window.TronclassDashboard.ready) start();
  else window.addEventListener('tronclass:dashboard:ready',start,{once:true});
})();
