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
  /* NO_POLL_ENSURE_V1：原本是「無限」500 ms 輪詢（頁面活著就一直跑 querySelectorAll）。
     這些按鈕只會在「更多」面板／登入卡被建立或重繪時出現，改用 MutationObserver 監看
     body 子樹的 childList；另 pageshow 補跑。 */
  var scheduled = false;
  function scheduleFix(){
    if(scheduled) return;
    scheduled = true;
    (window.requestAnimationFrame || window.setTimeout)(function(){ scheduled = false; fix(); });
  }
  try{ new MutationObserver(scheduleFix).observe(document.body || document.documentElement, {childList:true, subtree:true}); }catch(_){}
  window.addEventListener('pageshow', fix);
})();
