(function(){
  'use strict';
  if(window.DASHBOARD_MATCH_LOGIN_BACKGROUND_V1) return;
  window.DASHBOARD_MATCH_LOGIN_BACKGROUND_V1 = true;

  var raf = 0;
  var particles = [];

  function q(id){ return document.getElementById(id); }
  function isDashboardPath(){
    return location.pathname === '/admin' || location.pathname === '/admin/' || location.pathname === '/admin/dashboard';
  }
  function visible(el){
    if(!el) return false;
    try{
      var st = getComputedStyle(el);
      if(st.display === 'none' || st.visibility === 'hidden' || Number(st.opacity) === 0) return false;
      var r = el.getBoundingClientRect();
      return r.width > 0 && r.height > 0;
    }catch(_){ return false; }
  }
  function loginVisible(){
    return visible(q('loginCard') || q('loginPanel') || document.querySelector('.login-card'));
  }
  function loggedIn(){
    return document.body.classList.contains('logged-in') ||
      document.documentElement.classList.contains('dash-auth-ok') ||
      document.body.classList.contains('auth-ok') ||
      document.body.classList.contains('dash-auth-ok') ||
      visible(document.querySelector('.protected')) && !loginVisible();
  }
  function shouldEnable(){
    return isDashboardPath() && loggedIn() && !loginVisible();
  }
  function ensureCanvas(){
    var c = q('dashboardLoginBgParticlesV1');
    if(c) return c;
    c = document.createElement('canvas');
    c.id = 'dashboardLoginBgParticlesV1';
    c.setAttribute('aria-hidden','true');
    document.body.prepend(c);
    return c;
  }
  function resize(c){
    var dpr = Math.min(window.devicePixelRatio || 1, 2);
    c.width = Math.floor(window.innerWidth * dpr);
    c.height = Math.floor(window.innerHeight * dpr);
    c.style.width = window.innerWidth + 'px';
    c.style.height = window.innerHeight + 'px';
    var ctx = c.getContext('2d');
    if(ctx) ctx.setTransform(dpr,0,0,dpr,0,0);
  }
  function initParticles(c){
    particles = [];
    var count = Math.min(95, Math.floor((window.innerWidth * window.innerHeight) / 15000));
    for(var i=0;i<count;i++){
      particles.push({
        x: Math.random()*window.innerWidth,
        y: Math.random()*window.innerHeight,
        s: Math.random()*2.2 + .8,
        vx: (Math.random()-.5)*.36,
        vy: (Math.random()-.5)*.36,
        a: Math.random()*.18 + .05
      });
    }
  }
  function startParticles(){
    var c = ensureCanvas();
    var ctx = c.getContext('2d');
    if(!ctx || raf) return;
    resize(c);
    initParticles(c);
    function loop(){
      if(!document.body.classList.contains('dashboard-login-bg-active')){
        raf = 0;
        return;
      }
      ctx.clearRect(0,0,window.innerWidth,window.innerHeight);
      for(var i=0;i<particles.length;i++){
        var p = particles[i];
        p.x += p.vx; p.y += p.vy;
        if(p.x < 0) p.x = window.innerWidth;
        if(p.x > window.innerWidth) p.x = 0;
        if(p.y < 0) p.y = window.innerHeight;
        if(p.y > window.innerHeight) p.y = 0;
        ctx.fillStyle = 'rgba(226, 239, 255,' + p.a + ')';
        ctx.beginPath();
        ctx.arc(p.x,p.y,p.s,0,Math.PI*2);
        ctx.fill();
      }
      raf = requestAnimationFrame(loop);
    }
    raf = requestAnimationFrame(loop);
  }
  function stopParticles(){
    if(raf){ cancelAnimationFrame(raf); raf = 0; }
  }
  function update(){
    var active = shouldEnable();
    document.body.classList.toggle('dashboard-login-bg-active', active);
    if(active) startParticles(); else stopParticles();
  }
  function start(){
    update();
    window.addEventListener('resize', function(){
      var c = q('dashboardLoginBgParticlesV1');
      if(c){ resize(c); initParticles(c); }
    }, {passive:true});
    var n=0;
    var timer=setInterval(function(){ update(); n++; if(n>=40) clearInterval(timer); }, 600);
    try{ new MutationObserver(update).observe(document.documentElement,{childList:true,subtree:true,attributes:true,attributeFilter:['class','style']}); }catch(_){ }
    document.addEventListener('visibilitychange', function(){ if(!document.hidden) update(); });
    window.addEventListener('hashchange', update);
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
