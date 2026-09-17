/* MOCK_ROLLCALL_MODULE_AFTER_READY_PHASE4D3 */
/* NOTIFICATIONS_STANDALONE_PAGE_V1: 通知設定已改為獨立頁面 /admin/notifications，
 * 本檔不再建立「通知」分頁與通知偏好面板，僅保留模擬點名測試與「點名排程」入口。 */
(function(){
  function start(){
/* MOCK_ROLLCALL_MODULE_PHASE4D */
(function(){
  'use strict';
  if (window.RESTORE_MOCK_NOTIFY_DASHBOARD_V1) return;
  window.RESTORE_MOCK_NOTIFY_DASHBOARD_V1 = true;
  if (!(location.pathname === '/admin' || location.pathname === '/admin/' || location.pathname === '/admin/dashboard')) return;

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
    if(view==='rollcall') setTimeout(loadMockAccounts,80);
  }

  // 手機「更多」面板的「點名排程」入口（原本依附在通知按鈕之後，改為獨立插入）。
  function ensureScheduleEntry(){
    var sheets=document.querySelectorAll('#mobileMoreFinalV2 .more-grid,.sheet-grid');
    sheets.forEach(function(host){
      if(host.querySelector('[data-detection-schedule-entry]')) return;
      var schedule=document.createElement('button');
      schedule.type='button';
      schedule.dataset.detectionScheduleEntry='1';
      schedule.textContent='點名排程';
      schedule.onclick=function(){
        var back=host.closest('.visible');
        if(back)back.classList.remove('visible');
        location.href='/admin/detection-schedule';
      };
      var logout=host.querySelector('.danger,#mobileDashLogout,#mobileLogoutForce,#moreLogoutFinalV2');
      host.insertBefore(schedule,logout||null);
    });
  }

  async function loadStatusOnce(){ notifyStatus=await (window.TronclassRuntime&&window.TronclassRuntime.statusSummary?window.TronclassRuntime.statusSummary(false):api('/admin/api/status?include_history=false&history_limit=0',{cache:'no-store'})); return notifyStatus; }

  function ensureMockPanel(){
    var view=q('view-rollcall'); if(!view)return;
    if(q('mockRollcallPanel'))return;
    var panel=document.createElement('section'); panel.id='mockRollcallPanel'; panel.className='panel mock-panel';
    panel.innerHTML='<div class="toolbar"><div><h2>模擬點名測試</h2><div class="small">用來測試數字點名、QR Code 點名、雷達點名、Discord 通知與 Web 通知。</div></div><div class="toolbar-right"><button id="mockSelectAll" class="secondary" type="button">全選</button><button id="mockClearAll" class="secondary" type="button">清除</button></div></div><div class="mock-grid"><div class="mock-form"><select id="mockKind"><option value="number">數字點名</option><option value="qr">QR Code 點名</option><option value="radar">雷達點名</option></select><input id="mockCode" value="1234" placeholder="數字點名代碼"><input id="mockInterval" type="number" min="1" max="120" value="15" placeholder="QR 秒數"><input id="mockRadarLat" class="hidden-init" type="number" step="0.000001" value="25.0478" placeholder="模擬教室緯度"><input id="mockRadarLon" class="hidden-init" type="number" step="0.000001" value="121.5170" placeholder="模擬教室經度"><input id="mockRadarRadius" class="hidden-init" type="number" min="10" max="5000" value="100" placeholder="判定半徑（公尺）"><input id="mockRadarClass" class="hidden-init" type="number" min="0" max="100" value="0" placeholder="模擬班上到課率 %"><div id="mockRadarHint" class="small hidden-init">模擬學校的判定：座標離教室 ≤ 判定半徑才算成功（否則測試頁會顯示「距離不足」）。到課率填 0 ＝模擬查不到班上進度；超過 15% 時系統會自動用教室座標送出一次。</div><button id="triggerMock" type="button">觸發 Mock 點名</button></div><div><div id="mockAccounts" class="mock-accounts-grid"><div class="small">載入帳號中...</div></div><div id="mockResult" class="mock-result"><div id="mockResultText" class="small"></div><img id="mockQrImg" alt="Mock QR" class="hidden-init"><div id="mockQrPayload" class="mock-payload"></div><div class="mock-actions"><a id="mockOpenUrl" class="button secondary hidden-init" href="#" target="_blank" rel="noopener">開啟測試頁</a><button id="stopMockQr" class="secondary hidden-init" type="button">停止 QR 預覽</button></div></div></div></div>';
    view.appendChild(panel);
    q('mockSelectAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=true;}); };
    q('mockClearAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=false;}); };
    q('triggerMock').onclick=triggerMockRollcall;
    q('mockKind').onchange=syncMockFields;
    syncMockFields();
    q('stopMockQr').onclick=stopMockQr;
    loadMockAccounts();
  }
  var MOCK_RADAR_FIELD_IDS=['mockRadarLat','mockRadarLon','mockRadarRadius','mockRadarClass'];
  /* MOCK_RADAR_V1：依點名類型顯示對應欄位（數字＝代碼、QR＝秒數、雷達＝教室與判定條件） */
  function syncMockFields(){
    var kind=q('mockKind')?String(q('mockKind').value||'number'):'number';
    MOCK_RADAR_FIELD_IDS.forEach(function(id){var el=q(id);if(el)el.classList.toggle('hidden-init',kind!=='radar');});
    var hint=q('mockRadarHint'); if(hint)hint.classList.toggle('hidden-init',kind!=='radar');
    var code=q('mockCode'); if(code)code.classList.toggle('hidden-init',kind!=='number');
    var interval=q('mockInterval'); if(interval)interval.classList.toggle('hidden-init',kind!=='qr');
  }
  async function loadMockAccounts(){ ensureMockPanel(); try{ var st=notifyStatus||await loadStatusOnce(); var isAdmin=!!st.is_admin; var own=String(st.current_account_id||''); var accounts=isAdmin?(st.accounts||[]):(st.accounts||[]).filter(function(a){return String(a.account_id||'')===own;}); var html=accounts.map(function(a){var locked=!isAdmin?' checked disabled':'';return '<label class="mock-account"><input type="checkbox" value="'+esc(a.account_id)+'"'+locked+'><span>'+esc(a.account_id)+' <span class="small">'+esc(a.user_name||a.username||'')+'</span></span></label>';}).join(''); q('mockAccounts').innerHTML=html||'<div class="small">沒有可操作的帳號。</div>'; q('mockSelectAll').style.display=isAdmin?'':'none'; q('mockClearAll').style.display=isAdmin?'':'none'; }catch(e){ q('mockAccounts').innerHTML='<div class="bad">帳號讀取失敗：'+esc(e.message||e)+'</div>'; } }
  function stopMockQr(){ if(mockQrTimer)clearInterval(mockQrTimer); mockQrTimer=null; q('mockQrImg').style.display='none'; q('stopMockQr').style.display='none'; }
  function startMockQrPreview(body){ stopMockQr(); if(!body.qr_base||!body.rollcall_id)return; var interval=Math.max(1,Number(body.interval_secs||15)); function render(){ var slot=Math.floor(Date.now()/1000/interval); var payload='0~100!3~'+body.qr_base+'_'+slot+'!4~'+body.rollcall_id; q('mockQrPayload').textContent=payload; q('mockQrImg').src='/admin/api/mock/qr.svg?data='+encodeURIComponent(payload)+'&t='+Date.now(); q('mockQrImg').style.display='block'; } render(); mockQrTimer=setInterval(render,1000); q('stopMockQr').style.display='inline-flex'; }
  async function triggerMockRollcall(){ var selected=Array.prototype.slice.call(document.querySelectorAll('#mockAccounts input:checked')).map(function(i){return i.value;}); if(!selected.length)return toast('請至少選一個帳號',false); var kind=String(q('mockKind').value||'number'); try{
    var payload={kind:kind,selected_accounts:selected};
    if(kind==='number'){ payload.code=q('mockCode').value.trim()||'1234'; }
    if(kind==='qr'){ payload.interval_secs=Number(q('mockInterval').value||15); }
    if(kind==='radar'){
      /* 教室座標留空＝用後端的預設模擬教室；有填就一定要合法（不要把打錯的座標當教室）。 */
      var latRaw=String(q('mockRadarLat').value==null?'':q('mockRadarLat').value).trim();
      var lonRaw=String(q('mockRadarLon').value==null?'':q('mockRadarLon').value).trim();
      if(latRaw||lonRaw){
        var lat=Number(latRaw),lon=Number(lonRaw);
        if(!isFinite(lat)||!isFinite(lon)||lat<-90||lat>90||lon<-180||lon>180)return toast('模擬教室座標不合法（緯度 -90~90、經度 -180~180）',false);
        if(lat===0&&lon===0)return toast('模擬教室座標不能是 0, 0',false);
        payload.radar_latitude=lat; payload.radar_longitude=lon;
      }
      var radius=Number(String(q('mockRadarRadius').value==null?'':q('mockRadarRadius').value).trim());
      if(isFinite(radius)&&radius>0)payload.radar_radius_m=radius;
      var classPercent=Number(String(q('mockRadarClass').value==null?'':q('mockRadarClass').value).trim());
      if(isFinite(classPercent)&&classPercent>0)payload.radar_class_percent=Math.round(classPercent);
    }
    var body=await api('/admin/api/mock/start',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(payload)});
    q('mockResult').classList.add('visible');
    var label=kind==='qr'?'QR Code':(kind==='radar'?'雷達':'數字');
    var extra='';
    if(body.kind==='radar'&&body.radar){ extra='（模擬教室 '+Number(body.radar.latitude).toFixed(6)+', '+Number(body.radar.longitude).toFixed(6)+'／判定半徑 '+body.radar.radius_m+' 公尺／班上到課率 '+body.radar.class_percent+'%）。同一個模擬點名共用一組座標：任一個測試頁送出且距離合格，這個點名下的帳號都會一起完成。'; }
    q('mockResultText').textContent='已觸發 Mock '+label+' 點名，帳號 '+selected.length+' 個。'+extra+(body.url?'　請打開測試頁送出位置或掃碼。':'');
    if(body.url){q('mockOpenUrl').href=body.url; q('mockOpenUrl').style.display='inline-flex';}
    if(body.kind==='qr')startMockQrPreview(body); else stopMockQr();
    toast('Mock 點名已觸發');
  }catch(e){toast(e.message||'Mock 點名失敗',false);} }

  function boot(){ ensureScheduleEntry(); ensureMockPanel(); var n=0,t=setInterval(function(){ ensureScheduleEntry(); ensureMockPanel(); n++; if(n>=12)clearInterval(t); },500); document.addEventListener('click',function(ev){ if(ev.target.closest&&ev.target.closest('[data-view="rollcall"]'))setTimeout(loadMockAccounts,80); },true); }
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot);else boot();
})();

  }
  if(window.TronclassDashboard&&window.TronclassDashboard.ready) start();
  else window.addEventListener('tronclass:dashboard:ready',start,{once:true});
})();
