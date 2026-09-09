/* INLINE_SPLIT_V1：ai-page.js 原本內嵌於 ai.html 的 AI_CHAT_PAGE_V1 頁面腳本，抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: AI_CHAT_PAGE_V1 (原 ai.html:403) ── */
(function(){
  'use strict';

  // ── 小工具 ─────────────────────────────────────────────
  function q(id){return document.getElementById(id);}
  function el(tag,cls,text){
    var node=document.createElement(tag);
    if(cls)node.className=cls;
    if(text!=null)node.textContent=String(text);
    return node;
  }
  function clear(node){while(node.firstChild)node.removeChild(node.firstChild);}

  var toastTimer=null;
  function toast(message,bad){
    var box=q('toast');
    box.textContent=message;
    box.className='toast show'+(bad?' bad':'');
    if(toastTimer)clearTimeout(toastTimer);
    toastTimer=setTimeout(function(){box.className='toast';},2600);
  }

  function postJson(path,data,cb){
    var done=false,st=0;
    function once(o){if(!done){done=true;cb(o);}}
    fetch(path,{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json'},body:JSON.stringify(data||{})})
      .then(function(r){st=r.status;return r.text().then(function(raw){
        var j=null;try{j=JSON.parse(raw);}catch(e){}
        if(!r.ok){once({error:(j&&j.error)||('HTTP '+r.status),status:r.status,code:(j&&j.code)||'',raw:j?'':String(raw||'').slice(0,400)});return;}
        once(j||{});
      });})
      .catch(function(e){once({error:(e&&e.message)||'網路錯誤',status:st});});
  }
  function getJson(path,cb){
    var done=false,st=0;
    function once(o){if(!done){done=true;cb(o);}}
    fetch(path,{credentials:'same-origin',cache:'no-store'})
      .then(function(r){st=r.status;return r.text().then(function(raw){
        var j=null;try{j=JSON.parse(raw);}catch(e){}
        if(!r.ok){once({error:(j&&j.error)||('HTTP '+r.status),status:r.status,code:(j&&j.code)||'',raw:j?'':String(raw||'').slice(0,400)});return;}
        once(j||{});
      });})
      .catch(function(e){once({error:(e&&e.message)||'網路錯誤',status:st});});
  }

  // ── 狀態 ───────────────────────────────────────────────
  var STATE={keys:[],settings:{},threads:[],providers:[],models:[],can_manage:false,usable_keys:0,usable_models:0};
  var currentModel='';
  var currentThread='';
  var currentMessages=[];
  var editingKey='';
  var activeJob=null;
  var pollTimer=null;
  var POLL_MS=350;
  // AI_CHAT_V8：SSE 串流。連不上就自動退回輪詢。
  var activeStream=null;
  var streamBuf='';
  var renderTimer=null;

  // ── Markdown → DOM ─────────────────────────────────────
  // CSP 有 require-trusted-types-for 'script'，innerHTML 會直接 throw，
  // 所以整棵樹都要用 createElement / textContent 自己組。
  function inlineInto(parent,text){
    // 依序處理 `code`、**粗體**、*斜體*、[文字](網址)
    var pattern=/(`[^`\n]+`)|(\*\*[^*]+\*\*)|(\*[^*\n]+\*)|(\[[^\]\n]+\]\([^)\s]+\))/;
    var rest=String(text);
    while(rest.length){
      var m=pattern.exec(rest);
      if(!m){parent.appendChild(document.createTextNode(rest));break;}
      if(m.index>0)parent.appendChild(document.createTextNode(rest.slice(0,m.index)));
      var token=m[0];
      if(token.charAt(0)==='`'){
        parent.appendChild(el('code','inline',token.slice(1,-1)));
      }else if(token.slice(0,2)==='**'){
        parent.appendChild(el('strong',null,token.slice(2,-2)));
      }else if(token.charAt(0)==='*'){
        parent.appendChild(el('em',null,token.slice(1,-1)));
      }else{
        var split=token.indexOf('](');
        var label=token.slice(1,split);
        var href=token.slice(split+2,-1);
        var link=el('a',null,label);
        if(/^https?:\/\//i.test(href)){link.href=href;link.target='_blank';link.rel='noopener noreferrer';}
        parent.appendChild(link);
      }
      rest=rest.slice(m.index+token.length);
    }
  }

  function codeBlock(lang,code){
    var box=el('div','codeblk');
    var head=el('div','codehead');
    head.appendChild(el('span','lang',lang||'code'));
    var copy=el('button','copybtn','複製');
    copy.type='button';
    copy.addEventListener('click',function(){
      if(navigator.clipboard&&navigator.clipboard.writeText){
        navigator.clipboard.writeText(code).then(function(){
          copy.textContent='已複製';
          setTimeout(function(){copy.textContent='複製';},1400);
        },function(){toast('複製失敗',true);});
      }else{toast('這個瀏覽器不支援複製',true);}
    });
    head.appendChild(copy);
    box.appendChild(head);
    var pre=el('pre');
    pre.appendChild(el('code',null,code));
    box.appendChild(pre);
    return box;
  }

  function renderMarkdown(target,text){
    clear(target);
    var lines=String(text==null?'':text).split('\n');
    var i=0,list=null;
    function closeList(){list=null;}
    while(i<lines.length){
      var line=lines[i];
      var fence=/^\s*```(.*)$/.exec(line);
      if(fence){
        closeList();
        var lang=fence[1].trim();
        var buf=[];
        i++;
        while(i<lines.length&&!/^\s*```/.test(lines[i])){buf.push(lines[i]);i++;}
        i++; // 吃掉結尾的 ```
        target.appendChild(codeBlock(lang,buf.join('\n')));
        continue;
      }
      if(/^\s*$/.test(line)){closeList();i++;continue;}
      if(/^\s*([-*_])\s*\1\s*\1[\s\-*_]*$/.test(line)){closeList();target.appendChild(el('hr'));i++;continue;}
      var heading=/^(#{1,3})\s+(.*)$/.exec(line);
      if(heading){
        closeList();
        var h=el('h'+heading[1].length);
        inlineInto(h,heading[2]);
        target.appendChild(h);i++;continue;
      }
      var quote=/^>\s?(.*)$/.exec(line);
      if(quote){
        closeList();
        var bq=el('blockquote');
        inlineInto(bq,quote[1]);
        target.appendChild(bq);i++;continue;
      }
      var bullet=/^\s*[-*+]\s+(.*)$/.exec(line);
      var numbered=/^\s*\d+[.)]\s+(.*)$/.exec(line);
      if(bullet||numbered){
        var want=bullet?'ul':'ol';
        if(!list||list.tagName.toLowerCase()!==want){
          list=el(want);
          target.appendChild(list);
        }
        var li=el('li');
        inlineInto(li,(bullet||numbered)[1]);
        list.appendChild(li);
        i++;continue;
      }
      closeList();
      // 連續的普通行併成一段
      var para=[line];
      i++;
      while(i<lines.length&&!/^\s*$/.test(lines[i])&&!/^\s*```/.test(lines[i])
            &&!/^(#{1,3})\s+/.test(lines[i])&&!/^>\s?/.test(lines[i])
            &&!/^\s*[-*+]\s+/.test(lines[i])&&!/^\s*\d+[.)]\s+/.test(lines[i])){
        para.push(lines[i]);i++;
      }
      var p=el('p');
      inlineInto(p,para.join('\n'));
      target.appendChild(p);
    }
  }

  // ── AI_CHAT_V7：附件（圖片／文字檔）───────────────────────
  var MAX_FILES=6, MAX_IMG=4*1024*1024, MAX_TXT=256*1024;
  var IMG_MIME={'image/png':1,'image/jpeg':1,'image/jpg':1,'image/webp':1,'image/gif':1};
  var TEXT_EXT=['txt','md','markdown','csv','tsv','json','log','xml','yml','yaml','ini','conf','cfg',
                'sql','js','mjs','ts','jsx','tsx','py','rs','java','kt','go','rb','php','c','h','cpp',
                'hpp','cs','css','scss','html','htm','sh','bash','env','toml','properties'];
  var pending=[];

  function extOf(name){
    var i=String(name||'').lastIndexOf('.');
    return i<0?'':String(name).slice(i+1).toLowerCase();
  }
  function humanSize(n){
    if(n<1024)return n+' B';
    if(n<1024*1024)return (n/1024).toFixed(0)+' KB';
    return (n/1024/1024).toFixed(1)+' MB';
  }
  function classify(file){
    var mime=(file.type||'').toLowerCase();
    if(IMG_MIME[mime])return 'image';
    if(mime.indexOf('image/')===0)return 'bad-image';
    if(mime.indexOf('text/')===0)return 'text';
    if(TEXT_EXT.indexOf(extOf(file.name))>=0)return 'text';
    if(mime==='application/json'||mime==='application/xml')return 'text';
    return 'unknown';
  }

  function renderAttachBar(){
    var bar=q('attachBar');
    clear(bar);
    pending.forEach(function(item,index){
      var chip=el('div','chip');
      if(item.kind==='image'){
        var img=document.createElement('img');
        img.src=item.preview;
        img.alt=item.name;
        chip.appendChild(img);
      }else{
        chip.appendChild(el('div','fi','📄'));
      }
      var box=el('div');
      box.style.minWidth='0';
      box.appendChild(el('div','nm',item.name));
      box.appendChild(el('div','sz',humanSize(item.size)));
      chip.appendChild(box);
      var x=el('button','x','✕');
      x.type='button';
      x.title='移除';
      x.addEventListener('click',function(){pending.splice(index,1);renderAttachBar();});
      chip.appendChild(x);
      bar.appendChild(chip);
    });
  }

  function addFiles(files){
    var list=Array.prototype.slice.call(files||[]);
    if(!list.length)return;
    list.forEach(function(file){
      if(pending.length>=MAX_FILES){toast('一次最多只能附 '+MAX_FILES+' 個檔案',true);return;}
      var kind=classify(file);
      if(kind==='bad-image'){toast('「'+file.name+'」的圖片格式不支援，請用 PNG／JPG／WebP／GIF',true);return;}
      if(kind==='unknown'){toast('「'+file.name+'」不支援。目前只能附圖片和純文字類檔案（PDF、Word 還不行）',true);return;}
      if(kind==='image'&&file.size>MAX_IMG){toast('「'+file.name+'」有 '+humanSize(file.size)+'，圖片上限 4 MB',true);return;}
      if(kind==='text'&&file.size>MAX_TXT){toast('「'+file.name+'」有 '+humanSize(file.size)+'，文字檔上限 256 KB',true);return;}
      var slot={name:file.name||(kind==='image'?'貼上的圖片.png':'附件.txt'),
                mime:(file.type||'').toLowerCase(),kind:kind,size:file.size,
                data:'',text:'',preview:'',ready:false};
      pending.push(slot);
      renderAttachBar();
      var reader=new FileReader();
      reader.onload=function(){
        var out=String(reader.result||'');
        if(kind==='image'){
          slot.preview=out;
          var cut=out.indexOf('base64,');
          slot.data=cut>=0?out.slice(cut+7):'';
          if(!slot.mime){slot.mime='image/png';}
          if(!slot.data){dropSlot(slot,'「'+slot.name+'」讀取失敗');return;}
        }else{
          slot.text=out;
          if(!out.trim()){dropSlot(slot,'「'+slot.name+'」是空的');return;}
        }
        slot.ready=true;
        renderAttachBar();
      };
      reader.onerror=function(){dropSlot(slot,'「'+slot.name+'」讀取失敗');};
      if(kind==='image')reader.readAsDataURL(file);
      else reader.readAsText(file);
    });
  }
  function dropSlot(slot,message){
    var i=pending.indexOf(slot);
    if(i>=0)pending.splice(i,1);
    renderAttachBar();
    toast(message,true);
  }

  // ── 訊息渲染 ───────────────────────────────────────────
  function messageNode(role,content,via,atts){
    var wrap=el('div','msg '+(role==='user'?'user':'ai'));
    wrap.appendChild(el('div','avatar',role==='user'?'你':'AI'));
    var body=el('div','body');
    var who=el('div','who');
    who.appendChild(el('span',null,role==='user'?'你':'AI 助理'));
    if(via)who.appendChild(el('span','via',via));
    body.appendChild(who);
    var bubble=el('div','bubble');
    if(role==='user'){
      // 使用者輸入不套 Markdown，原樣顯示比較不會誤解。
      var p=el('p');
      p.style.whiteSpace='pre-wrap';
      p.textContent=content;
      bubble.appendChild(p);
    }else{
      renderMarkdown(bubble,content);
    }
    body.appendChild(bubble);
    if(atts&&atts.length){
      var row=el('div','msgatt');
      atts.forEach(function(name){row.appendChild(el('span','','📎 '+name));});
      body.appendChild(row);
    }
    wrap.appendChild(body);
    wrap._bubble=bubble;
    wrap._who=who;
    return wrap;
  }

  function atBottom(){
    var s=q('stream');
    return s.scrollHeight-s.scrollTop-s.clientHeight<80;
  }
  function scrollDown(force){
    var s=q('stream');
    if(force||atBottom())s.scrollTop=s.scrollHeight;
  }

  function renderThread(){
    var inner=q('inner');
    clear(inner);
    if(!currentMessages.length){
      var box=el('div','empty');
      box.appendChild(el('h2','','要聊點什麼？'));
      var tip;
      if(STATE.usable_models>0){
        if(currentModel==='auto'){
          tip='目前是「自動」模式：系統會依你的問題挑最合適的模型（共 '+STATE.usable_models+' 個可用），也可在右上角自己指定。';
        }else{
          var picked=null;
          (STATE.models||[]).forEach(function(m){if(m.id===currentModel)picked=m;});
          tip='目前使用 '+(picked?picked.name:'預設模型')+'，可在右上角切換。';
        }
      }else if(STATE.is_admin){
        tip=(STATE.models&&STATE.models.length)
          ? '有模型但都不可用，請到「對話設定 → 使用者可選的模型」看每一項的原因。'
          : '尚未設定任何模型：先到左下角「API 密鑰」新增金鑰，並在「可用模型」欄位填入模型代號。';
      }else{
        tip='系統尚未設定可用的 AI 模型，請聯絡管理員。';
      }
      box.appendChild(el('p','',tip));
      var grid=el('div','sugg');
      ['幫我把這段中文潤飾得更專業','用簡單的比喻解釋什麼是 TCP 三次握手','幫我寫一封請假信給教授','把這段程式碼改寫得更好讀'].forEach(function(text){
        var b=el('button',null,text);
        b.type='button';
        b.addEventListener('click',function(){q('input').value=text;q('input').focus();autoGrow();});
        grid.appendChild(b);
      });
      box.appendChild(grid);
      inner.appendChild(box);
      return;
    }
    currentMessages.forEach(function(m){
      var via=m.role==='assistant'&&m.key_name?(m.key_name+(m.model?' · '+m.model:'')):'';
      inner.appendChild(messageNode(m.role,m.content,via,m.attachments));
    });
    scrollDown(true);
  }

  // ── 模型選單 ───────────────────────────────────────────
  function renderModels(){
    var pick=q('modelPick');
    var models=STATE.models||[];
    clear(pick);
    if(!models.length){
      var none=el('option',null,'尚無可用模型');
      none.value='';none.disabled=true;
      pick.appendChild(none);
      pick.disabled=true;
      return;
    }
    pick.disabled=false;
    // AI_CHAT_V3：「自動」永遠排第一，讓系統依問題性質挑。
    var auto=el('option',null,'⚡ 自動（依問題挑選）');
    auto.value='auto';
    if(currentModel==='auto'||!currentModel)auto.selected=true;
    pick.appendChild(auto);
    // 選中的模型若已被管理員移除／停用，就退回自動。
    var valid=currentModel==='auto'||models.some(function(m){return m.id===currentModel&&m.available&&m.valid!==false;});
    if(!valid)currentModel='auto';
    models.forEach(function(m){
      // 只列出管理員填過、且名稱有效的模型。名稱有問題的不該出現在
      // 使用者面前 —— 開機時會自動正規化，理論上也不會有。
      if(!m.enabled||m.valid===false)return;
      var suffix=m.available?'':'（'+(m.unavailable_reason||'無可用密鑰')+'）';
      var option=el('option',null,m.name+suffix);
      option.value=m.id;
      option.disabled=!m.available;
      if(m.id===currentModel)option.selected=true;
      pick.appendChild(option);
    });
  }

  // ── 對話串 ─────────────────────────────────────────────
  function renderThreadList(){
    var list=q('threadList');
    clear(list);
    if(!STATE.threads.length){
      var hint=el('div','desc','還沒有對話紀錄');
      hint.style.cssText='font-size:11.5px;color:var(--muted);padding:10px;text-align:center';
      list.appendChild(hint);
      return;
    }
    STATE.threads.forEach(function(t){
      var row=el('div','thread'+(t.id===currentThread?' active':''));
      row.appendChild(el('div','t',t.title||'新對話'));
      var x=el('button','x','×');
      x.type='button';
      x.title='刪除這個對話';
      x.addEventListener('click',function(ev){
        ev.stopPropagation();
        if(!window.confirm('刪除「'+(t.title||'新對話')+'」？'))return;
        postJson('/admin/api/ai/threads',{action:'delete',id:t.id},function(){
          if(t.id===currentThread)startNewThread();
          refreshState();
        });
      });
      row.appendChild(x);
      row.addEventListener('click',function(){openThread(t.id);});
      list.appendChild(row);
    });
  }

  function openThread(id){
    postJson('/admin/api/ai/threads',{action:'get',id:id},function(r){
      if(r.error){toast(r.error,true);return;}
      currentThread=id;
      if(r.thread&&r.thread.model_id)currentModel=r.thread.model_id;
      renderModels();
      currentMessages=(r.thread&&r.thread.messages)||[];
      q('title').textContent=(r.thread&&r.thread.title)||'對話';
      renderThread();
      renderThreadList();
      closeSide();
    });
  }

  function startNewThread(){
    currentThread='';
    currentMessages=[];
    q('title').textContent='新對話';
    renderThread();
    renderThreadList();
    closeSide();
    q('input').focus();
  }

  // ── 送出與輪詢 ─────────────────────────────────────────
  function setSending(on){
    var btn=q('sendBtn');
    if(on){btn.className='sendbtn stop';btn.textContent='■';btn.title='停止生成';}
    else{btn.className='sendbtn';btn.textContent='↑';btn.title='送出';}
  }

  function showError(node,payload){
    var box=el('div','errbox');
    box.appendChild(el('div','h','⚠ 生成失敗'));
    var msg=el('div','m',payload.error||'未知錯誤');
    if(payload.code){
      var code=el('div','li','錯誤碼：'+payload.code);
      msg.appendChild(document.createElement('br'));
      msg.appendChild(code);
    }
    (payload.attempts||[]).forEach(function(a){
      var line=el('div','li','· '+a.key+'：'+(a.ok?'成功':((a.kind||'失敗')+(a.status?(' HTTP '+a.status):'')+(a.reason?(' — '+a.reason):''))));
      msg.appendChild(line);
    });
    box.appendChild(msg);
    clear(node);
    node.appendChild(box);
  }

  function send(){
    var input=q('input');
    var text=input.value.trim();
    if(activeJob){stopGeneration();return;}
    // 只附檔案不打字也可以送，但檔案還在讀就得等一下。
    if(!text&&!pending.length)return;
    if(pending.some(function(a){return !a.ready;})){toast('檔案還在讀取，請稍候再送出');return;}

    var payload=pending.map(function(a){
      return a.kind==='image'
        ? {name:a.name,mime:a.mime,kind:'image',data:a.data}
        : {name:a.name,mime:a.mime||'text/plain',kind:'text',text:a.text};
    });
    var names=pending.map(function(a){return a.name;});
    pending=[];
    renderAttachBar();

    input.value='';
    autoGrow();

    var inner=q('inner');
    if(!currentMessages.length)clear(inner);
    var shown=text||(payload.some(function(a){return a.kind==='image';})?'（請看這張圖片）':'（請看附件）');
    currentMessages.push({role:'user',content:shown});
    inner.appendChild(messageNode('user',shown,'',names));

    var aiNode=messageNode('assistant','',''); 
    var bubble=aiNode._bubble;
    clear(bubble);
    var thinking=el('p');
    thinking.appendChild(el('span','cursor'));
    bubble.appendChild(thinking);
    inner.appendChild(aiNode);
    scrollDown(true);
    setSending(true);

    postJson('/admin/api/ai/chat',{thread_id:currentThread,message:text,model_id:currentModel,attachments:payload},function(r){
      if(r.error){
        setSending(false);
        showError(bubble,r);
        return;
      }
      currentThread=r.thread_id||currentThread;
      if(r.model_id){currentModel=r.model_id;renderModels();}
      activeJob=r.job_id;
      POLL_MS=r.poll_ms||350;
      startStream(bubble,aiNode._who);
    });
  }

  // ── AI_CHAT_V8：SSE 串流 ────────────────────────────────
  function closeStream(){
    if(activeStream){try{activeStream.close();}catch(e){}activeStream=null;}
    if(renderTimer){clearTimeout(renderTimer);renderTimer=null;}
  }

  // 每來一小段就重畫整棵 Markdown 太貴，節流到 90ms 畫一次。
  function scheduleRender(bubble,done){
    if(renderTimer)return;
    renderTimer=setTimeout(function(){
      renderTimer=null;
      renderMarkdown(bubble,streamBuf);
      if(!done)bubble.appendChild(el('span','cursor'));
      scrollDown(false);
    },90);
  }

  function stampWho(who,meta){
    if(!who||!meta)return;
    if(meta.key_name&&!who.querySelector('.via')){
      who.appendChild(el('span','via',(meta.model||'')+' · '+meta.key_name+(meta.cached?' · 快取':'')));
    }
  }
  function stampReason(bubble,reason){
    if(!reason||!bubble||!bubble.parentNode)return;
    if(bubble.parentNode.querySelector('.autonote'))return;
    var note=el('div','autonote','⚡ '+reason);
    bubble.parentNode.insertBefore(note,bubble);
  }

  function finishStream(bubble,who,r){
    closeStream();
    activeJob=null;
    setSending(false);
    if(r.ok){
      renderMarkdown(bubble,r.text||'（沒有內容）');
      currentMessages.push({role:'assistant',content:r.text,key_name:r.key_name,model:r.model});
      if(r.code==='AI_CANCELLED')toast('已停止生成');
      refreshState();
    }else{
      showError(bubble,r);
      refreshState();
    }
  }

  function startStream(bubble,who){
    streamBuf='';
    if(typeof EventSource!=='function'){poll(bubble,who,0);return;}
    var job=activeJob;
    var got=false, ended=false;
    var es;
    try{
      es=new EventSource('/admin/api/ai/chat/stream?job='+encodeURIComponent(job));
    }catch(e){poll(bubble,who,0);return;}
    activeStream=es;

    es.addEventListener('meta',function(ev){
      got=true;
      var meta=null;try{meta=JSON.parse(ev.data);}catch(e){}
      if(!meta)return;
      stampWho(who,meta);
      stampReason(bubble,meta.auto_reason);
    });
    es.addEventListener('delta',function(ev){
      got=true;
      streamBuf+=ev.data;
      scheduleRender(bubble,false);
    });
    es.addEventListener('done',function(ev){
      ended=true;
      var r=null;try{r=JSON.parse(ev.data);}catch(e){}
      if(!r){closeStream();poll(bubble,who,0);return;}
      stampWho(who,r);
      stampReason(bubble,r.auto_reason);
      finishStream(bubble,who,r);
    });
    // 伺服器說這個 job 不見了（過期／逾時）：交給輪詢去拿最終狀態或錯誤訊息。
    es.addEventListener('gone',function(){
      ended=true;closeStream();
      if(activeJob)poll(bubble,who,0);
    });
    es.onerror=function(){
      if(ended)return;
      // 一律關掉，不讓 EventSource 自動重連 —— 重連會從頭再推一次造成重複。
      closeStream();
      if(!activeJob)return;
      // 已經收到過內容就沿用；沒收到代表這條路不通（代理擋 SSE），退回輪詢。
      poll(bubble,who,0);
    };
  }

  function poll(bubble,who,ticks){
    if(!activeJob)return;
    getJson('/admin/api/ai/chat/status?job='+encodeURIComponent(activeJob),function(r){
      if(!activeJob)return;
      if(r.error&&!r.state){
        // 輪詢本身失敗（例如代理短暫抽風）：再試幾次才放棄。
        if(ticks<20){pollTimer=setTimeout(function(){poll(bubble,who,ticks+1);},POLL_MS*2);return;}
        activeJob=null;setSending(false);showError(bubble,r);return;
      }
      if(r.key_name&&who&&!who.querySelector('.via')){
        who.appendChild(el('span','via',(r.model||'')+' · '+r.key_name+(r.cached?' · 快取':'')));
      }
      if(r.auto_reason&&bubble&&bubble.parentNode&&!bubble.parentNode.querySelector('.autonote')){
        var note=el('div','autonote','⚡ '+r.auto_reason);
        bubble.parentNode.insertBefore(note,bubble);
      }
      if(r.text){
        // 輪詢拿的是全文，直接覆蓋（可能是從 SSE 半途接手過來的）。
        streamBuf=r.text;
        renderMarkdown(bubble,r.text);
        if(!r.done)bubble.appendChild(el('span','cursor'));
        scrollDown(false);
      }
      if(!r.done){
        pollTimer=setTimeout(function(){poll(bubble,who,ticks+1);},POLL_MS);
        return;
      }
      finishStream(bubble,who,r);
    });
  }

  function stopGeneration(){
    if(!activeJob)return;
    // 先讓伺服器停，SSE 會收到帶 AI_CANCELLED 的 done 事件再收尾。
    postJson('/admin/api/ai/chat/stop',{job:activeJob},function(){});
  }

  // ── 密鑰面板 ───────────────────────────────────────────
  function statusChip(key){
    if(!key.enabled)return el('span','chip off','已停用');
    if(key.status==='dead')return el('span','chip dead','密鑰失效');
    if(key.status==='cooling'){
      var left=key.cooldown_secs_left||0;
      var text=left>3600?(Math.round(left/3600)+' 小時'):(left>60?(Math.round(left/60)+' 分'):(left+' 秒'));
      return el('span','chip cool','冷卻中 · 剩 '+text);
    }
    return el('span','chip ok','可用');
  }

  function renderKeys(){
    var list=q('keyList');
    clear(list);
    q('keyEmpty').style.display=STATE.keys.length?'none':'block';
    STATE.keys.forEach(function(key,index){
      var card=el('div','keycard'+(key.enabled?'':' off'));
      var head=el('div','keyhead');
      head.appendChild(el('span','ord','#'+(index+1)));
      head.appendChild(el('span','nm',key.name));
      head.appendChild(statusChip(key));
      card.appendChild(head);

      var meta=el('div','keymeta');
      meta.appendChild(el('div',null,key.provider_label+' · '+key.effective_model));
      meta.appendChild(el('div',null,'密鑰 '+key.secret_masked+'　成功 '+key.uses+' 次　失敗 '+key.fails+' 次'));
      if(key.total_tokens)meta.appendChild(el('div',null,'已用 token：'+key.total_tokens.toLocaleString()+'（輸入 '+key.prompt_tokens+' / 輸出 '+key.completion_tokens+'）'));
      card.appendChild(meta);
      if(key.last_error)card.appendChild(el('div','keyerr',key.last_error));

      var acts=el('div','keyacts');
      function act(label,cls,handler){
        var b=el('button','btn sm'+(cls?' '+cls:''),label);
        b.type='button';
        b.addEventListener('click',handler);
        acts.appendChild(b);
      }
      act('測試',null,function(){
        toast('測試中…');
        postJson('/admin/api/ai/keys/test',{id:key.id},function(r){
          toast(r.ok?('✅ '+key.name+'：'+(r.message||'可用')):('❌ '+key.name+'：'+((r.kind?r.kind+' — ':'')+(r.error||'失敗'))),!r.ok);
          refreshState();
        });
      });
      act(key.enabled?'停用':'啟用',null,function(){
        postJson('/admin/api/ai/keys',{action:'toggle',id:key.id,enabled:!key.enabled},function(r){
          if(r.error){toast(r.error,true);return;}
          toast(r.message||'已更新');refreshState();
        });
      });
      if(key.status!=='ok'){
        act('重設狀態',null,function(){
          postJson('/admin/api/ai/keys',{action:'revive',id:key.id},function(r){
            if(r.error){toast(r.error,true);return;}
            toast(r.message||'已重設');refreshState();
          });
        });
      }
      act('↑',null,function(){move(key.id,-1);});
      act('↓',null,function(){move(key.id,1);});
      act('編輯',null,function(){beginEdit(key);});
      act('刪除','danger',function(){
        if(!window.confirm('刪除密鑰「'+key.name+'」？'))return;
        postJson('/admin/api/ai/keys',{action:'delete',id:key.id},function(r){
          if(r.error){toast(r.error,true);return;}
          toast('已刪除');refreshState();
        });
      });
      card.appendChild(acts);
      list.appendChild(card);
    });
  }

  function move(id,offset){
    postJson('/admin/api/ai/keys',{action:'move',id:id,offset:offset},function(r){
      if(r.error){toast(r.error,true);return;}
      refreshState();
    });
  }

  function providerPreset(id){
    for(var i=0;i<STATE.providers.length;i++){
      if(STATE.providers[i].id===id)return STATE.providers[i];
    }
    return null;
  }

  function syncProviderHints(){
    var preset=providerPreset(q('fProvider').value);
    // placeholder 只用服務商的官方預設當範例。拿使用者既有的資料當範例，
    // 看起來會很像「系統已經幫我填好了」，反而造成誤會。
    q('fModels').placeholder=(preset&&preset.model)||'模型代號，一行一個';
    q('fBase').placeholder=(preset&&preset.base_url)||'必填，例如 https://…/v1';
  }

  function beginEdit(key){
    editingKey=key.id;
    q('formTitle').textContent='編輯密鑰：'+key.name;
    q('fProvider').value=key.provider;
    q('fLabel').value=key.label;
    q('fSecret').value='';
    q('secretNote').textContent='留空＝不更動現有密鑰（目前 '+key.secret_masked+'）';
    // 這裡刻意「不」自動帶入既有模型：儲存只會新增不會刪除，
    // 自動帶入會讓使用者以為刪掉某行就等於移除該模型，結果下次又長回來。
    q('fModels').value='';
    q('modelsNote').textContent='留空＝不更動這把密鑰的模型。填入的名稱會「新增」到可選清單；要刪除請到下方「使用者可選的模型」。';
    q('fBase').value=key.base_url;
    q('saveKeyBtn').textContent='儲存變更';
    q('cancelEditBtn').style.display='inline-flex';
    syncProviderHints();
    q('keyPanel').querySelector('.pbody').scrollTop=q('keyPanel').querySelector('.pbody').scrollHeight;
  }

  function resetForm(){
    editingKey='';
    q('formTitle').textContent='新增密鑰';
    q('fLabel').value='';
    q('fSecret').value='';
    q('fModels').value='';
    q('fBase').value='';
    q('secretNote').textContent='';
    q('modelsNote').textContent='請填官方的模型代號。Groq／OpenRouter 要帶完整前綴（openai/gpt-oss-120b）；OpenAI／Gemini 不能帶前綴（gpt-4o-mini）。不要填「GPT-4o mini」這種顯示名稱。';
    q('saveKeyBtn').textContent='新增';
    q('cancelEditBtn').style.display='none';
    syncProviderHints();
  }

  function saveKey(){
    var lines=q('fModels').value.split('\n').map(function(t){return t.trim();})
                                .filter(function(t){return t;});
    var payload={
      action:editingKey?'update':'add',
      id:editingKey,
      provider:q('fProvider').value,
      label:q('fLabel').value,
      secret:q('fSecret').value,
      model:lines.length?lines[0]:'',
      models:lines,
      base_url:q('fBase').value
    };
    postJson('/admin/api/ai/keys',payload,function(r){
      if(r.error){toast(r.error,true);return;}
      toast(r.message||'已儲存');
      resetForm();
      refreshState();
    });
  }

  // ── 設定面板 ───────────────────────────────────────────
  function fillSettings(){
    var s=STATE.settings||{};
    q('sPrompt').value=s.system_prompt||'';
    q('sTemp').value=(s.temperature==null?0.7:s.temperature);
    q('sMax').value=s.max_tokens||0;
    q('sStrategy').value=s.strategy||'order';
    q('sBudget').value=s.context_budget||1500;
    q('sSummary').value=s.summary_enabled===false?'0':'1';
    q('sCache').value=s.cache_enabled===false?'0':'1';
  }

  function renderModelAdmin(){
    var list=q('mdlList');
    clear(list);
    q('mdlDesc').textContent=(STATE.models&&STATE.models.length
      ? '使用者只會看到這裡列出的模型。系統不會自己新增任何模型 —— 這份清單完全等於你填過的內容。'
      : '目前沒有任何模型。請在密鑰的「可用模型」欄位或下方「批次貼上模型名稱」填入你要開放的模型。')
      +'名稱會自動正規化成官方代號（「GPT-4o mini」→「gpt-4o-mini」），同一個模型只會留一筆。'
      +'「分級」與「擅長」會決定自動模式派哪個模型出場：快速＝短問題，強力＝長文與推理。';
    (STATE.models||[]).forEach(function(m,index){
      var card=el('div','mdlcard'+(m.enabled?'':' off'));
      var head=el('div','mdlhead');
      head.appendChild(el('span','ord','#'+(index+1)));
      head.appendChild(el('span','nm',m.name));
      head.appendChild(el('span','tier '+(m.tier||'balanced'),m.tier_label||'均衡'));
      (m.tags||[]).forEach(function(t){head.appendChild(el('span','tagchip',t));});
      if(m.valid===false){
        head.appendChild(el('span','chip dead','名稱不合法'));
      }else{
        head.appendChild(el('span','chip '+(m.available?'ok':'dead'),
          m.available?('可用 · '+m.usable_keys+' 把密鑰'):(m.unavailable_reason||'無可用密鑰')));
      }
      card.appendChild(head);
      card.appendChild(el('div','mdlmeta',m.provider_label+' · '+m.model));
      if(m.invalid_reason)card.appendChild(el('div','keyerr','⚠ '+m.invalid_reason+' —— 不會出現在使用者選單，按「整理並合併重複」可自動修正。'));
      if(!STATE.models_customized){list.appendChild(card);return;}
      var acts=el('div','keyacts');
      function act(label,cls,handler){
        var b=el('button','btn sm'+(cls?' '+cls:''),label);
        b.type='button';b.addEventListener('click',handler);acts.appendChild(b);
      }
      act(m.enabled?'隱藏':'開放',null,function(){modelAction({action:'toggle',id:m.id,enabled:!m.enabled});});
      act('改名',null,function(){
        var next=window.prompt('顯示給使用者的名稱（留空＝自動）',m.label||'');
        if(next==null)return;
        modelAction({action:'rename',id:m.id,label:next});
      });
      act('分級：'+(m.tier_label||'均衡'),null,function(){
        // fast → balanced → deep → fast，點一下換一級。
        var order=['fast','balanced','deep'];
        var next=order[(order.indexOf(m.tier||'balanced')+1)%3];
        modelAction({action:'set_tier',id:m.id,tier:next});
      });
      act('擅長',null,function(){
        var next=window.prompt('擅長領域，逗號分隔（可用：code, math, long, vision）',(m.tags||[]).join(','));
        if(next==null)return;
        modelAction({action:'set_tags',id:m.id,tags:next});
      });
      act('↑',null,function(){modelAction({action:'move',id:m.id,offset:-1});});
      act('↓',null,function(){modelAction({action:'move',id:m.id,offset:1});});
      act('移除','danger',function(){modelAction({action:'delete',id:m.id});});
      card.appendChild(acts);
      list.appendChild(card);
    });
  }

  function modelAction(payload){
    postJson('/admin/api/ai/models',payload,function(r){
      if(r.error){toast(r.error,true);return;}
      toast(r.message||'已更新');
      refreshState();
    });
  }

  function saveSettings(){
    postJson('/admin/api/ai/settings',{
      system_prompt:q('sPrompt').value,
      temperature:parseFloat(q('sTemp').value)||0,
      max_tokens:parseInt(q('sMax').value,10)||0,
      strategy:q('sStrategy').value
    },function(r){
      if(r.error){toast(r.error,true);return;}
      toast('已儲存設定');refreshState();
    });
  }

  // ── 狀態同步 ───────────────────────────────────────────
  function refreshState(){
    getJson('/admin/api/ai/state',function(r){
      if(r.error){toast(r.error,true);return;}
      STATE=r;
      renderThreadList();
      renderModels();
      renderKeys();
      renderModelAdmin();
      fillSettings();
      var dot=q('statusDot');
      dot.className='dot'+(r.usable_models>0?'':' off');
      if(r.page_version){
        var ver=q('verTag');
        ver.textContent=r.page_version+(r.build&&r.build!=='dev'?(' · '+String(r.build).slice(5,16)):'');
        ver.title='版面 '+r.page_version+'｜編譯於 '+(r.build||'未知');
      }
      var tag=q('keyTag');
      if(r.usable_keys>0){tag.textContent=r.usable_keys+'/'+r.keys.length+' 可用';tag.className='tag';}
      else{tag.textContent=r.keys.length?'全部不可用':'未設定';tag.className='tag warn';}
      // AI_CHAT_V2：密鑰與設定一律只有管理員看得到入口。
      q('keysBtn').style.display=r.is_admin?'flex':'none';
      q('setBtn').style.display=r.is_admin?'flex':'none';
      if(!currentMessages.length)renderThread();
    });
  }

  // ── 面板與側欄抽屜開關 ─────────────────────────────────
  // 手機側欄滑開後會蓋住右上角的 ☰，所以除了 ☰ 切換，還提供：
  // 1) 抽屜內 brand 列的 ✕（視覺明確）；2) 點擊遮罩（mask）關閉整體。
  function isSideOpen(){return q('side').className.indexOf('open')>=0;}
  function panelIsOpen(){return q('keyPanel').className.indexOf('open')>=0||q('setPanel').className.indexOf('open')>=0;}
  // 遮罩在「任一浮層」(側欄或密鑰/設定面板) 開啟時才出現；關閉任一就同步隱藏，
  // 避免點掉側欄後蓋住整個畫面的半透明黑幕還留著。
  function syncMask(){q('mask').className=(panelIsOpen()||isSideOpen())?'mask open':'mask';}
  function openPanel(id){q(id).className='panel open';syncMask();}
  function closePanels(){q('keyPanel').className='panel';q('setPanel').className='panel';syncMask();}
  function setSide(open){q('side').className=open?'side open':'side';syncMask();}
  function closeSide(){setSide(false);}
  function closeAll(){q('side').className='side';q('keyPanel').className='panel';q('setPanel').className='panel';syncMask();}

  // ── 輸入框 ─────────────────────────────────────────────
  function autoGrow(){
    var input=q('input');
    input.style.height='auto';
    input.style.height=Math.min(input.scrollHeight,200)+'px';
  }

  // ── 綁定 ───────────────────────────────────────────────
  q('sendBtn').addEventListener('click',send);
  // AI_CHAT_V7：三種附檔方式 —— 按鈕、拖進來、直接貼上。
  q('attachBtn').addEventListener('click',function(){q('fileInput').click();});
  q('fileInput').addEventListener('change',function(ev){
    addFiles(ev.target.files);
    ev.target.value='';
  });
  q('input').addEventListener('paste',function(ev){
    var items=(ev.clipboardData&&ev.clipboardData.items)||[];
    var picked=[];
    for(var i=0;i<items.length;i++){
      if(items[i].kind==='file'){
        var f=items[i].getAsFile();
        if(f)picked.push(f);
      }
    }
    if(picked.length){ev.preventDefault();addFiles(picked);}
  });
  ['dragenter','dragover'].forEach(function(name){
    document.addEventListener(name,function(ev){
      if(!ev.dataTransfer||Array.prototype.indexOf.call(ev.dataTransfer.types||[],'Files')<0)return;
      ev.preventDefault();
      q('stream').className='stream dropping';
    });
  });
  ['dragleave','drop'].forEach(function(name){
    document.addEventListener(name,function(ev){
      if(name==='dragleave'&&ev.relatedTarget)return;
      q('stream').className='stream';
    });
  });
  document.addEventListener('drop',function(ev){
    if(!ev.dataTransfer||!ev.dataTransfer.files||!ev.dataTransfer.files.length)return;
    ev.preventDefault();
    addFiles(ev.dataTransfer.files);
  });
  q('input').addEventListener('input',autoGrow);
  q('input').addEventListener('keydown',function(ev){
    if(ev.key==='Enter'&&!ev.shiftKey&&!ev.isComposing){ev.preventDefault();send();}
  });
  q('newBtn').addEventListener('click',startNewThread);
  q('menuBtn').addEventListener('click',function(){setSide(!isSideOpen());});
  q('sideClose').addEventListener('click',function(){setSide(false);});
  q('renameBtn').addEventListener('click',function(){
    if(!currentThread){toast('先開始一段對話');return;}
    var next=window.prompt('新的對話名稱',q('title').textContent);
    if(next==null)return;
    postJson('/admin/api/ai/threads',{action:'rename',id:currentThread,title:next},function(r){
      if(r.error){toast(r.error,true);return;}
      q('title').textContent=r.title||next;
      refreshState();
    });
  });
  q('keysBtn').addEventListener('click',function(){resetForm();openPanel('keyPanel');});
  q('setBtn').addEventListener('click',function(){fillSettings();openPanel('setPanel');});
  q('mask').addEventListener('click',closeAll);
  Array.prototype.forEach.call(document.querySelectorAll('[data-close]'),function(b){
    b.addEventListener('click',closePanels);
  });
  q('saveKeyBtn').addEventListener('click',saveKey);
  q('cancelEditBtn').addEventListener('click',resetForm);
  q('saveSetBtn').addEventListener('click',saveSettings);
  q('saveTokenBtn').addEventListener('click',function(){
    postJson('/admin/api/ai/settings',{
      context_budget:parseInt(q('sBudget').value,10)||1500,
      summary_enabled:q('sSummary').value==='1',
      cache_enabled:q('sCache').value==='1'
    },function(r){
      if(r.error){toast(r.error,true);return;}
      toast('已儲存');refreshState();
    });
  });
  q('clearAllBtn').addEventListener('click',function(){
    if(!window.confirm('刪除你的全部對話紀錄？此動作無法復原。'))return;
    postJson('/admin/api/ai/threads',{action:'clear'},function(){
      startNewThread();refreshState();toast('已清空');
    });
  });
  q('fProvider').addEventListener('change',syncProviderHints);
  q('modelPick').addEventListener('change',function(){
    currentModel=q('modelPick').value;
    var picked=null;
    (STATE.models||[]).forEach(function(m){if(m.id===currentModel)picked=m;});
    if(picked)toast('已切換到 '+picked.name);
  });
  q('mProvider').addEventListener('change',function(){
    var preset=providerPreset(q('mProvider').value);
    q('mModel').placeholder=(preset&&preset.model)||'必填';
  });
  q('addModelBtn').addEventListener('click',function(){
    modelAction({action:'add',provider:q('mProvider').value,model:q('mModel').value,label:q('mLabel').value});
    q('mModel').value='';q('mLabel').value='';
  });
  q('bulkAddBtn').addEventListener('click',function(){
    var lines=q('bModels').value.split('\n').map(function(t){return t.trim();}).filter(function(t){return t;});
    if(!lines.length){toast('請先貼上模型名稱',true);return;}
    postJson('/admin/api/ai/models',{action:'bulk_add',provider:q('bProvider').value,models:lines},function(r){
      if(r.error){toast(r.error,true);return;}
      toast(r.message||'已加入');
      q('bModels').value='';
      refreshState();
    });
  });
  q('cleanModelBtn').addEventListener('click',function(){modelAction({action:'clean'});});
  q('orphanModelBtn').addEventListener('click',function(){modelAction({action:'orphans'});});
  q('resetModelBtn').addEventListener('click',function(){
    if(!window.confirm('清空全部模型？清空後使用者將沒有任何模型可選，需要重新填入。'))return;
    modelAction({action:'reset'});
  });
  document.addEventListener('keydown',function(ev){
    if(ev.key==='Escape')closeAll();
  });

  // ── 啟動 ───────────────────────────────────────────────
  getJson('/admin/api/ai/state',function(r){
    if(r.error){toast(r.error,true);return;}
    STATE=r;
    [['fProvider',null],['mProvider',null],['bProvider',null]].forEach(function(pair){
      var select=q(pair[0]);
      clear(select);
      (r.providers||[]).forEach(function(p){
        var option=el('option',null,p.label);
        option.value=p.id;
        select.appendChild(option);
      });
    });
    syncProviderHints();
    var mPreset=providerPreset(q('mProvider').value);
    q('mModel').placeholder=(mPreset&&mPreset.model)||'必填';
    renderThreadList();
    renderModels();
    renderKeys();
    renderModelAdmin();
    fillSettings();
    renderThread();
    refreshState();
    q('input').focus();
  });
})();
