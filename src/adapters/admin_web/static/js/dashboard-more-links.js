(function(){
  if(window.MORE_MOBILE_LINKS_V1) return;
  window.MORE_MOBILE_LINKS_V1 = true;

  function run(kind){
    if(kind==='password'){
      if(typeof window.tronOpenChangePassword==='function') window.tronOpenChangePassword();
      else if(typeof window.showPw==='function') window.showPw();
      return;
    }
    if(typeof window.tronPasskeyRegister==='function') window.tronPasskeyRegister();
  }
  function kindFrom(el){
    if(!el) return '';
    var href=(el.getAttribute&&el.getAttribute('href'))||el.hash||'';
    var id=el.id||'';
    var act=(el.getAttribute&&el.getAttribute('data-more-action'))||'';
    var tx=(el.textContent||'').replace(/\s+/g,'').trim();
    if(href.indexOf('tron-password')>=0||act==='password'||/ChangePassword|changePassword/i.test(id)||tx==='變更密碼') return 'password';
    if(href.indexOf('tron-passkey')>=0||act==='passkey'||/passkey/i.test(id)||tx==='設定通行密鑰'||tx==='設定通行密碼') return 'passkey';
    return '';
  }
  function toLink(old, kind){
    if(!old||!old.parentNode) return;
    if(old.tagName==='A' && ((old.getAttribute('href')||'').indexOf('#tron-')===0)) return;
    var a=document.createElement('a');
    a.id=old.id;
    a.className=old.className||'button secondary';
    a.href = kind==='password' ? '/admin/password' : '/admin/passkey';
    a.setAttribute('data-more-action', kind);
    a.textContent = kind==='password' ? '變更密碼' : '設定通行密鑰';
    a.style.pointerEvents='auto';
    a.style.touchAction='manipulation';
    old.parentNode.replaceChild(a, old);
  }
  function upgrade(){
    ['morePasskeyFinalV2','mPasskey'].forEach(function(id){
      var el=document.getElementById(id); if(el) toLink(el,'passkey');
    });
    ['moreChangePasswordFinalV2','mChangePassword'].forEach(function(id){
      var el=document.getElementById(id); if(el) toLink(el,'password');
    });
    var nodes=document.querySelectorAll('#mobileMoreFinalV2 button, #mobileSheet button, .more-grid button');
    for(var i=0;i<nodes.length;i++){
      var tx=(nodes[i].textContent||'').replace(/\s+/g,'').trim();
      if(tx==='設定通行密鑰'||tx==='設定通行密碼') toLink(nodes[i],'passkey');
      if(tx==='變更密碼'||tx==='修改密碼') toLink(nodes[i],'password');
    }
  }
  function onHash(){
    var h=(location.hash||'').replace(/^#/,'');
    if(h==='tron-password'||h==='change-password'||h==='password'){
      try{ history.replaceState(null,'',location.pathname+location.search+(location.hash&&location.hash.indexOf('tron-')<0?location.hash:'')); }catch(_){}
      run('password');
      return true;
    }
    if(h==='tron-passkey'||h==='passkey'){
      try{ history.replaceState(null,'',location.pathname+location.search); }catch(_){}
      run('passkey');
      return true;
    }
    return false;
  }
  document.addEventListener('click', function(ev){
    var el=ev.target;
    if(el && el.nodeType===3) el=el.parentElement;
    var a=el && el.closest && el.closest('a[href="javascript:void(0)"],a[href="javascript:void(0)"],#morePasskeyFinalV2,#moreChangePasswordFinalV2,#mPasskey,#mChangePassword');
    if(!a) return;
    var kind=kindFrom(a);
    if(!kind) return;
    ev.preventDefault();
    ev.stopPropagation();
    run(kind);
  }, false); // bubble：讓連結預設行為當備援，這裡同步開窗
  window.addEventListener('hashchange', onHash);
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', function(){ upgrade(); onHash(); });
  else { upgrade(); onHash(); }
  setInterval(upgrade, 600);
})();
