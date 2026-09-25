(function(){
  if(window.MORE_ACTIONS_CLICK_V1) return;
  window.MORE_ACTIONS_CLICK_V1 = true;
  function q(id){ return document.getElementById(id); }
  function closeMore(){
    ['mobileMoreFinalV2','mobileSheet'].forEach(function(id){
      var el=q(id); if(el) el.classList.remove('visible');
    });
    document.querySelectorAll('.mobile-sheet').forEach(function(el){ el.classList.remove('visible'); });
  }
  function toast(text, ok){
    try{ if(window.TronclassDashboard&&window.TronclassDashboard.toast){ window.TronclassDashboard.toast(text, ok!==false); return; } }catch(_){}
    var el=q('toast');
    if(el){
      el.textContent=text; el.className='toast visible '+(ok===false?'bad':'ok');
      el.style.zIndex='2147483647';
      clearTimeout(el._moreAct); el._moreAct=setTimeout(function(){ el.classList.remove('visible'); }, 3200);
      return;
    }
    try{ alert(text); }catch(_){}
  }
  function isTarget(el){
    if(el&&el.nodeType===3) el=el.parentElement; if(el&&el.nodeType===3) el=el.parentElement; if(!el||!el.closest) return null;
    var b=el.closest('#morePasskeyFinalV2,#moreChangePasswordFinalV2,#mPasskey,#mChangePassword,#mobilePasskeySheetBtn,[data-more-action]');
    if(b) return b;
    b=el.closest('button,a');
    if(!b) return null;
    var tx=(b.textContent||'').replace(/\s+/g,'').trim();
    if(tx==='設定通行密鑰'||tx==='變更密碼') return b;
    return null;
  }
  function ensureModal(){
    var wrap=q('changePasswordModal');
    if(wrap) return wrap;
    wrap=document.createElement('div');
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
    var m=ensureModal();
    ['cpCurrent','cpNew','cpConfirm'].forEach(function(id){ if(q(id)) q(id).value=''; });
    var msg=q('cpMsg'); if(msg){ msg.textContent=''; msg.className=''; }
    m.classList.add('visible');
    m.style.display='flex';
    closeMore();
    setTimeout(function(){ if(q('cpCurrent')) q('cpCurrent').focus(); }, 80);
  }
  function hideModal(){
    var m=q('changePasswordModal');
    if(m){ m.classList.remove('visible'); m.style.display='none'; }
  }
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
      setTimeout(hideModal, 500);
    }catch(e){
      var m=String((e&&e.message)||e||'');
      if(/incorrect|current Dashboard password/i.test(m)) m='目前密碼不正確。';
      set(m||'變更密碼失敗', false);
    }
  }
  function doPasskey(){
    closeMore();
    if(typeof window.tronPasskeyRegister==='function'){ window.tronPasskeyRegister(); return; }
    var btn=q('passkeyRegisterBtn');
    if(btn && btn.onclick){ btn.onclick(); return; }
    toast('通行密鑰功能尚未就緒，請重整後再試。', false);
  }
  window.tronOpenChangePassword=showModal;
  document.addEventListener('click', function(ev){
    var b=isTarget(ev.target);
    if(!b) return;
    /* PASSKEY_LOGIN_BUTTON_FIX_V1：登入卡內按鈕（通行密鑰「登入」）不攔截 */
    if(b.closest && b.closest('#animatedLoginForm,#loginCard')) return;
    if(/passkeylogin/i.test(b.id||'')) return;
    var act=b.getAttribute('data-more-action')||'';
    var id=b.id||'';
    var tx=(b.textContent||'').replace(/\s+/g,'').trim();
    var passkey = act==='passkey' || /Passkey|passkey/i.test(id) || tx==='設定通行密鑰';
    var password = act==='password' || /Password|password/i.test(id) || tx==='變更密碼';
    if(!passkey && !password) return;
    ev.preventDefault();
    if(ev.stopImmediatePropagation) ev.stopImmediatePropagation();
    ev.stopPropagation();
    if(passkey) doPasskey(); else showModal();
  }, true);
})();
