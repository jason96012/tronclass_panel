(function(){
  'use strict';
  if(window.NOTIFY_MORE_PURGE_V1) return;
  window.NOTIFY_MORE_PURGE_V1 = true;
  /* 更多面板若出現舊版「通知／通知設定」按鈕（舊編譯或半更新殘留），一律移除。 */
  function purge(){
    var panels=document.querySelectorAll('#mobileMoreFinalV2,#mobileSheet,.more-grid,.sheet-grid');
    panels.forEach(function(panel){
      panel.querySelectorAll('#moreNotifyFinalV2,[data-view="notifications"],[data-sheet-view="notifications"]').forEach(function(el){ el.remove(); });
      panel.querySelectorAll('button,a').forEach(function(el){
        var tx=(el.textContent||'').replace(/\s+/g,'').trim();
        if(tx==='通知設定'||tx==='通知') el.remove();
      });
    });
  }
  function start(){
    purge();
    var n=0, timer=setInterval(function(){ purge(); n++; if(n>=48) clearInterval(timer); }, 500);
    try{ new MutationObserver(purge).observe(document.body,{childList:true,subtree:true}); }catch(_){}
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
