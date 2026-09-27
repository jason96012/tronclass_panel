(function(){'use strict';
  if(window.ROLLCALL_QUICK_ALERT_V1)return;window.ROLLCALL_QUICK_ALERT_V1=true;
  var box=document.getElementById('rollcallQuickAlert'),title=document.getElementById('rollcallQuickAlertTitle'),meta=document.getElementById('rollcallQuickAlertMeta'),go=document.getElementById('rollcallQuickAlertGo'),notify=document.getElementById('rollcallQuickAlertNotify'),close=document.getElementById('rollcallQuickAlertClose');
  if(!box||!title||!meta||!go)return;
  var current=null,seenKey='tronclass_rollcall_quick_alert_seen_v1',liveStatus=null;
  function ensureLiveStatus(){
    if(liveStatus)return liveStatus;
    liveStatus=document.getElementById('rollcallLiveStatus');
    if(liveStatus)return liveStatus;
    // Keep the status beneath the Dashboard description. Inserting it into
    // .hero-top competes with the existing「更新」pill and shifts the layout.
    var host=document.querySelector('.hero-copy')||document.querySelector('.hero')||document.querySelector('main');
    if(!host)return null;
    liveStatus=document.createElement('span');liveStatus.id='rollcallLiveStatus';liveStatus.className='connecting';liveStatus.setAttribute('role','status');liveStatus.setAttribute('aria-live','polite');liveStatus.innerHTML='<span class="dot"></span><span class="label">即時點名：連線中</span>';
    host.appendChild(liveStatus);return liveStatus;
  }
  function setLiveStatus(state){
    var el=ensureLiveStatus();if(!el)return;
    var label=el.querySelector('.label');var now=new Date().toLocaleTimeString([], {hour:'2-digit',minute:'2-digit',second:'2-digit'});
    el.className=state;
    if(label)label.textContent=state==='connected'?'即時點名：已連線 · '+now:state==='reconnecting'?'即時點名：重新連線中':'即時點名：連線中';
  }
  function esc(v){return String(v==null?'':v).replace(/[&<>"']/g,function(c){return({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]})}
  function safeUrl(v){try{var u=new URL(String(v||''),location.origin);return /^https?:$/.test(u.protocol)?u.href:''}catch(_){return ''}}
  function seen(){try{return JSON.parse(sessionStorage.getItem(seenKey)||'{}')}catch(_){return {}}}
  function mark(key){try{var s=seen();s[key]=Date.now();sessionStorage.setItem(seenKey,JSON.stringify(s))}catch(_){}}
  var soundKey='tronclass_rollcall_sound_settings_v1';
  function soundSettings(){try{return Object.assign({enabled:true,volume:0.35,pattern:'double'},JSON.parse(localStorage.getItem(soundKey)||'{}'))}catch(_){return {enabled:true,volume:0.35,pattern:'double'}}}
  function saveSoundSettings(next){var value=Object.assign(soundSettings(),next||{});try{localStorage.setItem(soundKey,JSON.stringify(value))}catch(_){}return value}
  function playAlertSound(force){var opt=soundSettings();if(!force&&!opt.enabled)return;try{var Ctx=window.AudioContext||window.webkitAudioContext;if(!Ctx)return;var ctx=window.__tronRollcallAudioContext||(window.__tronRollcallAudioContext=new Ctx());if(ctx.state==='suspended')ctx.resume();var volume=Math.max(0,Math.min(1,Number(opt.volume == null ? 0.35 : opt.volume)));var notes=opt.pattern==='chime'?[659,880,1047]:[880,880];notes.forEach(function(freq,i){var start=ctx.currentTime+i*.18,osc=ctx.createOscillator(),gain=ctx.createGain();osc.type='sine';osc.frequency.value=freq;gain.gain.setValueAtTime(.0001,start);gain.gain.exponentialRampToValueAtTime(Math.max(.001,volume*.14),start+.018);gain.gain.exponentialRampToValueAtTime(.0001,start+.15);osc.connect(gain);gain.connect(ctx.destination);osc.start(start);osc.stop(start+.17)});}catch(_){}}
  window.TronclassRollcallAlertSettings={get:soundSettings,set:saveSoundSettings,test:function(){playAlertSound(true)}};
  function browserNotice(item){try{if(!('Notification'in window)||Notification.permission!=='granted')return;var n=new Notification('偵測到點名：'+item.course,{body:'按一下可直接前往點名頁',tag:'tronclass-rollcall-'+item.key,renotify:true});n.onclick=function(){try{window.focus();var u=safeUrl(item&&item.url);if(u)window.open(u,'_blank','noopener')}catch(_){}}}catch(_){}}
  function show(item){current=item;title.textContent=item.course||'偵測到新點名';meta.innerHTML='帳號：'+esc(item.account)+'<br>偵測時間：'+esc(new Date(item.timestamp*1000).toLocaleString());go.href=safeUrl(item&&item.url)||'#';box.classList.add('visible');playAlertSound(false);browserNotice(item)}
  function check(){var runtime=window.TronclassRuntime;var request;if(runtime&&runtime.refreshStatus){request=runtime.refreshStatus({include_history:false,history_limit:0});}else{request=fetch('/admin/api/status?include_history=false&history_limit=0',{credentials:'same-origin',cache:'no-store'}).then(function(r){if(!r.ok)throw new Error('status '+r.status);return r.json();});}request.then(function(st){var accounts=(st&&st.accounts)||[],now=Date.now()/1000,s=seen();for(var i=0;i<accounts.length;i++){var a=accounts[i]||{},url=safeUrl(a.last_detected_url),ts=Number(a.last_detected_timestamp||0);if(!url||!ts||now-ts>1800)continue;var key=[a.account_id||'',a.last_detected_rollcall_id||'',ts].join(':');if(s[key])continue;var item={key:key,url:url,course:a.last_detected_course||'新點名',account:a.account_id||'',timestamp:ts};mark(key);show(item);break}}).catch(function(){});}
  // SSE delivers a point-in-time alert from the monitor (normally within a
  // few hundred milliseconds). The periodic status check below remains only
  // as a fallback for proxies/browsers that do not keep an event stream open.
  function connectLiveEvents(){
    if(!('EventSource' in window)){setLiveStatus('reconnecting');return;}
    try{
      setLiveStatus('connecting');
      var source=new EventSource('/admin/api/rollcall/events');
      // Keep a reference so a browser does not garbage-collect an otherwise
      // background EventSource connection.
      window.__tronRollcallEventSource=source;
      source.onopen=function(){setLiveStatus('connected');};
      source.onerror=function(){setLiveStatus('reconnecting');};
      source.addEventListener('ping',function(){if(source.readyState===EventSource.OPEN)setLiveStatus('connected');});
      source.addEventListener('rollcall',function(ev){
        setLiveStatus('connected');
        var items=[];try{items=JSON.parse(ev.data||'[]')}catch(_){}
        var s=seen();
        for(var i=0;i<items.length;i++){
          var raw=items[i]||{},url=safeUrl(raw.url),key=String(raw.key||'');
          if(!url||!key||s[key])continue;
          var item={key:key,url:url,course:raw.course||'新點名',account:raw.account||'',timestamp:Number(raw.timestamp||Date.now()/1000)};
          mark(key);show(item);break;
        }
      });
    }catch(_){setLiveStatus('reconnecting');}
  }
  go.addEventListener('click',function(){box.classList.remove('visible')});close.addEventListener('click',function(){box.classList.remove('visible')});box.addEventListener('click',function(e){if(e.target===box)box.classList.remove('visible')});
  notify.addEventListener('click',function(){if(!('Notification'in window)){notify.textContent='此瀏覽器不支援';return}Notification.requestPermission().then(function(p){notify.textContent=p==='granted'?'瀏覽器通知已開啟':'通知未被允許';if(p==='granted'&&current)browserNotice(current)}).catch(function(){})});
  // Force a fresh status read: dashboard rendering may use a short cache, but a
  // safety notification must not wait for the user to reload the page.
  connectLiveEvents();
  window.setTimeout(check,1200);window.setInterval(function(){if(!document.hidden)check()},30000);
})();
