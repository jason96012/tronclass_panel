(function(){
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
