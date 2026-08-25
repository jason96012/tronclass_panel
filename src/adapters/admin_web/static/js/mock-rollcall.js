/* MOCK_ROLLCALL_MODULE_AFTER_READY_PHASE4D3 */
(function(){
  function start(){
/* MOCK_ROLLCALL_MODULE_PHASE4D */
(function(){
  'use strict';
  if (window.RESTORE_MOCK_NOTIFY_DASHBOARD_V1) return;
  window.RESTORE_MOCK_NOTIFY_DASHBOARD_V1 = true;
  if (!(location.pathname === '/admin' || location.pathname === '/admin/' || location.pathname === '/admin/dashboard')) return;

  var LS_SESSION = 'tronclass_admin_session';
  var notifyStatus = null;
  var mockQrTimer = null;

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g, function(ch){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]; }); }
  function toast(text, ok){ var el=q('toast'); if(el){el.textContent=text;el.className='toast visible '+(ok===false?'bad':'ok');clearTimeout(el._timer);el._timer=setTimeout(function(){el.classList.remove('visible');},3200)} else alert(text); }
  async function parseResponse(res){ var body=await res.json().catch(function(){return{}}); if(!res.ok){var e=new Error(body.error||('HTTP '+res.status)); e.status=res.status; throw e;} return body; }
  async function api(path, opts){ opts=opts||{}; opts.credentials=opts.credentials||'same-origin'; opts.cache=opts.cache||'no-store'; return fetch(path, opts).then(parseResponse); }


  function switchView(view){
    var tab=document.querySelector('.tab[data-view="'+view+'"]');
    if(tab) tab.click();
    else {
      document.querySelectorAll('.tab').forEach(function(t){t.classList.toggle('active',t.dataset.view===view);});
      document.querySelectorAll('.view').forEach(function(v){v.classList.toggle('active',v.id==='view-'+view);});
      history.replaceState(null,'','#'+view);
    }
    if(view==='notifications') setTimeout(loadNotifyPanel,80);
    if(view==='rollcall') setTimeout(loadMockAccounts,80);
  }

  function ensureNotifyTab(){
    var tabs=document.querySelector('.tabs');
    if(tabs && !document.querySelector('.tab[data-view="notifications"]')){
      var b=document.createElement('button');
      b.className='tab'; b.dataset.view='notifications'; b.type='button'; b.textContent='通知';
      var ref=document.querySelector('.tab[data-view="maintenance"]') || document.querySelector('.tab[data-view="settings"]');
      tabs.insertBefore(b, ref || null);
      b.onclick=function(){switchView('notifications');};
    }
    if(!q('view-notifications')){
      var sec=document.createElement('section');
      sec.id='view-notifications'; sec.className='view';
      sec.innerHTML='<div class="panel"><div class="toolbar"><div><h2>通知設定</h2><div class="small">設定 Discord 通知項目。管理員可切換帳號；一般使用者只能設定自己。</div></div><button id="notifyReload" class="secondary" type="button">重新整理</button></div><div class="notify-panel-grid"><aside><label class="small">帳號</label><select id="notifyAccount" class="notify-account-select"></select><div id="notifyInfo" class="small" style="margin-top:8px">載入中...</div></aside><main><div id="notifyList" class="notify-pref-list"><div class="small">載入中...</div></div><div class="notify-actions"><button id="notifySave" type="button">儲存通知設定</button><button id="notifyAllOn" class="secondary" type="button">全部開啟</button><button id="notifyAllOff" class="secondary" type="button">全部關閉</button></div></main></div></div>';
      var after=q('view-timetable')||q('view-materials')||q('view-rollcall')||document.querySelector('.view:last-of-type');
      if(after&&after.parentNode)after.parentNode.insertBefore(sec, after.nextSibling); else (document.querySelector('main')||document.body).appendChild(sec);
      q('notifyReload').onclick=loadNotifyPanel; q('notifySave').onclick=saveNotifyPrefs; q('notifyAllOn').onclick=function(){setAllPrefs(true)}; q('notifyAllOff').onclick=function(){setAllPrefs(false)}; q('notifyAccount').onchange=loadNotifyPrefs;
    }
    // Mobile more sheet support.
    var sheets=document.querySelectorAll('.mobile-dashboard-sheet-grid,.sheet-grid,.mobile-dashboard-sheet .grid,#mobileDashboardSheetForce .grid,#mobileMoreFinalV2 .more-grid,#mobileStableDashboardSheet .grid');
    sheets.forEach(function(host){
      var notify=host.querySelector('[data-view="notifications"]');
      if(!notify){
        notify=document.createElement('button');
        notify.type='button';
        notify.dataset.view='notifications';
        notify.textContent='通知設定';
        notify.onclick=function(){ var back=host.closest('.visible'); if(back)back.classList.remove('visible'); switchView('notifications'); };
        var logout=host.querySelector('.danger,#mobileDashLogout,#mobileLogoutForce,#moreLogoutFinalV2');
        host.insertBefore(notify,logout||null);
      }
      if(!host.querySelector('[data-detection-schedule-entry]')){
        var schedule=document.createElement('button');
        schedule.type='button';
        schedule.dataset.detectionScheduleEntry='1';
        schedule.textContent='點名排程';
        schedule.onclick=function(){
          var back=host.closest('.visible');
          if(back)back.classList.remove('visible');
          location.href='/admin/detection-schedule';
        };
        notify.insertAdjacentElement('afterend',schedule);
      }
    });
  }

  async function loadStatusOnce(){ notifyStatus=await (window.TronclassRuntime&&window.TronclassRuntime.statusSummary?window.TronclassRuntime.statusSummary(false):api('/admin/api/status?include_history=false&history_limit=0',{cache:'no-store'})); return notifyStatus; }
  async function loadNotifyPanel(){ ensureNotifyTab(); try{ var st=notifyStatus||await loadStatusOnce(); var sel=q('notifyAccount'); var old=sel.value; sel.innerHTML=''; if(st.is_admin){ (st.accounts||[]).forEach(function(a){ sel.appendChild(new Option((a.account_id||'')+' '+(a.user_name||a.username||''), a.account_id)); }); if(st.current_account_id&&st.current_account_id!=='<token-admin>') sel.value=st.current_account_id; } else { sel.appendChild(new Option(st.current_account_id, st.current_account_id)); sel.disabled=true; } if(old) sel.value=old; await loadNotifyPrefs(); } catch(e){ q('notifyList').innerHTML='<div class="bad">通知設定 API 讀取失敗：'+esc(e.message||e)+'</div><div class="small">請確認後端已安裝 /admin/api/notifications/preferences。</div>'; } }
  async function loadNotifyPrefs(){ var account=q('notifyAccount')&&q('notifyAccount').value; var path='/admin/api/notifications/preferences'+(account?'?account_id='+encodeURIComponent(account):''); try{ var body=await api(path,{cache:'no-store'}); q('notifyInfo').textContent='目前設定帳號：'+(body.account_id||account||'-'); renderNotifyPrefs(body.preferences||[]); } catch(e){ q('notifyList').innerHTML='<div class="bad">讀取失敗：'+esc(e.message||e)+'</div>'; } }
  function renderNotifyPrefs(prefs){ q('notifyList').innerHTML=prefs.length?prefs.map(function(p){ return '<label class="notify-pref-item"><input type="checkbox" data-notify-key="'+esc(p.key)+'" '+(p.enabled?'checked':'')+'><span><div class="notify-pref-title">'+esc(p.label)+'</div><div class="notify-pref-desc">'+esc(p.description||'')+'</div></span></label>'; }).join(''):'<div class="small">沒有通知項目。</div>'; }
  function setAllPrefs(v){ document.querySelectorAll('[data-notify-key]').forEach(function(cb){cb.checked=v;}); }
  async function saveNotifyPrefs(){ var account=q('notifyAccount')&&q('notifyAccount').value; var prefs=Array.prototype.slice.call(document.querySelectorAll('[data-notify-key]')).map(function(cb){return{key:cb.dataset.notifyKey,enabled:cb.checked};}); try{ await api('/admin/api/notifications/preferences',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({account_id:account||undefined,preferences:prefs})}); toast('Discord 通知設定已儲存'); } catch(e){ toast(e.message||'儲存通知設定失敗',false); } }

  function ensureMockPanel(){
    var view=q('view-rollcall'); if(!view)return;
    if(q('mockRollcallPanel'))return;
    var panel=document.createElement('section'); panel.id='mockRollcallPanel'; panel.className='panel mock-panel';
    panel.innerHTML='<div class="toolbar"><div><h2>模擬點名測試</h2><div class="small">用來測試數字點名、QR Code 點名、Discord 通知與 Web 通知。</div></div><div class="toolbar-right"><button id="mockSelectAll" class="secondary" type="button">全選</button><button id="mockClearAll" class="secondary" type="button">清除</button></div></div><div class="mock-grid"><div class="mock-form"><select id="mockKind"><option value="number">數字點名</option><option value="qr">QR Code 點名</option></select><input id="mockCode" value="1234" placeholder="數字點名代碼"><input id="mockInterval" type="number" min="1" max="120" value="15" placeholder="QR 秒數"><button id="triggerMock" type="button">觸發 Mock 點名</button></div><div><div id="mockAccounts" class="mock-accounts-grid"><div class="small">載入帳號中...</div></div><div id="mockResult" class="mock-result"><div id="mockResultText" class="small"></div><img id="mockQrImg" alt="Mock QR" style="display:none"><div id="mockQrPayload" class="mock-payload"></div><div class="mock-actions"><a id="mockOpenUrl" class="button secondary" href="#" target="_blank" rel="noopener" style="display:none">開啟測試頁</a><button id="stopMockQr" class="secondary" type="button" style="display:none">停止 QR 預覽</button></div></div></div></div>';
    view.appendChild(panel);
    q('mockSelectAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=true;}); };
    q('mockClearAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=false;}); };
    q('triggerMock').onclick=triggerMockRollcall;
    q('stopMockQr').onclick=stopMockQr;
    loadMockAccounts();
  }
  async function loadMockAccounts(){ ensureMockPanel(); try{ var st=notifyStatus||await loadStatusOnce(); var isAdmin=!!st.is_admin; var own=String(st.current_account_id||''); var accounts=isAdmin?(st.accounts||[]):(st.accounts||[]).filter(function(a){return String(a.account_id||'')===own;}); var html=accounts.map(function(a){var locked=!isAdmin?' checked disabled':'';return '<label class="mock-account"><input type="checkbox" value="'+esc(a.account_id)+'"'+locked+'><span>'+esc(a.account_id)+' <span class="small">'+esc(a.user_name||a.username||'')+'</span></span></label>';}).join(''); q('mockAccounts').innerHTML=html||'<div class="small">沒有可操作的帳號。</div>'; q('mockSelectAll').style.display=isAdmin?'':'none'; q('mockClearAll').style.display=isAdmin?'':'none'; }catch(e){ q('mockAccounts').innerHTML='<div class="bad">帳號讀取失敗：'+esc(e.message||e)+'</div>'; } }
  function stopMockQr(){ if(mockQrTimer)clearInterval(mockQrTimer); mockQrTimer=null; q('mockQrImg').style.display='none'; q('stopMockQr').style.display='none'; }
  function startMockQrPreview(body){ stopMockQr(); if(!body.qr_base||!body.rollcall_id)return; var interval=Math.max(1,Number(body.interval_secs||15)); function render(){ var slot=Math.floor(Date.now()/1000/interval); var payload='0~100!3~'+body.qr_base+'_'+slot+'!4~'+body.rollcall_id; q('mockQrPayload').textContent=payload; q('mockQrImg').src='/admin/api/mock/qr.svg?data='+encodeURIComponent(payload)+'&t='+Date.now(); q('mockQrImg').style.display='block'; } render(); mockQrTimer=setInterval(render,1000); q('stopMockQr').style.display='inline-flex'; }
  async function triggerMockRollcall(){ var selected=Array.prototype.slice.call(document.querySelectorAll('#mockAccounts input:checked')).map(function(i){return i.value;}); if(!selected.length)return toast('請至少選一個帳號',false); try{ var body=await api('/admin/api/mock/start',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({kind:q('mockKind').value,selected_accounts:selected,code:q('mockCode').value.trim()||'1234',interval_secs:Number(q('mockInterval').value||15)})}); q('mockResult').classList.add('visible'); q('mockResultText').textContent='已觸發 Mock '+(body.kind==='qr'?'QR Code':'數字')+' 點名，帳號 '+selected.length+' 個。'; if(body.url){q('mockOpenUrl').href=body.url; q('mockOpenUrl').style.display='inline-flex';} if(body.kind==='qr')startMockQrPreview(body); else stopMockQr(); toast('Mock 點名已觸發'); }catch(e){toast(e.message||'Mock 點名失敗',false);} }

  function boot(){ ensureNotifyTab(); ensureMockPanel(); var n=0,t=setInterval(function(){ ensureNotifyTab(); ensureMockPanel(); n++; if(n>=12)clearInterval(t); },500); document.addEventListener('click',function(ev){ if(ev.target.closest&&ev.target.closest('[data-view="notifications"]'))setTimeout(loadNotifyPanel,80); if(ev.target.closest&&ev.target.closest('[data-view="rollcall"]'))setTimeout(loadMockAccounts,80); },true); if(location.hash==='#notifications')switchView('notifications'); }
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot);else boot();
})();

  }
  if(window.TronclassDashboard&&window.TronclassDashboard.ready) start();
  else window.addEventListener('tronclass:dashboard:ready',start,{once:true});
})();
