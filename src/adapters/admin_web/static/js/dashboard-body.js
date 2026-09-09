/* INLINE_SPLIT_V1：dashboard-body.js 原本內嵌於 dashboard.html 的 <body> 後段內聯 <script>（passkey 還原／友善錯誤／偵測排程導覽），抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: passkeyLoginRestoreFinalScriptV2 (原 dashboard.html:2135) ── */
(function(){
  'use strict';
  if(window.PASSKEY_LOGIN_RESTORE_FINAL_V2) return;
  window.PASSKEY_LOGIN_RESTORE_FINAL_V2 = true;

  var TOKEN_KEYS=['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
  function q(id){return document.getElementById(id)}
  function token(){try{var u=new URLSearchParams(location.search).get('token');if(u)return u.trim();for(var i=0;i<TOKEN_KEYS.length;i++){var v=localStorage.getItem(TOKEN_KEYS[i])||sessionStorage.getItem(TOKEN_KEYS[i]);if(v&&String(v).trim())return String(v).trim();}}catch(_){}return''}
  /* TOKEN_STORAGE_PURGE_V1: cookie-only（SECURITY-F03）——不再寫 token 進 storage，改清除遺留 key */
  function storeToken(t){if(!t)return;TOKEN_KEYS.forEach(function(k){try{localStorage.removeItem(k)}catch(_){}try{sessionStorage.removeItem(k)}catch(_){}});try{localStorage.removeItem('tronclass_dashboard_manual_logout')}catch(_){}try{var url=new URL(location.href);if(url.searchParams.has('token')){url.searchParams.delete('token');history.replaceState(null,'',url.pathname+url.search+url.hash)}}catch(_){}}
  function toast(text,ok){var el=q('toast');if(el){el.textContent=text;el.style.background=ok===false?'#991b1b':'#172033';el.classList.add('visible');clearTimeout(el._passkeyTimer);el._passkeyTimer=setTimeout(function(){el.classList.remove('visible')},3600)}else alert(text)}
  function b64urlEncode(buf){var bytes=new Uint8Array(buf),s='';for(var i=0;i<bytes.length;i++)s+=String.fromCharCode(bytes[i]);return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'')}
  function b64urlDecode(str){str=String(str||'').replace(/-/g,'+').replace(/_/g,'/');while(str.length%4)str+='=';var bin=atob(str),out=new Uint8Array(bin.length);for(var i=0;i<bin.length;i++)out[i]=bin.charCodeAt(i);return out.buffer}
  async function postJson(url,body){var res=await fetch(url,{method:'POST',headers:{'content-type':'application/json'},cache:'no-store',body:body==null?undefined:JSON.stringify(body)});var json=await res.json().catch(function(){return{}});if(!res.ok)throw new Error(json.error||json.message||('HTTP '+res.status));return json}
  function prepareCreateOptions(opts){opts.challenge=b64urlDecode(opts.challenge);opts.user.id=b64urlDecode(opts.user.id);if(opts.excludeCredentials)opts.excludeCredentials=opts.excludeCredentials.map(function(c){return Object.assign({},c,{id:b64urlDecode(c.id)})});return opts}
  function prepareGetOptions(opts){opts.challenge=b64urlDecode(opts.challenge);if(opts.allowCredentials)opts.allowCredentials=opts.allowCredentials.map(function(c){return Object.assign({},c,{id:b64urlDecode(c.id)})});return opts}
  function loginName(){var el=q('loginUsername')||q('loginUser')||q('username')||document.querySelector('input[autocomplete="username"],input[placeholder*="帳號"],input[placeholder*="account"]');return el?String(el.value||'').trim():''}
  function hasSecureContext(){return window.isSecureContext || location.hostname==='localhost' || location.hostname==='127.0.0.1'}
  function checkSupport(){if(!hasSecureContext()){toast('Passkey 需要 HTTPS；請用 https://dashboard.jasoncloudserver.me 開啟。',false);return false}if(!window.PublicKeyCredential||!navigator.credentials){toast('此瀏覽器不支援通行密鑰 Passkey。',false);return false}return true}

  async function passkeyRegister(){window.tronPasskeyRegister=passkeyRegister;
    if(!checkSupport())return;
    var t=token();
    try{
      toast('準備建立通行密鑰...');
      var start=await postJson('/admin/api/passkey/register/start',null);
      var cred=await navigator.credentials.create({publicKey:prepareCreateOptions(start.publicKey)});
      if(!cred)throw new Error('未建立通行密鑰');
      var body={ceremony_id:start.ceremony_id,response:{id:cred.id,attestationObject:b64urlEncode(cred.response.attestationObject),clientDataJSON:b64urlEncode(cred.response.clientDataJSON),transports:cred.response.getTransports?cred.response.getTransports():[]}};
      await postJson('/admin/api/passkey/register/finish',body);
      toast('通行密鑰已設定完成，下次可用 Passkey 登入。');
    }catch(e){toast(window.tronPasskeyFriendlyError(e, '設定通行密鑰失敗'), false)}
  }

  /* LOGIN_PAGE_STANDALONE_V1：通行密鑰「登入」流程已搬到獨立登入頁（login.html）；
     Dashboard 內只保留「設定通行密鑰」（passkeyRegister）。 */
  function ensureLoginButton(){ return; }
  function ensureRegisterButton(){
    // TOPBAR_LOGOUT_ONLY_V1: 頂欄僅保留登出，不再注入「設定通行密鑰」；
    // 通行密鑰入口改由手機更多面板（mPasskey / morePasskeyFinalV2）與設定頁進入。
    return;
  }
  function ensureMobileSheetButton(){
    // MORE_SHEET_SETTINGS_V1: 通行密鑰不再注入更多面板（改由設定頁進入）。
    return;
    if(q('mobilePasskeySheetBtn'))return;
    var host=document.querySelector('#mobileSheet .sheet-grid')||document.querySelector('.mobile-sheet .sheet-grid')||document.querySelector('.sheet-grid');
    if(!host)return;
    var b=document.createElement('button');b.id='mobilePasskeySheetBtn';b.type='button';b.textContent='設定通行密鑰';b.onclick=function(){var sheet=q('mobileSheet')||document.querySelector('.mobile-sheet');if(sheet)sheet.classList.remove('visible');passkeyRegister()};
    var logout=host.querySelector('.danger,#mLogout,#mobileLogout,#logoutBtn');
    host.insertBefore(b,logout||null);
  }
  function ensureUi(){
    if(!window.PublicKeyCredential)return;
    ensureLoginButton();
    ensureRegisterButton();
    ensureMobileSheetButton();
  }
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',ensureUi);else ensureUi();
  /* NO_POLL_ENSURE_V1：原 30 次 × 700 ms 輪詢。ensureLoginButton／ensureRegisterButton／
     ensureMobileSheetButton 目前都是 no-op（入口已移到設定頁），跑一次即可，不再排程。 */
  window.addEventListener('pageshow',ensureUi);
})();

