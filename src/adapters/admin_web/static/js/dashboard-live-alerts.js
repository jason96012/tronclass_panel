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
  /* ROLLCALL_ALERT_HAPTICS_V1：設定多了 vibrate（預設開）與 repeat（預設開：彈窗沒被處理時每 20 秒再響一次，最多 3 次）。 */
  function soundSettings(){try{return Object.assign({enabled:true,volume:0.35,pattern:'double',vibrate:true,repeat:true},JSON.parse(localStorage.getItem(soundKey)||'{}'))}catch(_){return {enabled:true,volume:0.35,pattern:'double',vibrate:true,repeat:true}}}
  function vibrateAlert(){try{var opt=soundSettings();if(!opt.vibrate||typeof navigator.vibrate!=='function')return;navigator.vibrate([180,90,180,90,320])}catch(_){}}
  /* 非阻斷式 toast（沿用 dashboard 的 #toast），讓彈窗被關掉後主畫面上仍留一句「哪個帳號、哪堂課」的提示。 */
  function miniToast(text){try{var el=document.getElementById('toast');if(!el)return;el.textContent=text;el.className='toast visible ok';clearTimeout(el._timer);el._timer=setTimeout(function(){el.classList.remove('visible')},6000)}catch(_){}}
  var repeatTimer=null,repeatCount=0;
  function stopRepeat(){if(repeatTimer){clearInterval(repeatTimer);repeatTimer=null}repeatCount=0}
  function startRepeat(){stopRepeat();if(!soundSettings().repeat)return;repeatTimer=setInterval(function(){if(!box.classList.contains('visible')||repeatCount>=3){stopRepeat();return}repeatCount+=1;playAlertSound(false);vibrateAlert()},20000)}
  /* 原始網頁標題輪流閃「● 偵測到點名」，分頁在背景時也看得到。 */
  var baseTitle=document.title,titleTimer=null;
  function flashTitle(on){if(titleTimer){clearInterval(titleTimer);titleTimer=null;document.title=baseTitle}if(!on)return;var flip=false;titleTimer=setInterval(function(){if(!box.classList.contains('visible')){flashTitle(false);return}flip=!flip;document.title=flip?'● 偵測到點名 — '+baseTitle:baseTitle},1200)}
  function saveSoundSettings(next){var value=Object.assign(soundSettings(),next||{});try{localStorage.setItem(soundKey,JSON.stringify(value))}catch(_){}return value}
  function playAlertSound(force){var opt=soundSettings();if(!force&&!opt.enabled)return;try{var Ctx=window.AudioContext||window.webkitAudioContext;if(!Ctx)return;var ctx=window.__tronRollcallAudioContext||(window.__tronRollcallAudioContext=new Ctx());if(ctx.state==='suspended')ctx.resume();var volume=Math.max(0,Math.min(1,Number(opt.volume == null ? 0.35 : opt.volume)));var notes=opt.pattern==='chime'?[659,880,1047]:[880,880];notes.forEach(function(freq,i){var start=ctx.currentTime+i*.18,osc=ctx.createOscillator(),gain=ctx.createGain();osc.type='sine';osc.frequency.value=freq;gain.gain.setValueAtTime(.0001,start);gain.gain.exponentialRampToValueAtTime(Math.max(.001,volume*.14),start+.018);gain.gain.exponentialRampToValueAtTime(.0001,start+.15);osc.connect(gain);gain.connect(ctx.destination);osc.start(start);osc.stop(start+.17)});}catch(_){}}
  window.TronclassRollcallAlertSettings={get:soundSettings,set:saveSoundSettings,test:function(){playAlertSound(true)}};
  function browserNotice(item){try{if(!('Notification'in window)||Notification.permission!=='granted')return;var n=new Notification('偵測到點名：'+item.course,{body:'按一下可直接前往點名頁',tag:'tronclass-rollcall-'+item.key,renotify:true});n.onclick=function(){try{window.focus();window.open(item.url,'_blank','noopener')}catch(_){}}}catch(_){}}
  function show(item){current=item;title.textContent=item.course||'偵測到新點名';meta.innerHTML='帳號：'+esc(item.account)+'<br>偵測時間：'+esc(new Date(item.timestamp*1000).toLocaleString());go.href=item.url;box.classList.add('visible');playAlertSound(false);vibrateAlert();browserNotice(item);miniToast('偵測到點名：'+(item.course||'新點名')+'（'+(item.account||'')+'）');startRepeat();flashTitle(true);
    /* ROLLCALL_ALERT_HAPTICS_V1：焦點移到「前往點名」，鍵盤／讀屏使用者按 Enter 即可開啟。 */
    try{go.focus({preventScroll:true})}catch(_){}}
  function check(){var runtime=window.TronclassRuntime;var request;if(runtime&&runtime.refreshStatus){request=runtime.refreshStatus({include_history:false,history_limit:0});}else{request=fetch('/admin/api/status?include_history=false&history_limit=0',{credentials:'same-origin',cache:'no-store'}).then(function(r){if(!r.ok)throw new Error('status '+r.status);return r.json();});}request.then(function(st){var accounts=(st&&st.accounts)||[],now=Date.now()/1000,s=seen();for(var i=0;i<accounts.length;i++){var a=accounts[i]||{},url=safeUrl(a.last_detected_url),ts=Number(a.last_detected_timestamp||0);if(!url||!ts||now-ts>1800)continue;var key=[a.account_id||'',a.last_detected_rollcall_id||'',ts].join(':');if(s[key])continue;var item={key:key,url:url,course:a.last_detected_course||'新點名',account:a.account_id||'',timestamp:ts};mark(key);show(item);break}}).catch(function(){});}
  // SSE delivers a point-in-time alert from the monitor (normally within a
  // few hundred milliseconds). The periodic status check below remains only
  // as a fallback for proxies/browsers that do not keep an event stream open.
  /* ROLLCALL_SSE_WATCHDOG_V1：原本只建一次 EventSource——(1) 伺服器重啟／401 時瀏覽器會把
     readyState 設為 CLOSED 並放棄，不再自動重連；(2) iOS 回到前景後 socket 常已死但不觸發
     onerror（readyState 停在 CONNECTING／OPEN）。兩種情況徽章都永遠停在「連線中」。
     現在：連線 20 秒內沒 open／之後 45 秒沒任何 ping 就視為殭屍，關掉重連（5s→10s→…→60s 退避）；
     回到前景時也檢查一次。 */
  var liveSource=null,liveRetryTimer=null,liveRetryDelay=5000,liveLastPingAt=0,liveOpenTimer=null,liveStaleTimer=null;
  var LIVE_OPEN_TIMEOUT_MS=20000,LIVE_STALE_MS=45000;
  function closeLive(){if(liveSource){try{liveSource.close()}catch(_){}liveSource=null}clearTimeout(liveOpenTimer);clearTimeout(liveStaleTimer);liveOpenTimer=null;liveStaleTimer=null}
  function scheduleLiveReconnect(){clearTimeout(liveRetryTimer);var d=liveRetryDelay;liveRetryDelay=Math.min(liveRetryDelay*2,60000);liveRetryTimer=setTimeout(function(){liveRetryTimer=null;if(!document.hidden)connectLiveEvents();else scheduleLiveReconnect()},d)}
  function armStaleTimer(){clearTimeout(liveStaleTimer);liveStaleTimer=setTimeout(function(){if(!liveSource)return;if((Date.now()-liveLastPingAt)>=LIVE_STALE_MS){setLiveStatus('reconnecting');closeLive();scheduleLiveReconnect()}else armStaleTimer()},LIVE_STALE_MS)}
  function connectLiveEvents(){
    if(!('EventSource' in window)){setLiveStatus('reconnecting');return;}
    closeLive();
    try{
      setLiveStatus('connecting');
      var source=new EventSource('/admin/api/rollcall/events');
      liveSource=source;
      // Keep a reference so a browser does not garbage-collect an otherwise
      // background EventSource connection.
      window.__tronRollcallEventSource=source;
      liveOpenTimer=setTimeout(function(){if(liveSource===source&&source.readyState!==EventSource.OPEN){setLiveStatus('reconnecting');closeLive();scheduleLiveReconnect()}},LIVE_OPEN_TIMEOUT_MS);
      source.onopen=function(){if(liveSource!==source)return;liveRetryDelay=5000;liveLastPingAt=Date.now();clearTimeout(liveOpenTimer);liveOpenTimer=null;setLiveStatus('connected');armStaleTimer()};
      source.onerror=function(){if(liveSource!==source)return;setLiveStatus('reconnecting');if(source.readyState===EventSource.CLOSED){closeLive();scheduleLiveReconnect()}};
      source.addEventListener('ping',function(){if(liveSource!==source)return;liveLastPingAt=Date.now();if(source.readyState===EventSource.OPEN)setLiveStatus('connected');});
      source.addEventListener('rollcall',function(ev){
        if(liveSource!==source)return;
        liveLastPingAt=Date.now();
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
    }catch(_){setLiveStatus('reconnecting');scheduleLiveReconnect();}
  }
  document.addEventListener('visibilitychange',function(){if(document.hidden)return;if(!liveSource||liveSource.readyState===EventSource.CLOSED||(Date.now()-liveLastPingAt)>LIVE_STALE_MS){liveRetryDelay=5000;clearTimeout(liveRetryTimer);liveRetryTimer=null;connectLiveEvents()}},{passive:true});
  window.addEventListener('online',function(){liveRetryDelay=5000;clearTimeout(liveRetryTimer);liveRetryTimer=null;connectLiveEvents()});
  window.addEventListener('pagehide',function(){closeLive()});
  function dismiss(){box.classList.remove('visible');stopRepeat();flashTitle(false)}
  go.addEventListener('click',dismiss);close.addEventListener('click',dismiss);box.addEventListener('click',function(e){if(e.target===box)dismiss()});
  document.addEventListener('keydown',function(e){if(e.key==='Escape'&&box.classList.contains('visible'))dismiss()});
  notify.addEventListener('click',function(){if(!('Notification'in window)){notify.textContent='此瀏覽器不支援';return}Notification.requestPermission().then(function(p){notify.textContent=p==='granted'?'瀏覽器通知已開啟':'通知未被允許';if(p==='granted'&&current)browserNotice(current)}).catch(function(){})});
  // Force a fresh status read: dashboard rendering may use a short cache, but a
  // safety notification must not wait for the user to reload the page.
  connectLiveEvents();
  window.setTimeout(check,1200);window.setInterval(function(){if(!document.hidden)check()},30000);
})();
