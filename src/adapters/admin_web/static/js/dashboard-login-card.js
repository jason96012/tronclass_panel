(function(){
  'use strict';
  if(window.ANIMATED_LOGIN_STATIC_V1) return;
  window.ANIMATED_LOGIN_STATIC_V1 = true;

  var TOKEN_KEYS=['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
  var SAVED_USER='tronclass_admin_saved_username_v1';
  var busy=false;
  var raf=0;
  var particles=[];

  function q(id){return document.getElementById(id)}
  function loginRoot(){return q('loginCard')||q('loginPanel')||document.querySelector('.login-card')}
  function isLoggedIn(){if(document.documentElement.classList.contains('tron-auth-pending')||document.documentElement.classList.contains('tron-auth-ok')||window.__tronAuthState==='ok'||window.__tronAuthState==='pending')return true;return document.body.classList.contains('logged-in') || document.documentElement.classList.contains('dash-auth-ok') || (loginRoot() && getComputedStyle(loginRoot()).display==='none')}
  /* LOGIN_PAGE_STANDALONE_V1：本檔只在獨立登入頁（login.html）載入。登入卡 markup 已由伺服器輸出，
     不需要動態建立；ensureShell 過去是未定義的（每次 tick 都丟 ReferenceError），改為 no-op。 */
  function ensureShell(){}
/* TOKEN_STORAGE_PURGE_V1: cookie-only（SECURITY-F03）——不再寫 token 進 storage，改清除遺留 key */
  function purgeLegacyTokens(){TOKEN_KEYS.forEach(function(k){try{localStorage.removeItem(k)}catch(_){}try{sessionStorage.removeItem(k)}catch(_){}});try{var u=new URL(location.href);if(u.searchParams.has('token')){u.searchParams.delete('token');history.replaceState(null,'',u.pathname+u.search+u.hash)}}catch(_){}}
  function setMsg(m,ok){var el=q('loginMsg')||q('animatedLoginMsg');if(el){el.textContent=m||'';el.className='animated-login-message '+(ok===false?'bad':ok===true?'ok':'')}}
  async function parseResponse(res){var b=await res.json().catch(function(){return{}});if(!res.ok){var e=new Error(b.error||b.message||('HTTP '+res.status));e.status=res.status;throw e}return b}
  function friendly(err){var m=(err&&err.message)||String(err||'');if(m==='unauthorized')return'帳號或密碼錯誤，或此帳號未啟用。';if(m.indexOf('Failed to fetch')>=0)return'連線失敗，請確認網站連線後再試。';return m||'登入失敗，請稍後再試。'}
  function setBusy(v){busy=!!v;var b=q('loginBtn');if(b){b.disabled=busy;b.textContent=busy?'登入中...':'登入'}}
  function doLogin(ev){if(window.tronAuth&&typeof window.tronAuth.login==='function')return window.tronAuth.login(ev);setMsg('登入元件尚未就緒。',false);return false}
  function bind(){var f=q('animatedLoginForm');if(f)f.onsubmit=doLogin;var u=q('loginUsername'),p=q('loginPassword');[u,p].forEach(function(el){if(el){el.addEventListener('input',updateFields);el.addEventListener('focus',updateFields);el.addEventListener('blur',updateFields);el.addEventListener('keydown',function(e){if(e.key==='Enter')doLogin(e)})}});var t=q('toggleLoginPassword');if(t)t.onclick=function(){var p=q('loginPassword');if(!p)return;p.type=p.type==='password'?'text':'password';t.textContent=p.type==='password'?'👁':'🙈'};var pk=q('passkeyLoginBtn');if(pk)pk.onclick=passkeyLogin}
  function updateFields(){document.querySelectorAll('.animated-login-field').forEach(function(w){var input=w.querySelector('input');w.classList.toggle('active',!!(input&&(input.value||document.activeElement===input)))})}
  function applyActive(){var loggedOut=document.documentElement.classList.contains('tron-auth-logged-out')||window.__tronAuthState==='out';var active=loggedOut&&!isLoggedIn()&&!!loginRoot();document.body.classList.toggle('animated-login-active',active);if(active){ensureCanvas();startParticles()}else stopParticles()}
  /* LOGIN_THEME_SYNC_V1: 登入畫面跟隨外觀設定（tronclass_app_settings_v1）；◐ 切換鈕同步回寫統一設定 */
  function loginThemeChoice(){try{var st=JSON.parse(localStorage.getItem('tronclass_app_settings_v1')||'{}');var t=String(st.theme||'').toLowerCase();return(t==='light'||t==='dark')?t:''}catch(_){return''}}
  function toggleDark(){
    var willBeDark=!document.documentElement.classList.contains('dark-mode');
    document.documentElement.classList.toggle('dark-mode',willBeDark);
    try{
      var st={};try{st=JSON.parse(localStorage.getItem('tronclass_app_settings_v1')||'{}')||{}}catch(_){st={}}
      st.theme=willBeDark?'dark':'light';
      localStorage.setItem('tronclass_app_settings_v1',JSON.stringify(st));
      localStorage.setItem('tronclass_login_dark',willBeDark?'1':'0');
    }catch(_){}
  }
  function ensureTheme(){
    if(q('animatedLoginThemeToggle'))return;
    var b=document.createElement('button');b.id='animatedLoginThemeToggle';b.className='animated-login-theme-toggle';b.type='button';b.textContent='◐';b.onclick=toggleDark;document.body.appendChild(b);
    try{
      var choice=loginThemeChoice();var dark;
      if(choice==='dark')dark=true;
      else if(choice==='light')dark=false;
      else{var saved=localStorage.getItem('tronclass_login_dark');dark=saved==='1'||(!saved&&matchMedia('(prefers-color-scheme: dark)').matches)}
      document.documentElement.classList.toggle('dark-mode',!!dark);
    }catch(_){}
    try{window.addEventListener('storage',function(ev){if(ev&&(!ev.key||ev.key==='tronclass_app_settings_v1')){var c=loginThemeChoice();if(c)document.documentElement.classList.toggle('dark-mode',c==='dark')}})}catch(_){}
  }
  function ensureCanvas(){if(q('animatedLoginParticles'))return;var c=document.createElement('canvas');c.id='animatedLoginParticles';document.body.prepend(c)}
  function resizeCanvas(c){c.width=innerWidth;c.height=innerHeight}
  /* DASHBOARD_LITE_MOTION_V1：登入粒子在減少動態／手機／省流量時不跑。 */
  function preferLiteVisuals(){try{if(document.documentElement.getAttribute('data-motion')==='reduced')return true;if(window.matchMedia&&matchMedia('(prefers-reduced-motion: reduce)').matches)return true;if(window.matchMedia&&matchMedia('(max-width: 700px)').matches)return true;var conn=navigator.connection||navigator.mozConnection||navigator.webkitConnection;if(conn&&(conn.saveData||/(^|-)2g$/.test(String(conn.effectiveType||''))))return true}catch(_){}return false}
  function startParticles(){if(preferLiteVisuals())return;var c=q('animatedLoginParticles');if(!c||raf)return;var ctx=c.getContext('2d');if(!ctx)return;resizeCanvas(c);particles=[];var count=Math.min(90,Math.floor(c.width*c.height/17000));for(var i=0;i<count;i++)particles.push({x:Math.random()*c.width,y:Math.random()*c.height,s:Math.random()*2.4+1,vx:(Math.random()-.5)*.45,vy:(Math.random()-.5)*.45});function loop(){if(!document.body.classList.contains('animated-login-active')||document.body.classList.contains('logged-in')){raf=0;return}ctx.clearRect(0,0,c.width,c.height);var dark=document.documentElement.classList.contains('dark-mode');particles.forEach(function(p){p.x+=p.vx;p.y+=p.vy;if(p.x<0)p.x=c.width;if(p.x>c.width)p.x=0;if(p.y<0)p.y=c.height;if(p.y>c.height)p.y=0;ctx.fillStyle=dark?'rgba(255,255,255,.16)':'rgba(37,99,235,.14)';ctx.beginPath();ctx.arc(p.x,p.y,p.s,0,Math.PI*2);ctx.fill()});raf=requestAnimationFrame(loop)}raf=requestAnimationFrame(loop);addEventListener('resize',function(){resizeCanvas(c)},{passive:true})}
  function stopParticles(){if(raf){cancelAnimationFrame(raf);raf=0}}
  function start(){purgeLegacyTokens();ensureTheme();ensureShell();applyActive();var n=0,t=setInterval(function(){ensureShell();applyActive();updateFields();n++;if(n>20)clearInterval(t)},500);try{new MutationObserver(function(){ensureShell();applyActive();updateFields()}).observe(document.documentElement,{childList:true,subtree:true,attributes:true,attributeFilter:['class','style']})}catch(_){}}
  document.addEventListener('click',function(e){var b=e.target&&e.target.closest&&e.target.closest('#loginBtn');if(b)doLogin(e)},true);
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',start);else start();
})();