/* ── inline block: passkeyFriendlyErrorsV1 (原 dashboard.html:2204) ── */
(function(){
  'use strict';
  if(window.PASSKEY_FRIENDLY_ERRORS_V1) return;
  window.PASSKEY_FRIENDLY_ERRORS_V1 = true;

  function rawMessage(err){
    if(!err) return '';
    var parts=[];
    try{ if(err.name) parts.push(String(err.name)); }catch(_){ }
    try{ if(err.message) parts.push(String(err.message)); }catch(_){ }
    try{ if(typeof err === 'string') parts.push(err); }catch(_){ }
    try{ parts.push(String(err)); }catch(_){ }
    return parts.join(' ').trim();
  }

  window.tronPasskeyFriendlyError = function(err, fallback){
    var raw = rawMessage(err);
    var s = raw.toLowerCase();
    var base = fallback || '通行密鑰操作失敗';

    if(!raw) return base + '。請重新操作一次。';

    if(s.indexOf('the operation either timed out or was not allowed') >= 0 ||
       s.indexOf('notallowederror') >= 0 ||
       s.indexOf('not allowed') >= 0 ||
       s.indexOf('timed out') >= 0 ||
       s.indexOf('timeout') >= 0 ||
       s.indexOf('sctn-privacy-considerations-client') >= 0){
      return '操作逾時或你取消了驗證。請重新按一次「使用通行密鑰」，並在手機或電腦跳出驗證時完成 Face ID、指紋或 PIN。';
    }
    if(s.indexOf('aborterror') >= 0 || s.indexOf('abort') >= 0 || s.indexOf('cancel') >= 0){
      return '你取消了通行密鑰驗證。若要登入，請重新按一次並完成驗證。';
    }
    if(s.indexOf('invalidstateerror') >= 0 || s.indexOf('already registered') >= 0 || s.indexOf('excluded') >= 0){
      return '這台裝置可能已經設定過通行密鑰。你可以直接用「使用通行密鑰登入」，或換一個裝置重新設定。';
    }
    if(s.indexOf('notsupportederror') >= 0 || s.indexOf('not supported') >= 0 || s.indexOf('不支援') >= 0){
      return '這個瀏覽器或裝置不支援通行密鑰。請改用帳號密碼登入，或改用支援 Passkey 的瀏覽器。';
    }
    if(s.indexOf('securityerror') >= 0 || s.indexOf('secure context') >= 0 || s.indexOf('https') >= 0 || s.indexOf('rp id') >= 0 || s.indexOf('origin') >= 0){
      return '通行密鑰需要安全的 HTTPS 網址，而且網域必須正確。請用 https://dashboard.jasoncloudserver.me 重新開啟後再試。';
    }
    if(s.indexOf('networkerror') >= 0 || s.indexOf('failed to fetch') >= 0 || s.indexOf('network') >= 0){
      return '連線失敗。請確認網站以 HTTPS 開啟後再試一次。';
    }
    if(s.indexOf('no passkey') >= 0 || s.indexOf('has no passkey') >= 0 || s.indexOf('no credentials') >= 0 || s.indexOf('no credential') >= 0){
      return '這個帳號尚未設定通行密鑰。請先用帳號密碼登入，再按「設定通行密鑰」。';
    }
    if(s.indexOf('unknown passkey credential') >= 0 || s.indexOf('unknown credential') >= 0){
      return '這個通行密鑰不屬於目前帳號。請確認輸入的是正確帳號，或重新設定通行密鑰。';
    }
    if(s.indexOf('expired') >= 0 || s.indexOf('challenge') >= 0){
      return '驗證已過期。請重新按一次通行密鑰按鈕再操作。';
    }
    if(s.indexOf('account not found') >= 0 || s.indexOf('not found or disabled') >= 0){
      return '找不到這個帳號，或帳號尚未啟用。請確認輸入的是帳號 ID 或學號。';
    }
    if(s.indexOf('multiple accounts') >= 0){
      return '有多個帳號符合這個輸入。請改輸入完整帳號 ID 再使用通行密鑰登入。';
    }
    if(s.indexOf('passkey verify failed') >= 0 || s.indexOf('authentication') >= 0 && s.indexOf('failed') >= 0){
      return '通行密鑰驗證失敗。請確認你使用的是這個帳號設定過的通行密鑰。';
    }
    if(s.indexOf('registration verify failed') >= 0 || s.indexOf('registration') >= 0 && s.indexOf('failed') >= 0){
      return '通行密鑰設定失敗。請重新按「設定通行密鑰」，並完成系統跳出的驗證。';
    }

    // 如果是 WebAuthn/Passkey 相關但沒有命中上面規則，不直接把英文長錯誤丟給一般使用者。
    if(s.indexOf('webauthn') >= 0 || s.indexOf('publickeycredential') >= 0 || s.indexOf('credential') >= 0 || s.indexOf('passkey') >= 0){
      return base + '。請重新整理頁面後再試一次；如果仍失敗，請改用帳號密碼登入。';
    }
    return raw || base;
  };

  function translateText(text){
    if(!text) return text;
    var lower = String(text).toLowerCase();
    if(lower.indexOf('the operation either timed out or was not allowed') >= 0 ||
       lower.indexOf('sctn-privacy-considerations-client') >= 0 ||
       lower.indexOf('notallowederror') >= 0){
      return window.tronPasskeyFriendlyError(text, '通行密鑰操作失敗');
    }
    if((lower.indexOf('webauthn') >= 0 || lower.indexOf('publickeycredential') >= 0) && lower.length > 20){
      return window.tronPasskeyFriendlyError(text, '通行密鑰操作失敗');
    }
    return text;
  }

  function scan(root){
    try{
      var scope = root && root.querySelectorAll ? root : document;
      var selectors = '.toast,#loginMsg,#message,.bad,.api-missing,[role="alert"],.alert,.notice';
      var nodes = [];
      if(root && root.nodeType === 1 && root.matches && root.matches(selectors)) nodes.push(root);
      Array.prototype.push.apply(nodes, Array.prototype.slice.call(scope.querySelectorAll(selectors)));
      nodes.forEach(function(el){
        var old = el.textContent || '';
        var next = translateText(old);
        if(next && next !== old){ el.textContent = next; }
      });
    }catch(_){ }
  }

  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', function(){ scan(document); });
  else scan(document);
  try{
    new MutationObserver(function(muts){
      muts.forEach(function(m){
        Array.prototype.forEach.call(m.addedNodes || [], function(n){ if(n.nodeType === 1) scan(n); });
        if(m.target) scan(m.target);
      });
    }).observe(document.documentElement, {childList:true, subtree:true, characterData:true});
  }catch(_){ }
})();

