(function(){
  if(window.MORE_MODAL_VISIBLE_V1) return;
  window.MORE_MODAL_VISIBLE_V1 = true;

  var LAYER_CSS = 'position:fixed;top:0;left:0;right:0;bottom:0;width:100%;height:100%;min-height:100dvh;display:flex;align-items:center;justify-content:center;padding:18px 16px;box-sizing:border-box;background:rgba(15,23,42,.55);z-index:2147483647;-webkit-overflow-scrolling:touch;';
  var CARD_CSS = 'width:min(420px,100%);max-height:min(86dvh,640px);overflow:auto;background:#fff;color:#172033;border-radius:22px;padding:20px 16px 14px;box-shadow:0 24px 70px rgba(15,23,42,.4);box-sizing:border-box;';

  function $(id){ return document.getElementById(id); }
  function closeMore(){
    ['mobileMoreFinalV2','mobileSheet'].forEach(function(id){
      var el=$(id); if(el){ el.classList.remove('visible'); }
    });
  }
  function host(){ return document.documentElement || document.body; }

  function showPassword(){
    closeMore();
    var old=$('tronCenterPw');
    if(old) old.remove();
    var layer=document.createElement('div');
    layer.id='tronCenterPw';
    layer.setAttribute('style', LAYER_CSS);
    layer.innerHTML='<div style="'+CARD_CSS+'">'
      +'<h3 style="margin:0 0 8px;font-size:18px;font-weight:900">變更密碼</h3>'
      +'<p style="margin:0 0 12px;font-size:13px;line-height:1.5;color:#64748b;font-weight:700">這是 Dashboard 專用密碼，不是 Tronclass 密碼。</p>'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:8px 0 6px">目前 Dashboard 密碼</label>'
      +'<input id="tronCPw1" type="password" autocomplete="current-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:10px 0 6px">新密碼（至少 10 字）</label>'
      +'<input id="tronCPw2" type="password" autocomplete="new-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<label style="display:block;font-size:12px;font-weight:800;color:#64748b;margin:10px 0 6px">再輸入一次新密碼</label>'
      +'<input id="tronCPw3" type="password" autocomplete="new-password" style="width:100%;min-height:46px;border-radius:14px;border:1px solid #dbe3ef;background:#f8fafc;padding:0 12px;font-size:16px;box-sizing:border-box">'
      +'<div id="tronCPwMsg" style="min-height:18px;margin:10px 0 0;font-size:13px;font-weight:800;color:#64748b"></div>'
      +'<div style="display:flex;gap:8px;margin-top:14px">'
      +'<button id="tronCPwNo" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#e2e8f0;font-size:15px;font-weight:850">取消</button>'
      +'<button id="tronCPwYes" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#2563eb;color:#fff;font-size:15px;font-weight:850">儲存</button>'
      +'</div></div>';
    host().appendChild(layer);
    layer.addEventListener('click', function(ev){ if(ev.target===layer) layer.remove(); });
    $('tronCPwNo').onclick=function(){ layer.remove(); };
    $('tronCPwYes').onclick=savePassword;
    // iPhone 不要自動 focus，否則鍵盤會把視窗推到螢幕外
  }
  async function savePassword(){
    var a=($('tronCPw1')&&$('tronCPw1').value)||'';
    var b=($('tronCPw2')&&$('tronCPw2').value)||'';
    var c=($('tronCPw3')&&$('tronCPw3').value)||'';
    var msg=$('tronCPwMsg');
    function set(tx, bad){ if(msg){ msg.textContent=tx; msg.style.color=bad?'#b91c1c':'#15803d'; } }
    if(!a){ set('請輸入目前密碼。', true); return; }
    if(!b||!c){ set('請輸入並確認新密碼。', true); return; }
    if(b!==c){ set('兩次輸入的新密碼不一致。', true); return; }
    if(b.length<10){ set('新密碼至少 10 個字元。', true); return; }
    try{
      var res=await fetch('/admin/api/password',{
        method:'POST', credentials:'same-origin', cache:'no-store',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({current_password:a, new_password:b})
      });
      var body=await res.json().catch(function(){return {};});
      if(!res.ok) throw new Error(body.error||body.message||('HTTP '+res.status));
      set('密碼已更新。', false);
      setTimeout(function(){ var el=$('tronCenterPw'); if(el) el.remove(); }, 600);
    }catch(e){
      var m=String((e&&e.message)||e||'');
      if(/incorrect|current Dashboard password|unauthorized|401/i.test(m)) m='目前密碼不正確，或登入已過期。';
      set(m||'變更密碼失敗', true);
    }
  }

  function showPasskeyCard(text, showRetry){
    var old=$('tronCenterPk');
    if(old) old.remove();
    var layer=document.createElement('div');
    layer.id='tronCenterPk';
    layer.setAttribute('style', LAYER_CSS);
    layer.innerHTML='<div style="'+CARD_CSS+'">'
      +'<h3 style="margin:0 0 8px;font-size:18px;font-weight:900">設定通行密鑰</h3>'
      +'<p id="tronCPkMsg" style="margin:0 0 14px;font-size:14px;line-height:1.55;color:#475569;font-weight:650"></p>'
      +'<div style="display:flex;gap:8px">'
      +'<button id="tronCPkNo" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#e2e8f0;font-size:15px;font-weight:850">關閉</button>'
      +'<button id="tronCPkGo" type="button" style="flex:1;min-height:46px;border:0;border-radius:14px;background:#2563eb;color:#fff;font-size:15px;font-weight:850">繼續設定</button>'
      +'</div></div>';
    host().appendChild(layer);
    $('tronCPkMsg').textContent=text||'準備建立通行密鑰…';
    $('tronCPkGo').style.display=showRetry?'block':'none';
    layer.addEventListener('click', function(ev){ if(ev.target===layer) layer.remove(); });
    $('tronCPkNo').onclick=function(){ layer.remove(); };
    $('tronCPkGo').onclick=function(){ startPasskey(true); };
    return layer;
  }
  function b64urlEncode(buf){
    var bytes=new Uint8Array(buf), s='';
    for(var i=0;i<bytes.length;i++) s+=String.fromCharCode(bytes[i]);
    return btoa(s).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'');
  }
  function b64urlDecode(str){
    str=String(str||'').replace(/-/g,'+').replace(/_/g,'/');
    while(str.length%4) str+='=';
    var bin=atob(str), out=new Uint8Array(bin.length);
    for(var i=0;i<bin.length;i++) out[i]=bin.charCodeAt(i);
    return out.buffer;
  }
  function prepareCreate(opts){
    opts=JSON.parse(JSON.stringify(opts||{}));
    opts.challenge=b64urlDecode(opts.challenge);
    if(opts.user&&opts.user.id) opts.user.id=b64urlDecode(opts.user.id);
    if(opts.excludeCredentials){
      opts.excludeCredentials=opts.excludeCredentials.map(function(c){
        return Object.assign({},c,{id:b64urlDecode(c.id)});
      });
    }
    return opts;
  }
  async function startPasskey(fromRetry){
    closeMore();
    if(!window.PublicKeyCredential||!navigator.credentials||!navigator.credentials.create){
      showPasskeyCard('此瀏覽器不支援通行密鑰。請用 iPhone 的 Safari 開啟。', false);
      return;
    }
    showPasskeyCard(fromRetry?'請在系統視窗完成 Face ID / 密碼。':'準備建立通行密鑰…', false);
    try{
      var res=await fetch('/admin/api/passkey/register/start',{
        method:'POST', credentials:'same-origin', cache:'no-store',
        headers:{'content-type':'application/json'}
      });
      var json=await res.json().catch(function(){return {};});
      if(!res.ok) throw new Error(json.error||json.message||('HTTP '+res.status));
      var cred=await navigator.credentials.create({publicKey:prepareCreate(json.publicKey)});
      if(!cred) throw new Error('未建立通行密鑰');
      var fin=await fetch('/admin/api/passkey/register/finish',{
        method:'POST', credentials:'same-origin', cache:'no-store',
        headers:{'content-type':'application/json'},
        body:JSON.stringify({
          ceremony_id:json.ceremony_id,
          response:{
            id:cred.id,
            attestationObject:b64urlEncode(cred.response.attestationObject),
            clientDataJSON:b64urlEncode(cred.response.clientDataJSON),
            transports:cred.response.getTransports?cred.response.getTransports():[]
          }
        })
      });
      var done=await fin.json().catch(function(){return {};});
      if(!fin.ok) throw new Error(done.error||done.message||('HTTP '+fin.status));
      showPasskeyCard('通行密鑰已設定完成，下次可用 Passkey 登入。', false);
    }catch(e){
      var m=String((e&&(e.name+' '+e.message))||e||'');
      var retry=/NotAllowed|gesture|沒有跳出|user verification/i.test(m);
      var msg=retry
        ? '系統沒有跳出驗證。請按「繼續設定」，並完成 Face ID / 密碼。'
        : (/401|unauthorized/i.test(m)?'登入已過期，請重新登入。':(e&&e.message)||'設定通行密鑰失敗');
      showPasskeyCard(msg, retry);
    }
  }

  function lock(name, fn){
    try{
      Object.defineProperty(window, name, {configurable:true, get:function(){ return fn; }, set:function(){}});
    }catch(_){ window[name]=fn; }
  }
  lock('tronOpenChangePassword', showPassword);
  // 包一層：先保證中央卡片有出來，再跑原本通行密鑰
  var prevPk = window.tronPasskeyRegister;
  lock('tronPasskeyRegister', function(){ startPasskey(false); });

  function hit(el){
    if(el && el.nodeType===3) el=el.parentElement;
    if(!el||!el.closest) return '';
    var a=el.closest('a,button,[data-more-action]');
    if(!a) return '';
    var href=(a.getAttribute('href')||'');
    var id=a.id||'';
    var act=a.getAttribute('data-more-action')||'';
    var tx=(a.textContent||'').replace(/\s+/g,'').trim();
    if(act==='password'||/ChangePassword|changePassword/i.test(id)||tx==='變更密碼'||href.indexOf('tron-password')>=0) return 'password';
    if(act==='passkey'||/passkey/i.test(id)||tx==='設定通行密鑰'||tx==='設定通行密碼'||href.indexOf('tron-passkey')>=0) return 'passkey';
    return '';
  }
  document.addEventListener('click', function(ev){
    var k=hit(ev.target);
    if(!k) return;
    ev.preventDefault();
    ev.stopPropagation();
    if(k==='password') showPassword();
    else startPasskey(false);
  }, true);
})();
