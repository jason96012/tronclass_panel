/* ONBOARDING_GUIDE_V1：第一次進入 Dashboard 的上手導覽。
 *
 * - 只在登入完成、狀態載入後顯示；每個瀏覽器＋帳號只自動顯示一次
 *   （localStorage key：tronclass_onboarding_v1_done:<account_id>）。
 * - 之後可從頂欄「?」重新開啟，或加 #guide 到網址。
 * - 內容依角色（管理員／一般）與實際狀態（監控中、Discord 綁定、課表、推播）
 *   顯示「已完成／尚未」，每一步都有「前往」按鈕直接跳到對應分頁或頁面。
 * - 純前端、CSP 相容（外掛 JS + 外部 CSS；innerHTML 會經過 DOMPurify 政策）。
 */
(function(){
  'use strict';
  if(window.ONBOARDING_GUIDE_V1) return;
  window.ONBOARDING_GUIDE_V1 = true;

  var KEY_PREFIX = 'tronclass_onboarding_v1_done:';
  var LAYER_ID = 'tronOnboarding';
  var step = 0;
  var facts = null;
  var layer = null;

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v==null?'':v).replace(/[&<>"']/g,function(ch){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]; }); }
  function dash(){ return window.TronclassDashboard || {}; }
  function currentStatus(){
    try{ if(dash().getStatus) { var s=dash().getStatus(); if(s) return s; } }catch(_){}
    /* AUTH_STATE_FROM_SERVER_V1：伺服器注入的摘要只有 is_admin / current_account_id（__partial），
       新手導覽需要完整 status（accounts / passkey_count…），要等主腳本 loadStatus() 補齊。 */
    if(window.__tronAuthStatus && typeof window.__tronAuthStatus==='object' && 'is_admin' in window.__tronAuthStatus && !window.__tronAuthStatus.__partial) return window.__tronAuthStatus;
    return null;
  }
  function loggedIn(){
    return document.body.classList.contains('logged-in') || document.documentElement.classList.contains('dash-auth-ok');
  }
  function doneKey(acct){ return KEY_PREFIX + String(acct||''); }
  function isDone(acct){ try{ return localStorage.getItem(doneKey(acct))==='1'; }catch(_){ return true; } }
  function markDone(acct){ try{ localStorage.setItem(doneKey(acct),'1'); }catch(_){} }
  function wantsGuideFromUrl(){
    try{
      if(location.hash==='#guide') return true;
      return new URLSearchParams(location.search).get('guide')==='1';
    }catch(_){ return false; }
  }

  async function jsonGet(path){
    var res = await fetch(path,{credentials:'same-origin',cache:'no-store'});
    var body = await res.json().catch(function(){ return {}; });
    if(!res.ok){ var e=new Error(body.error||('HTTP '+res.status)); e.status=res.status; throw e; }
    return body;
  }

  /* 收集導覽要用的事實；每一項都容錯，缺資料時顯示「無法判斷」而不是壞掉。 */
  async function collectFacts(status){
    var accounts = Array.isArray(status.accounts) ? status.accounts : [];
    var me = status.current_account_id || '';
    var mine = accounts.find(function(a){ return a.account_id===me; }) || null;
    var f = {
      isAdmin: !!status.is_admin,
      accountId: me,
      accountCount: accounts.length,
      runningCount: accounts.filter(function(a){ return a.is_running; }).length,
      inactiveCount: Array.isArray(status.inactive_accounts) ? status.inactive_accounts.length : 0,
      mineRunning: mine ? !!mine.is_running : null,
      hasDiscord: mine ? !!(mine.discord_user_id && String(mine.discord_user_id).trim()) : null,
      hasLine: mine ? !!(mine.line_user_id && String(mine.line_user_id).trim()) : null,
      hasTimetable: null,
      hasPush: null,
      /* ONBOARDING_PUSH_STATE_V1：後端 status.push_subscription_count（伺服器端登記的裝置數） */
      pushServerCount: (typeof status.push_subscription_count==='number') ? status.push_subscription_count : null,
      pushOtherDevice: false,
      /* ONBOARDING_PASSKEY_STATE_V1：後端 status.passkey_count（null＝無法判斷） */
      passkeyCount: (typeof status.passkey_count==='number') ? status.passkey_count : null,
      hasPasskey: (typeof status.passkey_count==='number') ? status.passkey_count>0 : null,
      pushSupported: !!(('serviceWorker' in navigator) && ('PushManager' in window))
    };
    try{
      var acct = me && me!=='<token-admin>' ? me : (accounts[0] && accounts[0].account_id) || '';
      if(acct){
        var tt = await jsonGet('/admin/api/timetable?account_id='+encodeURIComponent(acct));
        f.hasTimetable = Array.isArray(tt.entries) && tt.entries.length>0;
      }
    }catch(_){}
    try{
      if(f.pushSupported){
        var reg = await navigator.serviceWorker.getRegistration('/');
        if(reg){ var sub = await reg.pushManager.getSubscription(); f.hasPush = !!sub; }
        else f.hasPush = false;
      }
    }catch(_){}
    /* ONBOARDING_PUSH_STATE_V1：本機沒訂閱，但伺服器有登記別台裝置 → 視為已啟用（其他裝置）。
       本機有訂閱但伺服器數量為 0（例如訂閱已被清理）→ 視為未啟用，提示重新啟用。 */
    if(f.pushServerCount!==null){
      if(f.pushServerCount>0 && f.hasPush!==true){ f.hasPush = true; f.pushOtherDevice = true; }
      else if(f.pushServerCount===0 && f.hasPush===true){ f.hasPush = false; }
      else if(f.hasPush===null){ f.hasPush = f.pushServerCount>0; f.pushOtherDevice = f.pushServerCount>0; }
    }
    return f;
  }

  function pill(state, yes, no, unknown){
    if(state===true) return '<span class="ob-pill good">'+esc(yes||'已完成')+'</span>';
    if(state===false) return '<span class="ob-pill todo">'+esc(no||'尚未')+'</span>';
    return '<span class="ob-pill">'+esc(unknown||'無法判斷')+'</span>';
  }
  /* action = { label, href } 或 { label, view }；只在「尚未／無法判斷」時顯示按鈕，
     已完成的項目不放按鈕以免干擾。按鈕透過 data-ob-href / data-ob-view 綁定，
     符合 CSP（無 inline handler）。 */
  function item(state, title, desc, yes, no, unknown, action){
    var btn = '';
    if(action && state!==true){
      var attr = action.href ? ' data-ob-href="'+esc(action.href)+'"' : ' data-ob-view="'+esc(action.view)+'"';
      btn = '<div class="ob-item-act"><button type="button" class="ob-btn small"'+attr+'>'+esc(action.label)+'</button></div>';
    }
    return '<li class="ob-item"><div class="ob-item-head"><span>'+title+'</span>'+pill(state,yes,no,unknown)+'</div>'+(desc?'<div class="ob-item-desc">'+desc+'</div>':'')+btn+'</li>';
  }

  function steps(f){
    var role = f.isAdmin ? '管理員' : '一般使用者';
    var list = [];

    list.push({
      title: '歡迎使用 Tronclass Dashboard',
      kicker: '第 1 步・這是什麼',
      body:
        '<p>這個網站會替你的 Tronclass 帳號<b>自動偵測並完成點名</b>（雷達、QR Code、數字碼），並追蹤<b>作業截止</b>、推送<b>通知</b>與<b>學校公告</b>。</p>'+
        '<p>你目前以 <b>'+esc(f.accountId||'-')+'</b> 登入，身分是 <b>'+role+'</b>。'+
        (f.isAdmin ? '管理員可以看到所有帳號、新增帳號與維護頁。' : '一般使用者只會看到自己的帳號與設定。')+'</p>'+
        '<p class="ob-note">Dashboard 密碼與 Tronclass 密碼是分開的；剛才設定的是 Dashboard 專用密碼，之後登入這個網站請用它。</p>',
      action: null
    });

    var acctItems = '';
    if(f.isAdmin){
      acctItems += item(f.accountCount>0, '已有帳號在系統裡', '目前 '+f.accountCount+' 個帳號，'+f.runningCount+' 個監控中。想加同學／其他帳號：帳號分頁 →「＋ 新增帳號」，新增後會立刻登入並開始監控，不用重啟。', '有 '+f.accountCount+' 個', '還沒有帳號', '', { label:'去新增帳號', view:'accounts' });
      acctItems += item(f.inactiveCount===0, '所有帳號都在監控', f.inactiveCount>0 ? '有 '+f.inactiveCount+' 個帳號在資料庫但沒在跑（多半是登入失敗），到帳號分頁底部「未在監控的帳號」修正後按「啟動監控」。' : '沒有卡住的帳號。', '正常', '有 '+f.inactiveCount+' 個未監控', '', { label:'去啟動監控', view:'accounts' });
    }else{
      acctItems += item(f.mineRunning, '我的帳號正在監控', f.mineRunning===false ? '目前是暫停狀態，到帳號分頁按「恢復」即可；若一直失敗請找管理員重新登入。' : '監控中代表系統會定時檢查 Tronclass 有沒有新的點名。', '監控中', '已暫停', '無法判斷', { label:'去恢復監控', view:'accounts' });
    }
    list.push({
      title: f.isAdmin ? '帳號與監控' : '確認你的帳號狀態',
      kicker: '第 2 步・帳號',
      body:
        '<p>「總覽」看整體狀態，「帳號」分頁可以對每個帳號按 <b>檢查</b>（立刻查一次）、<b>重登</b>（Tronclass 登入失效時）、<b>暫停／恢復</b>。</p>'+
        '<ul class="ob-list">'+acctItems+'</ul>',
      action: { label: '打開帳號分頁', view: 'accounts' }
    });

    list.push({
      title: '點名是怎麼進行的',
      kicker: '第 3 步・點名',
      body:
        '<ul class="ob-list">'+
        '<li class="ob-item"><div class="ob-item-head"><span>雷達點名</span><span class="ob-pill good">全自動</span></div><div class="ob-item-desc">偵測到後用設定好的座標自動送出，你什麼都不用做。</div></li>'+
        '<li class="ob-item"><div class="ob-item-head"><span>QR Code 點名</span><span class="ob-pill todo">需要你掃描</span></div><div class="ob-item-desc">收到通知後點開掃描頁，用手機相機對準教室螢幕的 QR Code；掃一次會套用到同一堂課的所有等待帳號。頁面上有<b>剩餘時間倒數</b>，逾時就會停止接收。</div></li>'+
        '<li class="ob-item"><div class="ob-item-head"><span>數字碼點名</span><span class="ob-pill todo">需要你輸入</span></div><div class="ob-item-desc">打開數字碼頁，輸入老師公布的 4 位數字（或按「自動讀取」試試看），同樣有倒數計時。</div></li>'+
        '</ul>'+
        '<p>「點名」分頁會列出目前偵測到的點名與歷史結果（成功／未成功）。</p>',
      action: { label: '打開點名分頁', view: 'rollcall' }
    });

    var notifyItems = '';
    notifyItems += item(f.hasDiscord, 'Discord 綁定', f.hasDiscord===false ? '到「通知設定 → 通知管道綁定」貼上你的 Discord User ID 即可（頁面有取得方式）；也可以在 Discord 用 /bind-account。綁定後點名、QR、結果都會 DM 給你。' : 'Discord 是最主要的即時通知管道。', '已綁定', '未綁定', '無法判斷', { label:'去綁定 Discord', href:'/admin/notifications#binding' });
    notifyItems += item(f.hasLine, 'LINE 綁定', f.hasLine===true ? 'LINE 也會收到點名通知。' : '選用。若有啟用 LINE Bot，把 Bot 加好友後取得 User ID，貼到「通知管道綁定」。', '已綁定', '未綁定（選用）', '無法判斷', { label:'去綁定 LINE', href:'/admin/notifications#binding' });
    notifyItems += item(f.hasPush, '手機／瀏覽器推播', f.pushSupported ? '到「推播狀態」頁按「啟用背景推播」，iPhone 需先把網站加到主畫面再開。啟用後晨間摘要、作業提醒都會推到這台裝置。' : '這個瀏覽器不支援 Web Push；請改用 Discord 通知。', f.pushOtherDevice ? ('已啟用（其他裝置 '+f.pushServerCount+' 台）') : '已啟用', '尚未啟用', f.pushSupported?'無法判斷':'不支援', f.pushSupported ? { label:'去啟用推播', href:'/admin/push-status' } : null);
    list.push({
      title: '把通知接起來',
      kicker: '第 4 步・通知',
      body:
        '<p>沒有通知的話，QR／數字碼點名你會來不及處理。建議至少接一個管道：</p>'+
        '<ul class="ob-list">'+notifyItems+'</ul>'+
        '<p class="ob-note">「通知」頁還可以個別開關各類 Discord 通知、上課前提醒、晨間摘要與作業截止提醒。</p>',
      action: { label: '前往通知設定', href: '/admin/notifications' }
    });

    list.push({
      title: '填好課表，偵測更聰明',
      kicker: '第 5 步・課表',
      body:
        '<ul class="ob-list">'+
        item(f.hasTimetable, '課表已填寫', f.hasTimetable===false ? '到「課表」分頁按「編輯課表」手動輸入，或用同步功能匯入。' : '有課表後系統才知道你什麼時候在上課。', '已有課表', '還是空的', '無法判斷', { label:'去填課表', view:'timetable' })+
        '</ul>'+
        '<p>課表會用在：<b>上課前 Discord 提醒</b>、<b>點名偵測排程</b>（上課時段高頻檢查、其他時段省流量）、以及總覽上的「下一堂課」卡片。排程細節在「更多 → 點名排程」。</p>',
      action: { label: '打開課表分頁', view: 'timetable' }
    });

    list.push({
      title: '最後：安全與外觀',
      kicker: '第 6 步・完成',
      body:
        '<ul class="ob-list">'+
        item(true, 'Dashboard 密碼已設定', '之後要改密碼：更多 → 設定。', '完成')+
        item(f.hasPasskey, '通行密鑰（Passkey）',
          f.hasPasskey ? '這個帳號已可用 Face ID／指紋一鍵登入。要在其他裝置也用，可到「通行密鑰」頁再新增。' : '建議設定：之後用 Face ID／指紋一鍵登入，不必輸入密碼。',
          '已設定'+(f.passkeyCount?(' '+f.passkeyCount+' 組'):''), '尚未設定', '無法判斷', { label:'去設定通行密鑰', href:'/admin/passkey' })+
        '</ul>'+
        '<p>「設定」頁可以調整主題、強調色與字體大小。這份導覽之後隨時可以從頂欄「?」再打開，或在網址後加 #guide。</p>',
      action: { label: '前往設定頁', href: '/admin/settings' }
    });
    return list;
  }

  function render(){
    if(!layer || !facts) return;
    var all = steps(facts);
    if(step<0) step=0; if(step>=all.length) step=all.length-1;
    var s = all[step];
    /* ONBOARDING_STEP_NUMBERS_V1：圓圈內顯示步驟數字（已完成的步驟打勾），並附「第 N / M 步」文字 */
    var dots = all.map(function(_,i){
      var cls = 'ob-dot'+(i===step?' active':(i<step?' done':''));
      var label = i<step ? '✓' : String(i+1);
      return '<button type="button" class="'+cls+'" data-ob-goto="'+i+'" aria-label="第 '+(i+1)+' 步" aria-current="'+(i===step?'step':'false')+'">'+label+'</button>';
    }).join('') + '<span class="ob-dot-label">第 '+(step+1)+' / '+all.length+' 步</span>';
    var last = step===all.length-1;
    layer.innerHTML =
      '<div class="ob-card" role="dialog" aria-modal="true" aria-labelledby="obTitle">'+
        '<div class="ob-head"><div><div class="ob-kicker">'+esc(s.kicker)+'</div><h3 id="obTitle" class="ob-title">'+esc(s.title)+'</h3></div><button type="button" class="ob-close" id="obClose" aria-label="關閉">×</button></div>'+
        '<div class="ob-body">'+s.body+'</div>'+
        '<div class="ob-foot">'+
          '<div class="ob-dots">'+dots+'</div>'+
          '<div class="ob-actions">'+
            (step>0 ? '<button type="button" class="ob-btn secondary" id="obPrev">上一步</button>' : '')+
            (s.action ? '<button type="button" class="ob-btn secondary" id="obGo">'+esc(s.action.label)+'</button>' : '')+
            '<button type="button" class="ob-btn primary" id="obNext">'+(last?'完成':'下一步')+'</button>'+
          '</div>'+
        '</div>'+
      '</div>';
    var prev=q('obPrev'), next=q('obNext'), go=q('obGo'), close=q('obClose');
    if(prev) prev.onclick=function(){ step--; render(); };
    if(next) next.onclick=function(){ if(last) closeGuide(); else { step++; render(); } };
    if(close) close.onclick=closeGuide;
    if(go) go.onclick=function(){ closeGuide(); jump(s.action); };
    layer.querySelectorAll('[data-ob-goto]').forEach(function(b){ b.onclick=function(){ step=Number(b.getAttribute('data-ob-goto'))||0; render(); }; });
    layer.querySelectorAll('[data-ob-href],[data-ob-view]').forEach(function(b){
      b.onclick=function(){
        var href=b.getAttribute('data-ob-href'), view=b.getAttribute('data-ob-view');
        closeGuide();
        jump(href ? { href:href } : { view:view });
      };
    });
    var body=layer.querySelector('.ob-body'); if(body) body.scrollTop=0;
  }

  function jump(action){
    if(!action) return;
    if(action.href){ location.href=action.href; return; }
    if(action.view){
      var tab=document.querySelector('.tab[data-view="'+action.view+'"]');
      if(tab){ tab.click(); return; }
      if(dash().switchView){ dash().switchView(action.view); return; }
      location.hash='#'+action.view;
    }
  }

  function closeGuide(){
    if(facts) markDone(facts.accountId);
    if(layer){ layer.remove(); layer=null; }
    document.removeEventListener('keydown', onKey);
    try{ if(location.hash==='#guide') history.replaceState(null,'',location.pathname+location.search+'#overview'); }catch(_){}
  }
  function onKey(ev){ if(ev.key==='Escape') closeGuide(); }

  async function openGuide(){
    var status = currentStatus();
    if(!status){ if(dash().toast) dash().toast('狀態尚未載入，請稍後再試', false); return; }
    if(layer) layer.remove();
    layer = document.createElement('div');
    layer.id = LAYER_ID;
    layer.className = 'ob-layer';
    layer.innerHTML = '<div class="ob-card"><div class="ob-body"><p class="ob-note">正在整理你的帳號狀態…</p></div></div>';
    (document.documentElement||document.body).appendChild(layer);
    layer.addEventListener('click', function(ev){ if(ev.target===layer) closeGuide(); });
    document.addEventListener('keydown', onKey);
    step = 0;
    facts = await collectFacts(status);
    if(!layer) return; // 使用者在載入期間已關閉
    render();
  }
  window.tronOpenOnboarding = openGuide;

  /* 頂欄「?」與更多面板的「新手導覽」入口 */
  function ensureEntries(){
    var actions = document.querySelector('header .top-actions');
    if(actions && !q('onboardingHelpBtn')){
      var b=document.createElement('button');
      b.id='onboardingHelpBtn'; b.className='icon'; b.type='button'; b.title='新手導覽'; b.setAttribute('aria-label','新手導覽'); b.textContent='?';
      b.onclick=function(){ openGuide(); };
      var refresh=q('refreshAll');
      if(refresh && refresh.parentNode===actions) actions.insertBefore(b, refresh); else actions.appendChild(b);
    }
    /* ONBOARDING_NO_MORE_ENTRY_V1：手機「更多」面板不再放「新手導覽」按鈕；
       仍可從頂欄「?」或網址加 #guide 重新開啟。 */
  }

  /* 自動顯示：登入完成＋狀態就緒＋這個帳號還沒看過（或網址帶 #guide）。 */
  var tries=0, autoTimer=null, forceFromUrl=false;
  /* dashboard 主腳本開機時會用 hash 當分頁名（switchView('guide') 會把所有分頁藏起來），
     所以先記下旗標、把 hash 換回 #overview 並切回總覽，再由 autoTick 開導覽。 */
  function normalizeGuideHash(){
    if(!wantsGuideFromUrl()) return;
    forceFromUrl=true;
    try{ history.replaceState(null,'',location.pathname+location.search+'#overview'); }catch(_){}
    var tab=document.querySelector('.tab[data-view="overview"]');
    if(tab) tab.click(); else if(dash().switchView) dash().switchView('overview');
  }
  function autoTick(){
    tries++;
    ensureEntries();
    if(!loggedIn()){ if(tries>240) clearInterval(autoTimer); return; }
    var status=currentStatus();
    if(!status || !status.current_account_id){ if(tries>240) clearInterval(autoTimer); return; }
    if(status.must_change_password){ clearInterval(autoTimer); return; }
    clearInterval(autoTimer);
    if(forceFromUrl || !isDone(status.current_account_id)){
      forceFromUrl=false;
      setTimeout(openGuide, 600);
    }
  }
  function start(){
    normalizeGuideHash();
    autoTimer=setInterval(autoTick, 500);
    autoTick();
    try{ new MutationObserver(ensureEntries).observe(document.body,{childList:true,subtree:true}); }catch(_){}
    window.addEventListener('hashchange', function(){ if(location.hash==='#guide'){ normalizeGuideHash(); forceFromUrl=false; openGuide(); } });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
