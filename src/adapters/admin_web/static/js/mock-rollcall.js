(function(){
  var q=function(id){return document.getElementById(id);};
  var toast=function(msg,ok){var el=document.getElementById('mockToast')||(function(){var e=document.createElement('div');e.id='mockToast';e.style.cssText='position:fixed;bottom:20px;right:20px;padding:10px 16px;background:#333;color:#fff;border-radius:4px;z-index:9999;display:none;';document.body.appendChild(e);return e;})();e.textContent=msg;e.style.background=ok?'#2e7d32':'#c62828';e.style.display='block';setTimeout(function(){e.style.display='none';},3000);};
  var isDashboardAdmin=function(){return window.isDashboardAdmin||function(){return false;};};
  var loadStatusOnce=function(){return window.loadStatusOnce||function(){return Promise.resolve({});};};
  var ensureMockPanel=function(){
    if(q('mockPanel'))return;
    var panel=document.createElement('div');panel.id='mockPanel';panel.style.cssText='margin:20px 0;padding:16px;border:1px solid #ccc;border-radius:4px;background:#fafafa;';
    panel.innerHTML='<div class="toolbar"><div><h2>模擬點名測試</h2><div class="small">用來測試數字點名、QR Code 點名、雷達點名、Discord 通知與 Web 通知。</div></div><div class="toolbar-right"><button id="mockSelectAll" class="secondary" type="button">全選</button><button id="mockClearAll" class="secondary" type="button">清除</button><button id="mockSimulateAll" class="secondary" type="button">模擬人數</button></div></div><div class="mock-grid"><div class="mock-form"><select id="mockKind"><option value="number">數字點名</option><option value="qr">QR Code 點名</option><option value="radar">雷達點名</option></select><input id="mockCode" value="1234" placeholder="數字點名代碼"><input id="mockInterval" type="number" min="1" max="120" value="15" placeholder="QR 秒數"><input id="mockCount" type="number" min="1" max="128" value="1" placeholder="模擬人數（1-128）"><input id="mockRadarLat" class="hidden-init" type="number" step="0.000001" value="25.0478" placeholder="模擬教室緯度"><input id="mockRadarLon" class="hidden-init" type="number" step="0.000001" value="121.5170" placeholder="模擬教室經度"><input id="mockRadarRadius" class="hidden-init" type="number" min="10" max="5000" value="100" placeholder="判定半徑（公尺）"><input id="mockRadarClass" class="hidden-init" type="number" min="0" max="100" value="0" placeholder="模擬班上到課率 %"><div id="mockRadarHint" class="small hidden-init">模擬學校的判定：座標離教室 ≤ 判定半徑才算成功（否則測試頁會顯示「距離不足」）。到課率填 0 ＝模擬查不到班上進度；超過 15% 時系統會自動用教室座標送出一次。</div><button id="triggerMock" type="button">觸發 Mock 點名</button></div><div><div id="mockAccounts" class="mock-accounts-grid"><div class="small">載入帳號中...</div></div><div id="mockResult" class="mock-result"><div id="mockResultText" class="small"></div><img id="mockQrImg" alt="Mock QR" class="hidden-init"><div id="mockQrPayload" class="mock-payload"></div><div class="mock-actions"><a id="mockOpenUrl" class="button secondary hidden-init" href="#" target="_blank" rel="noopener">開啟測試頁</a><button id="stopMockQr" class="secondary hidden-init" type="button">停止 QR 預覽</button></div></div></div></div>';
    document.querySelector('.dashboard-main,.view-rollcall,.admin-panel').appendChild(panel);
  };
  var mockQrTimer=null;
  var stopMockQr=function(){ if(mockQrTimer)clearInterval(mockQrTimer); mockQrTimer=null; q('mockQrImg').style.display='none'; q('stopMockQr').style.display='none'; };
  var startMockQrPreview=function(body){ stopMockQr(); if(!body.qr_base||!body.rollcall_id)return; var interval=Math.max(1,Number(body.interval_secs||15)); function render(){ var slot=Math.floor(Date.now()/1000/interval); var payload='0~100!3~'+body.qr_base+'_'+slot+'!4~'+body.rollcall_id; q('mockQrPayload').textContent=payload; q('mockQrImg').src='/admin/api/mock/qr.svg?data='+encodeURIComponent(payload)+'&t='+Date.now(); q('mockQrImg').style.display='block'; } render(); mockQrTimer=setInterval(render,1000); q('stopMockQr').style.display='inline-flex'; };
  var loadMockAccounts=function(){ if(!isDashboardAdmin()) return; ensureMockPanel(); try{ var st=notifyStatus||await loadStatusOnce(); var isAdmin=!!st.is_admin; var own=String(st.current_account_id||''); var accounts=isAdmin?(st.accounts||[]):(st.accounts||[]).filter(function(a){return String(a.account_id||'')===own;}); var html=accounts.map(function(a){var locked=!isAdmin?' checked disabled':'';return '<label class="mock-account"><input type="checkbox" value="'+esc(a.account_id)+'"'+locked+'><span>'+esc(a.account_id)+' <span class="small">'+esc(a.user_name||a.username||'')+'</span></span></label>';}).join(''); q('mockAccounts').innerHTML=html||'<div class="small">沒有可操作的帳號。</div>'; q('mockSelectAll').style.display=isAdmin?'':'none'; q('mockClearAll').style.display=isAdmin?'':'none'; }catch(e){ q('mockAccounts').innerHTML='<div class="bad">帳號讀取失敗：'+esc(e.message||e)+'</div>'; } };
  var esc=function(s){return String(s||'').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');};
  q('mockSelectAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=true;}); };
  q('mockClearAll').onclick=function(){ document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=false;}); };
  q('mockSimulateAll').onclick=function(){ var n=Number(q('mockCount').value||1); if(n<1||n>128)return toast('模擬人數須為 1-128',false); document.querySelectorAll('#mockAccounts input[type="checkbox"]').forEach(function(i){i.checked=false;}); toast('已清空真實帳號，準備模擬 '+n+' 人',true); };
  q('triggerMock').onclick=async function(){ var selected=Array.prototype.slice.call(document.querySelectorAll('#mockAccounts input:checked')).map(function(i){return i.value;}); var count=Number(q('mockCount').value||1); if(count<1||count>128)return toast('模擬人數須為 1-128',false); if(selected.length===0 && count===0)return toast('請選帳號或輸入模擬人數',false); var kind=String(q('mockKind').value||'number'); try{
    var payload={kind:kind,selected_accounts:selected,simulate_count:count};
    if(kind==='number'){ payload.code=q('mockCode').value.trim()||'1234'; }
    if(kind==='qr'){ payload.interval_secs=Number(q('mockInterval').value||15); }
    if(kind==='radar'){
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
    var body=await api('/admin/api/mock/start', {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(payload)});
    q('mockResult').classList.add('visible');
    var label=kind==='qr'?'QR Code':(kind==='radar'?'雷達':'數字');
    var extra='（模擬 '+count+' 人）';
    if(body.kind==='radar'&&body.radar){ extra+='（模擬教室 '+Number(body.radar.latitude).toFixed(6)+', '+Number(body.radar.longitude).toFixed(6)+'／判定半徑 '+body.radar.radius_m+' 公尺／班上到課率 '+body.radar.class_percent+'%）。同一個模擬點名共用一組座標：任一個測試頁送出且距離合格，這個點名下的帳號都會一起完成。'; }
    q('mockResultText').textContent='✅ 模擬點名已啟動：'+label+extra;
    if(body.kind==='qr')startMockQrPreview(body); else stopMockQr();
    toast('Mock 點名已觸發');
  }catch(e){toast(e.message||'Mock 點名失敗',false);} };
  q('stopMockQr').onclick=stopMockQr;
  function boot(){ ensureScheduleEntry(); ensureMockPanel(); var n=0,t=setInterval(function(){ ensureScheduleEntry(); ensureMockPanel(); n++; if(n>=12)clearInterval(t); },500); document.addEventListener('click',function(ev){ if(ev.target.closest&&ev.target.closest('[data-view="rollcall"]'))setTimeout(loadMockAccounts,80); },true); };
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',boot);else boot();
})();