(function(){
  if(window.MORE_REAL_PAGES_V1) return;
  window.MORE_REAL_PAGES_V1 = true;
  function toPage(el, href, label){
    if(!el||!el.parentNode) return;
    if(el.tagName==='A' && (el.getAttribute('href')||'').indexOf(href)===0) return;
    var a=document.createElement('a');
    a.id=el.id; a.className=el.className||'button secondary';
    a.href=href; a.textContent=label;
    el.parentNode.replaceChild(a, el);
  }
  function fix(){
    [['moreChangePasswordFinalV2','/admin/password','變更密碼'],
     ['mChangePassword','/admin/password','變更密碼'],
     ['morePasskeyFinalV2','/admin/passkey','設定通行密鑰'],
     ['mPasskey','/admin/passkey','設定通行密鑰']].forEach(function(it){
      var el=document.getElementById(it[0]); if(el) toPage(el,it[1],it[2]);
    });
    document.querySelectorAll('#mobileMoreFinalV2 a,#mobileMoreFinalV2 button,.more-grid a,.more-grid button').forEach(function(el){
      var tx=(el.textContent||'').replace(/\s+/g,'').trim();
      if(tx==='變更密碼'||tx==='修改密碼') toPage(el,'/admin/password','變更密碼');
      if(tx==='設定通行密鑰'||tx==='設定通行密碼') toPage(el,'/admin/passkey','設定通行密鑰');
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', fix);
  else fix();
  setInterval(fix, 500);
})();
