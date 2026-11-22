(function(){
  if(window.MORE_BUTTONS_NOW_V1) return;
  window.MORE_BUTTONS_NOW_V1 = true;
  function $(id){ return document.getElementById(id); }
  function closeMore(){
    var el=$('mobileMoreFinalV2'); if(el) el.classList.remove('visible');
    var s=$('mobileSheet'); if(s) s.classList.remove('visible');
  }
  function toast(msg, ok){
    var el=$('toast');
    if(el){
      el.textContent=msg;
      el.className='toast visible '+(ok===false?'bad':'ok');
      el.style.cssText='z-index:2147483647;position:fixed;left:50%;bottom:110px;transform:translateX(-50%);';
      clearTimeout(el._n); el._n=setTimeout(function(){ el.classList.remove('visible'); }, 3200);
      return;
    }
    try{ alert(msg); }catch(_){}
  }
  function ensurePw(){
    var w=$('tronMorePwNow');
    if(w) return w;
    w=document.createElement('div');
    w.id='tronMorePwNow';
    w.innerHTML='<div style="width:min(440px,100%);background:#fff;color:#172033;border-radius:22px;padding:20px 16px 14px;box-shadow:0 24px 70px rgba(15,23,42,.35)">'
      +'<h3 style="margin:0 0 8px;font-size:18px;font-weight:900">變更密碼</h3>'
      +'<p style="margin:0 0 10px;font-size:12px;color:#64748b;font-weight:700">這是 Dashboard 專用密碼，不是 Tronclass 密碼。</p>'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:10px 0 6px">目前 Dashboard 密碼</label>'
      +'<input id="tronPwCur" type="password" autocomplete="current-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:10px 0 6px">新密碼（至少 10 字）</label>'
      +'<input id="tronPwNew" type="password" autocomplete="new-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:10px 0 6px">再輸入一次新密碼</label>'
      +'<input id="tronPwCfm" type="password" autocomplete="new-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<div id="tronPwMsg" style="min-height:18px;margin:10px 0 0;font-size:13px;font-weight:800;color:#64748b"></div>'
      +'<div style="display:flex;gap:8px;margin-top:14px">'
      +'<button id="tronPwNo" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#e2e8f0;font-weight:850">取消</button>'
      +'<button id="tronPwYes" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#2563eb;color:#fff;font-weight:850">儲存</button>'
      +'</div></div>';
    document.body.appendChild(w);
    w.addEventListener('click', function(ev){ if(ev.target===w) hidePw(); });
    $('tronPwNo').onclick=hidePw;
    $('tronPwYes').onclick=savePw;
    return w;
  }
  function hidePw(){
    var w=$('tronMorePwNow');
    if(w){ w.classList.remove('on'); w.style.display='none'; }
  }
  function showPw(){
    var w=ensurePw();
    ['tronPwCur','tronPwNew','tronPwCfm'].forEach(function(id){ if($(id)) $(id).value=''; });
    var m=$('tronPwMsg'); if(m){ m.textContent=''; m.style.color='#64748b'; }
    w.classList.add('on');
    w.style.display='flex';
    w.style.zIndex='2147483647';
    // 不要先關更多再畫視窗：視窗蓋在上面即可
    setTimeout(function(){ closeMore(); if($('tronPwCur')) $('tronPwCur').focus(); }, 30);
  }
  async function savePw(){
    var cur=($('tronPwCur')&&$('tronPwCur').value)||'';
    var n=($('tronPwNew')&&$('tronPwNew').value)||'';
    var c=($('tronPwCfm')&&$('tronPwCfm').value)||'';
    var msg=$('tronPwMsg');
    function set(tx, bad){ if(msg){ msg.textContent=tx; msg.style.color=bad?'#b91c1c':'#15803d'; } }
    if(!cur){ set('請輸入目前密碼。', true); return; }
    if(!n||!c){ set('請輸入並確認新密碼。', true); return; }
    if(n!==c){ set('兩次輸入的新密碼不一致。', true); return; }
    if(n.length<10){ set('新密碼至少 10 個字元。', true); return; }
    try{
      var res=await fetch('/admin/api/password',{
        method:'POST', credentials:'same-origin', cache:'no-store',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({current_password:cur, new_password:n})
      });
      var body=await res.json().catch(function(){return {};});
      if(!res.ok) throw new Error(body.error||body.message||('HTTP '+res.status));
      set('密碼已更新。', false);
      toast('密碼已更新');
      setTimeout(hidePw, 500);
    }catch(e){
      var m=String((e&&e.message)||e||'');
      if(/incorrect|current Dashboard password|unauthorized|401/i.test(m)) m='目前密碼不正確，或登入已過期。';
      set(m||'變更密碼失敗', true);
    }
  }

  // 鎖住，後面的舊腳本不能再覆蓋成壞掉的 showModal
  try{
    Object.defineProperty(window, 'tronOpenChangePassword', {
      configurable:true,
      get:function(){ return showPw; },
      set:function(){}
    });
  }catch(_){
    window.tronOpenChangePassword=showPw;
  }

  function bindOne(el, kind){
    if(!el) return;
    var fn = kind==='passkey'
      ? function(ev){ if(ev){ev.preventDefault(); ev.stopPropagation();} if(window.tronPasskeyRegister) window.tronPasskeyRegister(); }
      : function(ev){ if(ev){ev.preventDefault(); ev.stopPropagation();} showPw(); };
    el.onclick=fn;
    el.style.pointerEvents='auto';
    el.style.touchAction='manipulation';
    if(!el._tronMoreBound){
      el._tronMoreBound=true;
      el.addEventListener('touchend', function(ev){ ev.preventDefault(); fn(ev); }, {passive:false});
    }
  }
  function bindAll(){
    bindOne($('morePasskeyFinalV2'), 'passkey');
    bindOne($('mPasskey'), 'passkey');
    bindOne($('passkeyRegisterBtn'), 'passkey');
    bindOne($('moreChangePasswordFinalV2'), 'password');
    bindOne($('mChangePassword'), 'password');
    bindOne($('changePasswordBtn'), 'password');
    var nodes=document.querySelectorAll('#mobileMoreFinalV2 button, #mobileMoreFinalV2 a, #mobileSheet button, .more-grid button, .sheet-grid button');
    for(var i=0;i<nodes.length;i++){
      var tx=(nodes[i].textContent||'').replace(/\s+/g,'').trim();
      if(tx==='設定通行密鑰'||tx==='設定通行密碼') bindOne(nodes[i],'passkey');
      if(tx==='變更密碼'||tx==='修改密碼') bindOne(nodes[i],'password');
    }
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', bindAll);
  else bindAll();
  setInterval(bindAll, 400);
})();