/* ── inline block: detectionScheduleDesktopNavScript (原 dashboard.html:2390) ── */
(function(){'use strict';
  if(window.DETECTION_SCHEDULE_DESKTOP_NAV_V1)return;window.DETECTION_SCHEDULE_DESKTOP_NAV_V1=true;
  function ensure(){
    var tabs=document.querySelector('.tabs');
    if(!tabs)return;
    if(!document.getElementById('detectionScheduleDesktopLink')){
      var link=document.createElement('a');
      link.id='detectionScheduleDesktopLink';
      link.className='tab detection-schedule-desktop-link';
      link.href='/admin/detection-schedule';
      link.textContent='排程';
      link.setAttribute('aria-label','前往點名偵測排程');
      link.addEventListener('click',function(ev){ev.preventDefault();ev.stopPropagation();location.assign('/admin/detection-schedule');});
      // NOTIFICATIONS_STANDALONE_PAGE_V1: 通知分頁已移除，改錨定在「維護」分頁前。
      var ref=tabs.querySelector('.tab[data-view="maintenance"]');
      if(ref)tabs.insertBefore(link,ref); else tabs.appendChild(link);
    }
    // HOMEWORK_TAB_V1: 「作業」分頁（課表旁），樣式與排程連結同一顆膠囊 class。
    if(!document.getElementById('homeworkReminderTab')){
      var hw=document.createElement('a');
      hw.id='homeworkReminderTab';
      hw.className='tab detection-schedule-desktop-link';
      hw.href='/admin/homework';
      hw.textContent='作業';
      hw.setAttribute('aria-label','前往作業提醒（作業截止追蹤）');
      hw.addEventListener('click',function(ev){ev.preventDefault();ev.stopPropagation();location.assign('/admin/homework');});
      var tt=tabs.querySelector('.tab[data-view="timetable"]');
      if(tt)tt.insertAdjacentElement('afterend',hw); else tabs.appendChild(hw);
    }
    // BULLETINS_TAB_V1: 「公告」分頁（教材旁），樣式同排程膠囊。
    if(!document.getElementById('bulletinsTab')){
      var bl=document.createElement('a');
      bl.id='bulletinsTab';
      bl.className='tab detection-schedule-desktop-link';
      bl.href='/admin/bulletins';
      bl.textContent='公告';
      bl.setAttribute('aria-label','前往課程公告');
      bl.addEventListener('click',function(ev){ev.preventDefault();ev.stopPropagation();location.assign('/admin/bulletins');});
      var mt=tabs.querySelector('.tab[data-view="materials"]');
      if(mt)mt.insertAdjacentElement('afterend',bl); else tabs.appendChild(bl);
    }
    // AI_CHAT_TAB_V1: 「AI」分頁（公告旁），樣式同排程膠囊。
    if(!document.getElementById('aiChatTab')){
      var ai=document.createElement('a');
      ai.id='aiChatTab';
      ai.className='tab detection-schedule-desktop-link';
      ai.href='/admin/ai';
      ai.textContent='AI';
      ai.setAttribute('aria-label','前往 AI 對話');
      ai.addEventListener('click',function(ev){ev.preventDefault();ev.stopPropagation();location.assign('/admin/ai');});
      var bt=document.getElementById('bulletinsTab');
      if(bt)bt.insertAdjacentElement('afterend',ai); else tabs.appendChild(ai);
    }
  }
  /* NO_POLL_ENSURE_V1：MutationObserver 已覆蓋 .tabs 被建立／重繪的時機，移除 20 次 × 500 ms 輪詢。 */
  function start(){ensure();try{new MutationObserver(ensure).observe(document.documentElement,{childList:true,subtree:true});}catch(_){}window.addEventListener('pageshow',ensure);}
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',start);else start();
})();
