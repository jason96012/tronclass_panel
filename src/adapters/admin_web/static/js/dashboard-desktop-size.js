(function(){
  if(window.DESKTOP_SIZE_ALWAYS_JS_V1) return;
  window.DESKTOP_SIZE_ALWAYS_JS_V1 = true;
  function unlock(){
    var h=document.documentElement;
    if(!(h.classList.contains('tron-auth-ok')||h.classList.contains('dash-auth-ok')||(document.body&&document.body.classList.contains('logged-in')))) return;
    if(document.body){
      document.body.classList.remove('animated-login-active','login-screen-active');
      document.body.classList.add('logged-in','auth-ok','dash-auth-ok');
    }
    h.classList.remove('tron-auth-logged-out','login-screen-active');
  }
  unlock();
  document.addEventListener('tronclass:auth-state', unlock);
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', unlock);
  setTimeout(unlock, 0);
  setTimeout(unlock, 50);
  setTimeout(unlock, 200);
})();
