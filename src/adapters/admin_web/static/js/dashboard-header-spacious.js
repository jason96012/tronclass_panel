(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <style id="DESKTOP_HEADER_SPACIOUS_V1">，瀏覽器會把有 id 的元素暴露成
     window.DESKTOP_HEADER_SPACIOUS_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.DESKTOP_HEADER_SPACIOUS_V1) return;
  window.DESKTOP_HEADER_SPACIOUS_V1 = true;
  function tidy(){
    var header = document.querySelector('header.topbar, header, .topbar');
    if(!header) return;
    var actions = header.querySelector('.top-actions');
    if(!actions){
      actions = document.createElement('div');
      actions.className = 'top-actions';
      header.appendChild(actions);
    }
    Array.prototype.slice.call(header.children).forEach(function(n){
      if(n === actions) return;
      if(n.classList && n.classList.contains('brand')) return;
      if(n.matches && n.matches('button,a.button,a.secondary')){
        actions.appendChild(n);
      }
    });
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', tidy);
  else tidy();
  setTimeout(tidy, 80);
  setTimeout(tidy, 400);
})();
