/* SHARE_V1: 免登入分享預覽頁的渲染邏輯（外掛以符合 CSP，不用 inline <script>）。
 * EXT / RAW 由伺服器以 <meta> 標籤注入，這裡讀取後在前端本機渲染。 */
(function () {
  var extMeta = document.getElementById('share-ext');
  var rawMeta = document.getElementById('share-raw');
  var EXT = (extMeta && extMeta.content) || '';
  var RAW = (rawMeta && rawMeta.content) || '';
  var stage = document.getElementById('stage');
  if (!stage) return;
  var IMAGE = ['jpg', 'jpeg', 'png', 'gif', 'bmp', 'webp', 'avif', 'ico', 'svg'];
  var VIDEO = ['mp4', 'webm', 'mov', 'm4v'];
  var AUDIO = ['mp3', 'wav', 'ogg', 'm4a', 'flac'];
  var OFFICE = ['docx', 'pptx', 'xlsx', 'odt', 'ods', 'odp', 'rtf'];
  var TEXT = ['txt', 'text', 'md', 'markdown', 'log', 'json', 'xml', 'yaml', 'yml',
    'ini', 'conf', 'cfg', 'toml', 'sql', 'csv', 'tsv', 'srt', 'vtt'];
  function fail(m) { stage.innerHTML = '<div class="msg">' + (m || '此格式無法線上預覽') + '</div>'; }
  if (IMAGE.indexOf(EXT) >= 0) stage.innerHTML = '<img src="' + RAW + '" alt="">';
  else if (VIDEO.indexOf(EXT) >= 0) stage.innerHTML = '<video src="' + RAW + '" controls autoplay playsinline></video>';
  else if (AUDIO.indexOf(EXT) >= 0) stage.innerHTML = '<audio src="' + RAW + '" controls autoplay></audio>';
  else if (EXT === 'pdf') stage.innerHTML = '<iframe src="' + RAW + '"></iframe>';
  else if (['doc', 'ppt', 'xls'].indexOf(EXT) >= 0) {
    // 舊版二進位 Office 無純 JS 解析器 → 交給 Microsoft Office Online 內嵌
    // （src 用同一簽章的公開 /pub/office URL，微軟伺服器可直接抓取）。
    var absolute = location.origin + RAW;
    stage.innerHTML = '<iframe src="https://view.officeapps.live.com/op/embed.aspx?src=' + encodeURIComponent(absolute) + '"></iframe>';
  }
  else if (TEXT.indexOf(EXT) >= 0) {
    fetch(RAW, { cache: 'no-store' }).then(function (r) { if (!r.ok) throw 0; return r.text(); })
      .then(function (t) { var p = document.createElement('pre'); p.textContent = t; stage.innerHTML = ''; stage.appendChild(p); })
      .catch(function () { fail('讀取失敗'); });
  }
  else if (OFFICE.indexOf(EXT) >= 0) {
    fetch(RAW, { cache: 'no-store' }).then(function (r) { if (!r.ok) throw new Error('fetch'); return r.arrayBuffer(); })
      .then(function (buf) { return window.TronOfficeView.preview(buf, EXT); })
      .then(function (html) { stage.innerHTML = html; if (window.TronOfficeView.mount) window.TronOfficeView.mount(stage, html); })
      .catch(function (e) { fail('預覽失敗：' + ((e && e.message) || e)); });
  }
  else fail();
})();
