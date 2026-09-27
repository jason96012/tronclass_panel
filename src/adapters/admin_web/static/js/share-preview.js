/* SHARE_V1: 免登入分享預覽頁的渲染邏輯（外掛以符合 CSP，不用 inline <script>）。
 * EXT / RAW 由伺服器以 <meta> 標籤注入，這裡讀取後在前端本機渲染。 */
(function () {
  var extMeta = document.getElementById('share-ext');
  var rawMeta = document.getElementById('share-raw');
  var EXT = String((extMeta && extMeta.content) || '').toLowerCase().replace(/[^a-z0-9]/g,'');
  var RAW = (rawMeta && rawMeta.content) || '';
  try {
    var ru = new URL(RAW, location.origin);
    if (ru.origin !== location.origin || ru.pathname.indexOf('/pub/') !== 0) RAW = '';
    else RAW = ru.pathname + ru.search;
  } catch (_) { RAW = ''; }
  var stage = document.getElementById('stage');
  if (!stage) return;
  var IMAGE = ['jpg', 'jpeg', 'png', 'gif', 'bmp', 'webp', 'avif', 'ico', 'svg'];
  var VIDEO = ['mp4', 'webm', 'mov', 'm4v'];
  var AUDIO = ['mp3', 'wav', 'ogg', 'm4a', 'flac'];
  var OFFICE = ['docx', 'pptx', 'xlsx', 'odt', 'ods', 'odp', 'rtf'];
  var TEXT = ['txt', 'text', 'md', 'markdown', 'log', 'json', 'xml', 'yaml', 'yml',
    'ini', 'conf', 'cfg', 'toml', 'sql', 'csv', 'tsv', 'srt', 'vtt'];
  function esc(s){ return String(s==null?'':s).replace(/[&<>"']/g, function(c){ return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]; }); }
  function fail(m) { report('fail', m); stage.innerHTML = '<div class="msg">' + esc(m || '此格式無法線上預覽') + '</div>'; }
  // SHARE_V1d: 把魔數/分支/錯誤回報後台（/pub/preview-log），一次性定位問題。
  function report(stg, msg) {
    try {
      fetch('/pub/preview-log', { method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: location.search, ext: EXT, stage: stg, message: String(msg || '').slice(0, 600) }) });
    } catch (_) {}
  }
  function hex(b) { var s = []; for (var i = 0; i < Math.min(b.length, 8); i++) s.push(b[i].toString(16)); return s.join(' '); }
  // PDFJS_PREVIEW_V1: 與雲端硬碟同款，PDF.js 逐頁渲染成 canvas（不靠瀏覽器外掛/Range）。
  function renderPdf(buf) {
    if (!window.pdfjsLib) { fail('PDF 引擎未載入'); return; }
    try { window.pdfjsLib.GlobalWorkerOptions.workerSrc = '/pub/static/js/pdf.worker.min.js?v=sharev5'; } catch (_) {}
    stage.innerHTML = '<div class="msg">PDF 渲染中…</div>';
    window.pdfjsLib.getDocument({ data: buf }).promise.then(function (pdf) {
      var wrap = document.createElement('div');
      wrap.style.cssText = 'display:flex;flex-direction:column;gap:12px;align-items:center';
      function go(n) {
        if (n > pdf.numPages) { stage.innerHTML = ''; stage.appendChild(wrap); return; }
        pdf.getPage(n).then(function (page) {
          var vp = page.getViewport({ scale: 1.6 });
          var canvas = document.createElement('canvas');
          canvas.width = vp.width; canvas.height = vp.height;
          canvas.style.cssText = 'max-width:100%;height:auto;background:#fff;border-radius:4px;box-shadow:0 2px 10px rgba(0,0,0,.3)';
          page.render({ canvasContext: canvas.getContext('2d'), viewport: vp }).promise.then(function () {
            wrap.appendChild(canvas); go(n + 1);
          });
        });
      }
      go(1);
    }).catch(function (e) { fail('PDF 解析失敗：' + ((e && e.message) || e)); });
  }
  if (!RAW) { fail('預覽位址不合法'); return; }
  report('load', 'ext=' + EXT);
  if (IMAGE.indexOf(EXT) >= 0) stage.innerHTML = '<img src="' + RAW + '" alt="">';
  else if (VIDEO.indexOf(EXT) >= 0) stage.innerHTML = '<video src="' + RAW + '" controls autoplay playsinline></video>';
  else if (AUDIO.indexOf(EXT) >= 0) stage.innerHTML = '<audio src="' + RAW + '" controls autoplay></audio>';
  else if (EXT === 'pdf') stage.innerHTML = '<iframe src="' + RAW + '"></iframe>';
  else if (TEXT.indexOf(EXT) >= 0) {
    fetch(RAW, { cache: 'no-store' }).then(function (r) { if (!r.ok) throw 0; return r.text(); })
      .then(function (t) { var p = document.createElement('pre'); p.textContent = t; stage.innerHTML = ''; stage.appendChild(p); })
      .catch(function () { fail('讀取失敗'); });
  }
  else if (OFFICE.concat(['doc', 'ppt', 'xls']).indexOf(EXT) >= 0) {
    // MAGIC_ROUTE_V1: 與雲端硬碟 renderOfficeSmart 完全相同的魔數分流——
    // 先抓位元組看魔數：%PDF→iframe；PK(zip)→office-view 本機解析；
    // OLE2(D0CF11E0)或非 zip→Microsoft Office Online（舊版/改名檔也能預覽）。
    fetch(RAW, { cache: 'no-store' }).then(function (r) { if (!r.ok) throw new Error('fetch'); return r.arrayBuffer(); })
      .then(function (buf) {
        var b = new Uint8Array(buf);
        report('magic', 'len=' + b.length + ' hex=' + hex(b));
        var isPdf = b.length > 3 && b[0] === 0x25 && b[1] === 0x50 && b[2] === 0x44 && b[3] === 0x46;
        var isZip = b.length > 1 && b[0] === 0x50 && b[1] === 0x4B;
        if (isPdf) { report('branch', 'pdf-js'); renderPdf(buf); return; }
        if (isZip && window.TronOfficeView) {
          report('branch', 'local-office-view');
          window.TronOfficeView.preview(buf, EXT)
            .then(function (html) {
              report('local-result', 'slides=' + ((html.match(/class="tron-slide"/g) || []).length) + ' err=' + html.includes('tron-office-error'));
              stage.innerHTML = html; if (window.TronOfficeView.mount) window.TronOfficeView.mount(stage, html);
            })
            .catch(function (e) { fail('預覽失敗：' + ((e && e.message) || e)); });
          return;
        }
        report('branch', 'office-online(OLE2/non-zip)');
        // OLE2 / 非 zip（如 .doc 改名 .docx）→ Office Online，src 用現簽 15 分鐘 /pub/office。
        var offMeta = document.getElementById('share-office');
        var src = (offMeta && offMeta.content) || RAW;
        stage.innerHTML = '<iframe src="https://view.officeapps.live.com/op/embed.aspx?src=' + encodeURIComponent(location.origin + src) + '"></iframe>';
      })
      .catch(function (e) { fail('讀取失敗：' + ((e && e.message) || e)); });
  }
  else fail();
})();
