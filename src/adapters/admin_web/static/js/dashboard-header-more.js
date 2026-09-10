(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <style id="MOBILE_HEADER_TO_MORE_V1">，瀏覽器會把有 id 的元素暴露成
     window.MOBILE_HEADER_TO_MORE_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.MOBILE_HEADER_TO_MORE_V1) return;
  window.MOBILE_HEADER_TO_MORE_V1 = true;
  function q(id){ return document.getElementById(id); }
  function closeMore(){
    var el=q('mobileMoreFinalV2'); if(el) el.classList.remove('visible');
  }
  function toast(text, ok){
    if(window.TronclassDashboard && window.TronclassDashboard.toast) return window.TronclassDashboard.toast(text, ok!==false);
    var el=q('toast'); if(!el){ alert(text); return; }
    el.textContent=text; el.className='toast visible '+(ok===false?'bad':'ok');
    clearTimeout(el._cpTimer); el._cpTimer=setTimeout(function(){ el.classList.remove('visible'); }, 2800);
  }
  function goView(view){
    closeMore();
    if(view==='materials' || view==='files'){
      location.href='/admin/files';
      return;
    }
    var tab=document.querySelector('.tab[data-view="'+view+'"]');
    if(tab){ tab.click(); return; }
    if(window.TronclassDashboard && window.TronclassDashboard.switchView){
      window.TronclassDashboard.switchView(view);
      return;
    }
    location.hash='#'+view;
  }
  function ensureModal(){
    if(q('changePasswordModal')) return q('changePasswordModal');
    var wrap=document.createElement('div');
    wrap.id='changePasswordModal';
    wrap.className='cp-modal';
    wrap.innerHTML='<div class="cp-card"><h3>變更密碼</h3>'+
      '<label for="cpCurrent">目前 Dashboard 密碼</label><input id="cpCurrent" type="password" autocomplete="current-password">'+
      '<label for="cpNew">新密碼（至少 10 字）</label><input id="cpNew" type="password" autocomplete="new-password">'+
      '<label for="cpConfirm">再輸入一次新密碼</label><input id="cpConfirm" type="password" autocomplete="new-password">'+
      '<div id="cpMsg"></div>'+
      '<div class="cp-actions"><button id="cpCancel" class="secondary" type="button">取消</button>'+
      '<button id="cpSave" type="button">儲存</button></div></div>';
    document.body.appendChild(wrap);
    wrap.addEventListener('click', function(ev){ if(ev.target===wrap) hideModal(); });
    q('cpCancel').onclick=hideModal;
    q('cpSave').onclick=submitPassword;
    return wrap;
  }
  function showModal(){
    closeMore();
    var m=ensureModal();
    ['cpCurrent','cpNew','cpConfirm'].forEach(function(id){ if(q(id)) q(id).value=''; });
    var msg=q('cpMsg'); if(msg){ msg.textContent=''; msg.className=''; }
    m.classList.add('visible');
    setTimeout(function(){ if(q('cpCurrent')) q('cpCurrent').focus(); }, 50);
  }
  function hideModal(){ var m=q('changePasswordModal'); if(m) m.classList.remove('visible'); }
  async function submitPassword(){
    var cur=(q('cpCurrent')&&q('cpCurrent').value)||'';
    var n=(q('cpNew')&&q('cpNew').value)||'';
    var c=(q('cpConfirm')&&q('cpConfirm').value)||'';
    var msg=q('cpMsg');
    function set(tx,ok){ if(msg){ msg.textContent=tx||''; msg.className=ok===false?'bad':ok===true?'ok':''; } }
    if(!cur){ set('請輸入目前密碼。', false); return; }
    if(!n||!c){ set('請輸入並確認新密碼。', false); return; }
    if(n!==c){ set('兩次輸入的新密碼不一致。', false); return; }
    if(n.length<10){ set('新密碼至少 10 個字元。', false); return; }
    try{
      var res=await fetch('/admin/api/password',{
        method:'POST', credentials:'same-origin', cache:'no-store',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({current_password:cur, new_password:n})
      });
      var body=await res.json().catch(function(){return {};});
      if(!res.ok) throw new Error(body.error||body.message||('HTTP '+res.status));
      set('密碼已更新。', true);
      toast('密碼已更新');
      setTimeout(hideModal, 600);
    }catch(e){
      var m=String((e&&e.message)||e||'');
      if(m.indexOf('current Dashboard password')>=0 || m.indexOf('incorrect')>=0) m='目前密碼不正確。';
      set(m||'變更密碼失敗', false);
    }
  }
  function addBtn(grid, id, label, cls, onClick){
    if(q(id)) return q(id);
    var isPk=/passkey|Passkey/i.test(id)||label.indexOf('通行')>=0;
    var isPw=/password|Password/i.test(id)||label.indexOf('密碼')>=0;
    var b=document.createElement((isPk||isPw)?'a':'button');
    b.id=id; b.className=cls||'secondary';
    if(b.tagName==='A'){ b.href=isPk?'/admin/passkey':'/admin/password'; }
    else { b.type='button'; }
    b.textContent=label; b.onclick=onClick;
    var logout=grid.querySelector('.danger,#moreLogoutFinalV2');
    grid.insertBefore(b, logout||null);
    return b;
  }
  function enhanceMore(){
    var grid=document.querySelector('#mobileMoreFinalV2 .more-grid');
    if(!grid) return;
    // NOTIFICATIONS_STANDALONE_PAGE_V2: 通知按鈕已自更多面板移除，通知設定請由設定頁或 /admin/notifications 前往。
    addBtn(grid,'moreMaintFinalV2','維護','secondary', function(){ goView('maintenance'); });
    // MORE_SHEET_SETTINGS_V1: 更多面板第一格固定為「設定」；通行密鑰/變更密碼/推播狀態已移除（改由設定頁進入）。
    (function(){
      if(document.getElementById('moreSettingsFinalV2')) return;
      var a=document.createElement('a');
      a.id='moreSettingsFinalV2'; a.className='button secondary'; a.href='/admin/settings';
      a.textContent='設定';
      grid.insertBefore(a, grid.firstChild);
    })();
    var blacklist=addBtn(grid,'moreBlacklistFinalV2','黑名單','secondary', function(){ location.href='/admin/blacklist'; });
    if(blacklist){ blacklist.setAttribute('data-admin-only','1'); blacklist.onclick=function(){ location.href='/admin/blacklist'; }; }
    var drive=q('moreDriveFinalV2');
    if(drive && drive.textContent!=='教材／雲端硬碟') drive.textContent='教材／雲端硬碟'; /* INLINE_SPLIT_DEAD_BLOCK_V1：冪等寫入，避免 MutationObserver 自我觸發 */
  }
  window.tronOpenChangePassword=showModal;
  function start(){
    enhanceMore();
    /* NO_POLL_ENSURE_V1：移除 24 次 × 500 ms 輪詢；MutationObserver（整棵樹 childList）已能在
       「更多」面板建立時立刻補上按鈕，另在 pageshow 補跑一次。 */
    try{ new MutationObserver(enhanceMore).observe(document.documentElement,{childList:true,subtree:true}); }catch(_){}
    window.addEventListener('pageshow', enhanceMore);
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
