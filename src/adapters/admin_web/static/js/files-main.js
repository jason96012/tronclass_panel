/* INLINE_SPLIT_V1：files-main.js 原本內嵌於 files.html 的 <body> 主腳本與其後所有內聯 <script>，抽成靜態檔（CSP script-src/style-src 'self'；
   由 asset_versions 統一改寫 ?sv=<build id>，內容變動即自動失效快取）。區塊順序與原 HTML 一致。 */

/* ── inline block: main (原 files.html:1077) ── */
(function(){
  'use strict';
  const LS_SESSION = 'tronclass_admin_session';
  let files = [], folders = [], currentFolder = '', isAdmin = false;
  let view = localStorage.getItem('drive_view') || 'grid';
  let sort = 'modified_desc';

  const q = id => document.getElementById(id);
  
  function token(){
    try {
      const u = '';
      if (u) return String(u).trim();
      const keys = ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];
      for (let i = 0; i < keys.length; i++) {
        const v = localStorage.getItem(keys[i]) || sessionStorage.getItem(keys[i]);
        if (v && String(v).trim()) return String(v).trim();
      }
    } catch(_) {}
    return '';
  }

  function withToken(path){
    try {
      const u = new URL(path, location.origin);
      const t = token();
      // F-03: token removed, auth via HttpOnly cookie
      return u.pathname + u.search + u.hash;
    } catch(_) { return path; }
  }

  function esc(v){
    return String(v == null ? '' : v).replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
  }

  function norm(p){
    return String(p || '').replace(/^\/+|\/+$/g, '').replace(/\/+/g, '/').trim();
  }

  function base(p){
    p = norm(p);
    return p ? p.split('/').pop() : '我的教材';
  }

  function parent(p){
    p = norm(p);
    if (!p || !p.includes('/')) return '';
    const a = p.split('/');
    a.pop();
    return a.join('/');
  }

  function toast(t, ok=true){
    const el = q('toast');
    if (!el) return;
    el.textContent = t;
    el.className = 'toast visible ' + (ok ? 'ok' : 'bad');
    clearTimeout(el._timer);
    el._timer = setTimeout(() => el.classList.remove('visible'), 3200);
  }

  function sizeLabel(n){
    n = Number(n || 0);
    if (n < 1024) return n + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
    if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + ' MB';
    return (n / 1024 / 1024 / 1024).toFixed(2) + ' GB';
  }

  function ext(f){
    return String(f.extension || (f.name || '').split('.').pop() || '').toLowerCase();
  }

  function dl(path){
    return '/admin/api/files/download?path=' + encodeURIComponent(path);
  }

  function pv(path){
    return '/admin/api/files/preview?path=' + encodeURIComponent(path);
  }

  // NO_LIBREOFFICE_PREVIEW_V1: 雲端硬碟可預覽的格式。
  // ⚠️ 這幾份清單必須與後端 files.rs 的 `PREVIEWABLE_EXTS` 完全一致。
  //    兩邊不同步時會出現「按了預覽卻收到 400」——舊版的 gif/bmp/webp 就是這樣壞的。
  // 全部走瀏覽器原生或前端本機解析，伺服器不呼叫 LibreOffice 等任何轉檔程式。
  const IMAGE_PREVIEW_EXTS  = ['jpg','jpeg','png','gif','bmp','webp','avif','ico','svg'];
  const VIDEO_PREVIEW_EXTS  = ['mp4','webm','mov','m4v'];
  const AUDIO_PREVIEW_EXTS  = ['mp3','wav','ogg','m4a','flac'];
  // 純文字類：抓回來後一律經 esc() 跳脫再顯示，絕不當成 HTML 執行。
  const TEXT_PREVIEW_EXTS   = ['txt','text','md','markdown','log','json','xml','yaml','yml',
                               'ini','conf','cfg','toml','sql','sh','rs','js','ts','jsx','tsx',
                               'css','py','java','c','h','cpp','hpp','go','php','html','htm',
                               'srt','vtt'];
  // 前端本機解析的 Office 格式（OOXML / ODF / RTF）
  const OFFICE_PREVIEW_EXTS = ['docx','pptx','xlsx','odt','ods','odp','rtf'];
  // 舊二進位：伺服器會回原始位元組，前端顯示「請下載查看」
  const LEGACY_OFFICE_EXTS  = ['doc','ppt','xls'];
  // OFFICE_ONLINE_V1: 這些格式可交給 Microsoft Office Online 檢視器（OneDrive 同款）。
  // 舊版 .doc/.ppt/.xls 本機無法解析，一律走線上；新版則本機優先、可切線上。
  const OFFICE_ONLINE_EXTS  = ['doc','ppt','xls','docx','pptx','xlsx'];

  function canPreview(e){
    return IMAGE_PREVIEW_EXTS.includes(e) || e === 'pdf' || e === 'url' ||
           VIDEO_PREVIEW_EXTS.includes(e) || AUDIO_PREVIEW_EXTS.includes(e) ||
           TEXT_PREVIEW_EXTS.includes(e) || e === 'csv' || e === 'tsv' ||
           OFFICE_PREVIEW_EXTS.includes(e) || LEGACY_OFFICE_EXTS.includes(e);
  }
  // URL_SHORTCUT_V1: 解析 Internet Shortcut（.url）內容，取出目標網址。
  // 支援 [InternetShortcut] 區的 URL= 行，或整份內容就是一個 http(s) 連結。
  function parseShortcutUrl(txt){
    txt = txt || '';
    let m = /(?:^|[\r\n])\s*URL\s*=\s*([^\s\r\n]+)/i.exec(txt);
    let u = m ? m[1].trim() : '';
    if (!u) {
      const t = txt.trim();
      if (/^https?:\/\//i.test(t)) u = t.split(/\s+/)[0];
    }
    if (!u) return null;
    if (!/^https?:\/\//i.test(u)) u = 'https://' + u;
    return u;
  }
  // URL_SHORTCUT_V1: .url 的「預覽」＝跳轉到該網站。先同步開新分頁（about:blank）
  // 以避免被浏览器的彈窗阻擋器擋下，fetch 完再把位址指過去；若新分頁開不成，
  // 退為同分頁跳轉。解析失敗才退回彈窗顯示原始內容。
  async function previewUrlFile(path, f){
    const win = window.open('about:blank', '_blank');
    let txt = '';
    try {
      const res = await fetch(pv(path), { cache: 'no-store' });
      txt = await res.text();
      const target = parseShortcutUrl(txt);
      if (!target) throw new Error('找不到 URL=');
      if (win) win.location.replace(target);
      else window.location.href = target;
      return;
    } catch (err) {
      if (win) win.close();
      reportPreview(path, 'url', 'url', 'shortcut jump failed: ' + (err && err.message));
      if (q('previewTitle')) q('previewTitle').textContent = f.name || path;
      if (q('previewDownload')) q('previewDownload').href = dl(path);
      setPreviewOpen(true);
      q('previewBody').innerHTML = '<div class="empty">此 .url 檔沒有可跳轉的網址，原始內容如下：</div><pre>' + esc(txt) + '</pre>';
    }
  }
  function isOfficePreview(e){ return OFFICE_PREVIEW_EXTS.includes(e) || LEGACY_OFFICE_EXTS.includes(e); }

  async function api(path, opts={}, timeout=12000){
    const t = token();
    const c = new AbortController();
    const timer = setTimeout(() => c.abort(), timeout);
    opts.signal = c.signal;
    opts.credentials = opts.credentials || 'same-origin';
    const sep = path.includes('?') ? '&' : '?';
    try {
      const res = await fetch(path, opts);
      if (res.status === 401 || res.status === 403) {
        localStorage.removeItem(LS_SESSION);
        sessionStorage.removeItem(LS_SESSION);
        const next = encodeURIComponent(location.pathname + location.search + location.hash);
        location.replace('/admin?next=' + next);
        throw new Error('登入失效，跳轉至核心登入頁');
      }
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error || ('HTTP ' + res.status));
      return body;
    } finally {
      clearTimeout(timer);
    }
  }

  // MATERIALS_IMPORT_V1 + SHARED: 從 iClass 自動匯入當學期教材(共用雲端硬碟)。
  // 後端改為背景 job：這端立即發起，再輪詢 /import/status 顯示進度，避免 502。
  async function importMaterials(){
    const btn = q('importHere');
    const old = btn ? btn.textContent : '';
    if (btn) { btn.disabled = true; btn.textContent = '匯入中…'; }
    try {
      const start = await api('/admin/api/files/import', {}, 60000);
      if (!start.ok){ alert(start.error || start.message || '匯入無法開始'); return; }
      if (start.accepted === false){ alert(start.error || start.message || '匯入無法開始'); return; }
      // 輪詢進度(最多約 10 分鐘)。
      const maxWait = 600000;
      const began = Date.now();
      let lastMsg = '';
      while (Date.now() - began < maxWait) {
        await new Promise(r => setTimeout(r, 2500));
        const st = await api('/admin/api/files/import/status', {}, 60000);
        if (st && st.message && st.message !== lastMsg) { lastMsg = st.message; if (btn) btn.textContent = st.message.slice(0, 24); }
        if (st && st.done) {
          const msg = '匯入完成：\n' +
            '課程 ' + (st.courses_scanned||0) + ' 門\n' +
            '教材 ' + (st.materials_found||0) + ' 項\n' +
            '已儲存 ' + (st.files_saved||0) + '、已跳過 ' + (st.files_skipped||0) +
            '、連結 ' + (st.links_created||0) + '、失敗 ' + (st.failed||0);
          if (Array.isArray(st.errors) && st.errors.length){
            msg += '\n\n錯誤：\n' + st.errors.slice(0,10).join('\n');
          }
          alert(msg);
          loadFiles();
          return;
        }
      }
      alert('匯入仍在背景執行中，請稍後回到此頁查看「全部教材」');
    } catch (e) {
      alert('匯入失敗：' + e.message);
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = old; }
    }
  }

  function folderSet(){
    const set = new Set();
    folders.forEach(f => {
      const n = norm(f);
      if (n) set.add(n);
    });
    files.forEach(f => {
      const p = norm(f.folder || parent(f.path || ''));
      if (p) set.add(p);
    });
    return Array.from(set).sort((a,b) => a.localeCompare(b, 'zh-Hant'));
  }

  function childFolders(){
    const all = folderSet();
    const prefix = currentFolder ? currentFolder + '/' : '';
    const out = new Set();
    all.forEach(f => {
      if (!currentFolder) {
        const first = f.split('/')[0];
        if (first) out.add(first);
      } else if (f.startsWith(prefix)) {
        const rest = f.slice(prefix.length);
        const first = rest.split('/')[0];
        if (first) out.add(prefix + first);
      }
    });
    const needle = (q('search') ? q('search').value : '').trim().toLowerCase();
    return Array.from(out).filter(f => f !== currentFolder && (!needle || f.toLowerCase().includes(needle))).sort((a,b) => base(a).localeCompare(base(b), 'zh-Hant'));
  }

  function directFiles(){
    const needle = (q('search') ? q('search').value : '').trim().toLowerCase();
    let list = files.filter(f => norm(f.folder || parent(f.path || '')) === currentFolder);
    if (needle) {
      list = files.filter(f => ((f.path||'') + ' ' + (f.name||'') + ' ' + (f.folder||'')).toLowerCase().includes(needle));
    }
    list.sort((a,b) => {
      if (sort === 'name_asc') return String(a.name || a.path || '').localeCompare(String(b.name || b.path || ''), 'zh-Hant');
      if (sort === 'size_desc') return Number(b.size || 0) - Number(a.size || 0);
      return String(b.modified || '').localeCompare(String(a.modified || ''));
    });
    return list;
  }

  function renderNav(){
    const all = folderSet();
    if (q('navAll')) q('navAll').classList.toggle('active', currentFolder === '');
    if (q('folderNav')) {
      q('folderNav').innerHTML = all.slice(0, 60).map(f => 
        '<button type="button" class="' + (f === currentFolder ? 'active' : '') + '" data-folder="' + esc(f) + '">📁 ' + esc(base(f)) + '</button>'
      ).join('') || '<div class="small" style="padding:6px 10px;color:var(--muted)">無子資料夾</div>';
    }
  }

  function renderBreadcrumb(){
    const parts = currentFolder ? currentFolder.split('/') : [];
    let acc = '', html = '<button type="button" data-folder="">我的教材</button>';
    parts.forEach(part => {
      acc = acc ? acc + '/' + part : part;
      html += '<span>/</span><button type="button" data-folder="' + esc(acc) + '">' + esc(part) + '</button>';
    });
    if (q('breadcrumb')) q('breadcrumb').innerHTML = html;
    if (q('pageTitle')) q('pageTitle').textContent = currentFolder ? base(currentFolder) : '我的教材';
  }

  function renderStorage(){
    const used = files.reduce((s, f) => s + Number(f.size || 0), 0);
    const quota = 10 * 1024 * 1024 * 1024;
    const p = Math.min(100, Math.round(used * 100 / quota));
    if (q('storageBar')) q('storageBar').style.width = p + '%';
    if (q('storageText')) q('storageText').textContent = sizeLabel(used) + ' / 10 GB 已使用';
  }

  function folderCard(f){
    return '<div class="folder-card" data-open-folder="' + esc(f) + '">' +
      '<div class="folder-icon"></div>' +
      '<div class="folder-name">' + esc(base(f)) + '</div>' +
      '<button class="button danger folder-del" data-delete-folder="' + esc(f) + '" title="刪除資料夾">🗑</button>' +
    '</div>';
  }

  function fileCard(f){
    const e = ext(f);
    const isImg = IMAGE_PREVIEW_EXTS.includes(e);
    // BADGE_INLINE_V1: 非圖片檔左側只放色塊縮圖，副檔名改以小標籤放在檔名欄內，
    // 不再用左側大格放徽章，徹底避免遮擋檔名。
    const thumb = isImg
      ? '<img class="file-thumb-img" src="' + pv(f.path) + '" alt="" loading="lazy">'
      : '<div class="file-thumb-ph"></div>';
    const chip = isImg ? '' : '<span class="file-chip">' + esc(e || 'FILE') + '</span>';
    return '<div class="file-card">' +
      '<div class="file-head">' +
        '<div class="file-thumb">' + thumb + '</div>' +
        '<div class="file-info">' + chip +
          '<div class="file-name">' + esc(f.name || f.path) + '</div>' +
          '<div class="file-meta">' + esc(f.modified || '-') + ' ｜ ' + esc(f.size_label || sizeLabel(f.size)) + '</div>' +
        '</div>' +
      '</div>' +
      '<div class="file-actions">' +
        '<a class="button secondary download-link" href="' + dl(f.path) + '">下載</a>' +
        (canPreview(e) ? '<button class="button secondary" data-preview="' + esc(f.path) + '">預覽</button>' : '') +
        '<button class="button secondary" data-share="' + esc(f.path) + '" title="分享">分享</button>' +
        '<button class="button danger remove-btn" data-delete-file="' + esc(f.path) + '" title="刪除檔案">🗑</button>' +
      '</div>' +
    '</div>';
  }

  function renderListTable(folds, fList){
    const tbody = q('listBody');
    if (!tbody) return;
    tbody.innerHTML = folds.map(f => 
      '<tr><td><button class="button secondary" style="min-height:30px;padding:0 8px" data-folder="' + esc(f) + '">📁 ' + esc(base(f)) + '</button></td><td>' + esc(parent(f) || '我的教材') + '</td><td>-</td><td>資料夾</td><td><button class="button danger" data-delete-folder="' + esc(f) + '" title="刪除資料夾">刪除</button></td></tr>'
    ).concat(fList.map(f => 
      '<tr><td><b>' + esc(f.name || f.path) + '</b></td><td>' + esc(f.folder || '我的教材') + '</td><td>' + esc(f.modified || '-') + '</td><td>' + esc(f.size_label || sizeLabel(f.size)) + '</td><td><a class="button secondary" href="' + dl(f.path) + '">下載</a>' + (canPreview(ext(f)) ? '<button class="button secondary" data-preview="' + esc(f.path) + '">預覽</button>' : '') + '<button class="button secondary" data-share="' + esc(f.path) + '" title="分享">分享</button><button class="button danger" data-delete-file="' + esc(f.path) + '" title="刪除檔案">刪除</button></td></tr>'
    )).join('') || '<tr><td colspan="5" class="empty">目前資料夾為空</td></tr>';
  }

  function render(){
    renderNav();
    renderBreadcrumb();
    renderStorage();

    const folds = childFolders();
    const fList = directFiles();

    if (q('summary')) q('summary').textContent = '資料夾 ' + folds.length + ' 個 ｜ 檔案 ' + fList.length + ' 個';

    if (q('folderSection')) q('folderSection').style.display = folds.length ? 'flex' : 'none';
    if (q('folderGrid')) q('folderGrid').innerHTML = folds.map(folderCard).join('');

    if (q('fileGrid')) {
      q('fileGrid').innerHTML = fList.length ? fList.map(fileCard).join('') : (!folds.length ? '<div class="empty">目前資料夾沒有教材檔案。請點擊「上傳檔案」或拖曳檔案至此。</div>' : '');
    }

    renderListTable(folds, fList);

    const isGrid = view === 'grid';
    if (q('fileGrid')) q('fileGrid').style.display = isGrid ? 'grid' : 'none';
    if (q('listTable')) q('listTable').classList.toggle('visible', !isGrid);
    if (q('gridView')) q('gridView').classList.toggle('active', isGrid);
    if (q('listView')) q('listView').classList.toggle('active', !isGrid);
  }

  async function loadFiles(){
    try {
      const body = await api('/admin/api/files/list', { cache: 'no-store' });
      isAdmin = !!body.is_admin;
      files = (body.files || []).map(f => typeof f === 'string' ? { path: f, name: f } : f);
      folders = body.folders || [];
      render();
    } catch(e) {
      toast(e.message || '讀取教材失敗', false);
      if (q('fileGrid')) q('fileGrid').innerHTML = '<div class="empty">讀取教材失敗：' + esc(e.message || e) + '</div>';
    }
  }

  function setFolder(f){
    currentFolder = norm(f);
    const u = new URL(location.href);
    if (currentFolder) u.searchParams.set('folder', currentFolder);
    else u.searchParams.delete('folder');
    history.replaceState(null, '', u.pathname + u.search + u.hash);
    render();
  }

  async function mkdir(){
    const name = prompt('請輸入新資料夾名稱：');
    if (!name) return;
    let path = norm(name);
    if (!path) return;
    if (currentFolder && !path.includes('/')) path = currentFolder + '/' + path;
    try {
      const b = await api('/admin/api/files/mkdir', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ path })
      });
      currentFolder = norm(b.path || path);
      toast('資料夾建立成功');
      await loadFiles();
    } catch(e) {
      toast(e.message || '建立資料夾失敗', false);
    }
  }

  function showProgress(p, t){
    if (q('uploadProgress')) q('uploadProgress').classList.add('visible');
    if (q('uploadProgressBar')) q('uploadProgressBar').style.width = Math.max(0, Math.min(100, p || 0)) + '%';
    if (q('uploadProgressText')) q('uploadProgressText').textContent = t || '';
  }

  function hideProgress(delay){
    setTimeout(() => {
      if (q('uploadProgress')) q('uploadProgress').classList.remove('visible');
      if (q('uploadProgressText')) q('uploadProgressText').style.color = '';
    }, delay || 1600);
  }
  function progressError(t){
    if (q('uploadProgress')) q('uploadProgress').classList.add('visible');
    if (q('uploadProgressText')) q('uploadProgressText').style.color = 'var(--bad,#b42318)';
    if (q('uploadProgressText')) q('uploadProgressText').textContent = t || '';
    if (q('uploadProgressBar')) q('uploadProgressBar').style.width = '0%';
  }

  function uploadOne(file, i, total, displayName){
    return new Promise((resolve, reject) => {
      const fd = new FormData();
      fd.append('folder', currentFolder);
      // UPLOAD_PNG_FIX_V2: 帶上（可能補了副檔名的）檔名，後端以此判斷格式
      fd.append('file', file, displayName || file.name);
      const xhr = new XMLHttpRequest();
      xhr.open('POST', '/admin/api/files/upload');
      xhr.withCredentials = true;
      xhr.upload.onprogress = ev => {
        if (ev.lengthComputable) {
          showProgress(ev.loaded / ev.total * 100, '上傳 ' + i + '/' + total + '：' + file.name + ' (' + Math.round(ev.loaded / ev.total * 100) + '%)');
        } else {
          showProgress(30, '上傳 ' + i + '/' + total + '：' + file.name);
        }
      };
      xhr.onerror = () => reject(new Error('網路連線中斷（檔案可能超過伺服器 50 MB 限制，或訊號不穩）'));
      xhr.onabort = () => reject(new Error('上傳已取消'));
      xhr.onload = () => {
        let body = {};
        try { body = JSON.parse(xhr.responseText || '{}'); } catch(_) {}
        if (xhr.status >= 200 && xhr.status < 300) resolve(body);
        else reject(new Error(body.error || ('HTTP ' + xhr.status)));
      };
      showProgress(0, '準備上傳 ' + i + '/' + total + '：' + file.name);
      xhr.send(fd);
    });
  }

  // UPLOAD_ROBUST_V1: 上傳流程強化（手機白畫面/沒上傳修復）
  // 1) 立即清空 fileInput——失敗後重選同一個檔案仍會觸發 change
  // 2) 用戶端預先檢查 50 MB 上限（與伺服器 MATERIALS_MAX_FILE_SIZE 一致），
  //    避免手機大檔上傳到一半被重置（看起來像白畫面/沒反應）
  // 3) 錯誤訊息停留在進度條 8 秒並標紅，不再一閃即逝
  const UPLOAD_MAX_BYTES = 50 * 1024 * 1024;
  // UPLOAD_TYPE_GUARD_V1: 允許格式清單（與後端 MATERIAL_ALLOWED_EXTENSIONS 一致）
  const UPLOAD_ALLOWED_EXTS = ['pdf','docx','pptx','xlsx','txt','md','jpg','jpeg','png','zip','mp4','heic','gif','mov'];
  function uploadExtOf(name){
    const m = String(name || '').toLowerCase().match(/\.([a-z0-9]+)$/);
    return m ? m[1] : '';
  }
  // UPLOAD_PNG_FIX_V2: iPhone 照片選擇器給的檔名常無副檔名（如 IMG_1234），
  // 改用 MIME 類型輔助判斷，避免 png/heic 等被誤判為格式不支援。
  const UPLOAD_MIME_EXT = {
    'image/png':'png','image/jpeg':'jpg','image/heic':'heic','image/gif':'gif',
    'application/pdf':'pdf','application/zip':'zip','application/x-zip-compressed':'zip',
    'video/mp4':'mp4','video/quicktime':'mov',
    'text/plain':'txt','text/markdown':'md',
    'application/vnd.openxmlformats-officedocument.wordprocessingml.document':'docx',
    'application/vnd.openxmlformats-officedocument.presentationml.presentation':'pptx',
    'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet':'xlsx'
  };
  function uploadMimeToExt(type){
    return UPLOAD_MIME_EXT[String(type || '').toLowerCase()] || '';
  }
  function uploadRealExt(f){
    return uploadExtOf(f.name) || uploadMimeToExt(f.type);
  }
  async function uploadFiles(fileList){
    const arr = Array.from(fileList || []);
    if (q('fileInput')) q('fileInput').value = '';
    if (!arr.length) return;
    // 格式預檢：不在白名單的檔案跳出提示通知，不上傳（無副檔名時以 MIME 判斷）
    const badType = arr.filter(f => UPLOAD_ALLOWED_EXTS.indexOf(uploadRealExt(f)) < 0);
    const okType = arr.filter(f => UPLOAD_ALLOWED_EXTS.indexOf(uploadRealExt(f)) >= 0);
    if (badType.length) {
      const names = badType.map(f => f.name || f.type || '未知檔案').join('、');
      alert('以下檔案格式不支援，未上傳：\n' + names + '\n\n允許的格式：' + UPLOAD_ALLOWED_EXTS.join('、'));
      progressError('未上傳（格式不支援）：' + names + '｜允許：' + UPLOAD_ALLOWED_EXTS.join('、'));
      toast('格式不支援，未上傳：' + names, false);
    }
    const tooBig = okType.filter(f => f.size > UPLOAD_MAX_BYTES);
    const will = okType.filter(f => f.size <= UPLOAD_MAX_BYTES);
    if (!will.length) {
      if (!badType.length && !tooBig.length) progressError('沒有可上傳的檔案');
      hideProgress(8000);
      return;
    }
    if (tooBig.length) {
      const names = tooBig.map(f => f.name).join('、');
      progressError('未上傳（超過 50 MB 限制）：' + names);
      toast('超過 50 MB 限制，未上傳：' + names, false);
    }
    try {
      for (let i = 0; i < will.length; i++) {
        // 無副檔名的檔案（iPhone 照片）：以 MIME 推得副檔名補在後端要看的檔名上
        const f = will[i];
        const realExt = uploadRealExt(f);
        const sendName = (uploadExtOf(f.name) || !realExt) ? f.name : (f.name + '.' + realExt);
        await uploadOne(f, i + 1, will.length, sendName);
      }
      if (q('uploadProgressText')) q('uploadProgressText').style.color = '';
      const skipped = badType.length + tooBig.length;
      showProgress(100, skipped ? '上傳完成（部分檔案未上傳，詳見提示）' : '所有檔案上傳完成！');
      toast(skipped ? '上傳完成（部分檔案未上傳）' : '教材上傳完成');
      hideProgress();
      await loadFiles();
    } catch(e) {
      progressError('上傳失敗：' + (e.message || e));
      toast('上傳失敗：' + (e.message || e), false);
      hideProgress(8000);
    }
  }

  async function delFile(path){
    if (!confirm('確定刪除檔案？\n' + path)) return;
    try {
      await api('/admin/api/files/delete', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ path })
      });
      toast('檔案已刪除');
      await loadFiles();
    } catch(e) {
      toast(e.message || '刪除檔案失敗', false);
    }
  }

  async function delFolder(path){
    if (!confirm('確定刪除資料夾及其所有內容？\n' + path)) return;
    try {
      await api('/admin/api/files/rmdir', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ path })
      });
      if (currentFolder === path || currentFolder.startsWith(path + '/')) currentFolder = '';
      toast('資料夾已刪除');
      await loadFiles();
    } catch(e) {
      toast(e.message || '刪除資料夾失敗', false);
    }
  }


  // PREVIEW_LOG_V1: 把前端預覽/解析錯誤回報到後台日誌（fire-and-forget），
  // 讓管理者在 Pterodactyl console 看到「哪個檔、哪一步、什麼錯」。
  function reportPreview(path, ext, stage, msg){
    try {
      fetch('/admin/api/files/preview-log', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: path, ext: ext, stage: stage, message: String(msg || '').slice(0, 2000) }),
        cache: 'no-store'
      }).catch(function(){});
    } catch(_) {}
  }

  // OFFICE_ONLINE_V1: OneDrive 同款。向後端索取「限時簽章公開連結」包成的
  // Office Online 嵌入 URL，用 <iframe> 顯示。微軟伺服器會自行抓取該連結。
  // 用於本機無法解析的舊版 .doc/.ppt/.xls。失敗時給明確訊息＋下載。
  async function renderOfficeOnline(path, ext){
    const body = q('previewBody');
    body.innerHTML = '<div class="empty">正在準備線上檢視…</div>';
    try {
      const res = await fetch('/admin/api/files/office-embed?path=' + encodeURIComponent(path), { cache: 'no-store' });
      if (!res.ok) throw new Error('http ' + res.status);
      const data = await res.json();
      if (!data.public_path) throw new Error('no public path');
      // OFFICE_ONLINE_HOST_V1: 絕對 URL 用瀏覽器實際 origin 組（後端猜的 host
      // 在 Pterodactyl/Tunnel 後是內部位址，微軟抓不到）。簽章不含 host，安全不變。
      const absolute = location.origin + data.public_path;
      const embed = 'https://view.officeapps.live.com/op/embed.aspx?src=' + encodeURIComponent(absolute);
      body.innerHTML =
        '<div class="tron-online-wrap">' +
          '<div class="tron-online-note">線上檢視由 Microsoft Office Online 提供（連結 15 分鐘內有效）。若持續空白，代表外網抓不到此連結，請改用上方「下載」。</div>' +
          '<iframe class="tron-online" src="' + esc(embed) + '" allowfullscreen></iframe>' +
        '</div>';
    } catch(err) {
      reportPreview(path, ext, 'office-online', err && err.message);
      body.innerHTML = '<div class="empty">無法啟動線上檢視（' + esc((err && err.message) || String(err)) + '），請按上方「下載」查看。</div>';
    }
  }

  // PDFJS_PREVIEW_V1: 用 PDF.js 把 PDF 逐頁渲染成 canvas（同 Firefox 內建檢視器核心）。
  // 失敗時退回原生 <iframe>，再失敗給明確訊息。
  // PDFJS_PREVIEW_V1: 把「已取得的 PDF 位元組」逐頁渲染成 canvas（fit-width 堆疊）。
  async function renderPdfData(data){
    const body = q('previewBody');
    // FILES_LAZY_LIB_V1: PDF.js 改為按需載入（只在真的開 PDF 時才抓 ~320KB）。
    if (window.__tronEnsureFileLib) await window.__tronEnsureFileLib('pdf');
    window.pdfjsLib.GlobalWorkerOptions.workerSrc = '/admin/static/js/pdf.worker.min.js?v=pdfjs1';
    const doc = await window.pdfjsLib.getDocument({ data, isEvalSupported: false }).promise;
    const wrap = document.createElement('div');
    wrap.className = 'tron-pdf';
    body.innerHTML = '';
    body.appendChild(wrap);
    const targetWidth = Math.max(320, (body.clientWidth || 800) - 28);
    for (let p = 1; p <= doc.numPages; p++) {
      const page = await doc.getPage(p);
      const base = page.getViewport({ scale: 1 });
      const scale = targetWidth / base.width;
      const vp = page.getViewport({ scale });
      const canvas = document.createElement('canvas');
      const ratio = window.devicePixelRatio || 1;
      canvas.width = Math.floor(vp.width * ratio);
      canvas.height = Math.floor(vp.height * ratio);
      canvas.style.width = Math.floor(vp.width) + 'px';
      canvas.style.height = 'auto';
      wrap.appendChild(canvas);
      await page.render({
        canvasContext: canvas.getContext('2d'),
        viewport: vp,
        transform: ratio !== 1 ? [ratio, 0, 0, ratio, 0, 0] : null
      }).promise;
    }
  }

  async function renderPdfPreview(url, path, ext){
    const body = q('previewBody');
    const fallbackIframe = () => {
      body.innerHTML = '<iframe src="' + url + '#toolbar=1" type="application/pdf"></iframe>';
    };
    // FILES_LAZY_LIB_V1: 先按需載入 PDF.js，否則下方的 `!window.pdfjsLib` 會誤判為
    // 「引擎缺失」而退回原生 iframe（首次開 PDF 就降級）。
    if (window.__tronEnsureFileLib) await window.__tronEnsureFileLib('pdf');
    if (!window.pdfjsLib) { reportPreview(path, ext, 'pdf-noengine', 'pdfjsLib missing'); fallbackIframe(); return; }
    body.innerHTML = '<div class="empty">正在載入 PDF…</div>';
    try {
      const res = await fetch(url, { cache: 'no-store' });
      if (!res.ok) throw new Error('http ' + res.status);
      await renderPdfData(new Uint8Array(await res.arrayBuffer()));
    } catch(err) {
      reportPreview(path, ext, 'pdf', err && err.message);
      fallbackIframe();
    }
  }

  // SINGLE_CONTAINER_V1: Office 預覽智能分流。
  //  回應是 PDF（未來若接轉換服務）→ PDF.js；舊版二進位→Office Online；
  //  現代格式→office-view.js 本機解析（單一 container、無 LibreOffice 下最穩）。
  // SINGLE_CONTAINER_V1: 本機解析（office-view.js）。單一 container、無 LibreOffice
  // 時，現代格式（docx/pptx/xlsx/odf/rtf）唯一可靠的預覽方式。
  async function renderOfficeLocal(buf, ext, path){
    const body = q('previewBody');
    const html = await window.TronOfficeView.preview(buf, ext);
    if (!html) throw new Error('empty');
    // 解析器回傳「錯誤卡片」時，把卡片文字＋zip 零件清單送進後台日誌，
    // 讓後台能直接看到這個 Office 檔的內部結構，一次定位問題。
    if (html.indexOf('tron-office-error') !== -1) {
      const tmp = document.createElement('div');
      tmp.innerHTML = html;
      let diag = '';
      try {
        const z = await JSZip.loadAsync(buf);
        const names = Object.keys(z.files).filter(n => !z.files[n].dir);
        diag = ' | parts(' + names.length + '): ' + names.slice(0, 15).join(', ');
        // 深入診斷：XML 是否被 DOMParser 接受、關鍵節點是否存在。
        const PNS = 'http://schemas.openxmlformats.org/presentationml/2006/main';
        // XML_ENCODING_V1 驗證：直接讀 bytes，回報十六進位開頭與 NUL 數量，
        // 確認 XML part 是否為 UTF-16（夾 0x00）導致 DOMParser "Document is empty"。
        const hex = (u) => Array.from(u.slice(0, 16)).map(b => b.toString(16).padStart(2, '0')).join(' ');
        const nulCount = (u) => { let c = 0; const n = Math.min(u.length, 256); for (let i = 1; i < n; i += 2) if (u[i] === 0) c++; return c; };
        const pbytes = z.file('ppt/presentation.xml') ? await z.file('ppt/presentation.xml').async('uint8array') : new Uint8Array(0);
        diag += ' | presHex[' + hex(pbytes) + '] presNul:' + nulCount(pbytes) + ' len:' + pbytes.length;
        const sfile = z.file('ppt/slides/slide1.xml');
        if (sfile) {
          const sbytes = await sfile.async('uint8array');
          diag += ' | slideHex[' + hex(sbytes) + '] slideNul:' + nulCount(sbytes) + ' len:' + sbytes.length;
        }
      } catch(e) { diag = ' | zip reload failed: ' + (e && e.message); }
      reportPreview(path, ext, 'office-parse', (tmp.textContent || '').trim() + diag);
    }
    if (window.TronOfficeView.mount) window.TronOfficeView.mount(body, html);
    else body.innerHTML = html;
  }

  // 預覽分流：PDF→PDF.js；舊版二進位→Office Online；現代→本機解析。
  async function renderOfficeSmart(url, path, ext){
    const body = q('previewBody');
    body.innerHTML = '<div class="empty">正在載入…</div>';
    try {
      const res = await fetch(url, { cache: 'no-store' });
      if (!res.ok) throw new Error('http ' + res.status);
      const buf = new Uint8Array(await res.arrayBuffer());
      const isPdf = buf.length > 4 && buf[0] === 0x25 && buf[1] === 0x50 && buf[2] === 0x44 && buf[3] === 0x46; // %PDF
      if (isPdf) { await renderPdfData(buf); return; }
      if (LEGACY_OFFICE_EXTS.includes(ext)) { await renderOfficeOnline(path, ext); return; }
      // FILES_LAZY_LIB_V1: 現代 Office 格式才按需載入 jszip + office-view（~145KB），
      // 其餘情況退回 Office Online，避免進頁就抓。
      if (window.__tronEnsureFileLib) await window.__tronEnsureFileLib('office');
      if (window.TronOfficeView && window.JSZip) { await renderOfficeLocal(buf, ext, path); return; }
      if (OFFICE_ONLINE_EXTS.includes(ext)) { await renderOfficeOnline(path, ext); return; }
      reportPreview(path, ext, 'unsupported', 'no engine for ext');
      body.innerHTML = '<div class="empty">此格式（.' + esc(ext) + '）不支援線上預覽，請按上方「下載」查看。</div>';
    } catch(err) {
      reportPreview(path, ext, 'office', err && err.message);
      body.innerHTML = '<div class="empty">無法預覽（' + esc((err && err.message) || String(err)) + '），請按上方「下載」。</div>';
    }
  }

  // CSV/TSV 轉成表格顯示。純前端 split，所有字串都經 esc() 跳脫。
  function csvToTable(text, sep){
    const lines = String(text).replace(/\r\n?/g, '\n').split('\n');
    if (lines.length && lines[lines.length - 1] === '') lines.pop();
    if (!lines.length) return '';
    const rows = lines.map(line => {
      const cells = []; let cur = '', quoted = false;
      for (let i = 0; i < line.length; i++){
        const ch = line[i];
        if (quoted){
          if (ch === '"' && line[i + 1] === '"'){ cur += '"'; i++; }
          else if (ch === '"'){ quoted = false; }
          else cur += ch;
        } else if (ch === '"') quoted = true;
        else if (ch === sep) { cells.push(cur); cur = ''; }
        else cur += ch;
      }
      cells.push(cur);
      return cells;
    });
    const MAX = 300;
    const head = rows[0];
    let html = '<table class="tron-csv-table"><thead><tr>';
    for (let c = 0; c < head.length; c++) html += '<th>' + esc(head[c]) + '</th>';
    html += '</tr></thead><tbody>';
    for (let r = 1; r < Math.min(rows.length, MAX); r++){
      html += '<tr>';
      for (let c = 0; c < head.length; c++) html += '<td>' + esc(rows[r][c] || '') + '</td>';
      html += '</tr>';
    }
    html += '</tbody></table>';
    if (rows.length > MAX) html += '<div class="tron-office-note">檔案過大，僅顯示前 ' + MAX + ' 列。完整內容請下載。</div>';
    return html;
  }

  // PREVIEW_HIDE_NAV_V1: 預覽彈窗開/關時同步切換 body class，隱藏手機導覽列。
  function setPreviewOpen(open){
    if (q('previewModal')) q('previewModal').classList.toggle('visible', open);
    document.body.classList.toggle('tron-preview-open', open);
  }

  // SHARE_V1: 分享按鈕＝向後端核發「免登入、可預覽」的公開連結（7 天時效），
  // 優先用 Web Share API 分享該連結（手機可傳 LINE/訊息），否則複製到剪貼簿。
  // 連結指向 /pub/share 預覽頁，不是下載頁；對方不用登入即可預覽。
  // QR_SHARE_V1: 按「分享」→ 彈窗顯示「分享連結 + QR code」，可複製或掃描。
  async function shareFile(path){
    let link = null;
    try {
      const res = await fetch('/admin/api/files/share-link?path=' + encodeURIComponent(path), { cache: 'no-store' });
      if (res.ok) { const j = await res.json(); if (j && j.public_path) link = location.origin + j.public_path; }
    } catch (_) {}
    if (!link) { toast('無法產生分享連結'); return; }
    await openShareModal(link);
  }

  // TRUSTED_TYPES_V1: 後台 CSP require-trusted-types-for 'script'，innerHTML 需經
  // 允許清單內的 'default' policy，否則拋錯。
  var __TT = null;
  try { if (window.trustedTypes) __TT = trustedTypes.createPolicy('default', { createHTML: function(s){return s;} }); } catch (e) { __TT = null; }
  function setHTMLSafe(el, html) { if (__TT) el.innerHTML = __TT.createHTML(html); else el.innerHTML = html; }

  async function openShareModal(link){
    const old = document.getElementById('share-modal'); if (old) old.remove();
    const ov = document.createElement('div'); ov.id = 'share-modal';
    ov.style.cssText = 'position:fixed;inset:0;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;z-index:99999;padding:16px;';
    const box = document.createElement('div');
    box.style.cssText = 'background:#fff;color:#111;border-radius:14px;max-width:420px;width:100%;padding:20px;box-shadow:0 10px 40px rgba(0,0,0,.35);text-align:center;';
    setHTMLSafe(box,
      '<div style="font-size:16px;font-weight:600;margin-bottom:6px;">分享連結</div>'
      + '<div id="share-qr" style="display:flex;justify-content:center;margin:10px 0;"></div>'
      + '<div style="font-size:11px;color:#888;margin-bottom:6px;">免登入可預覽 · 7 天有效</div>'
      + '<div id="share-link" style="word-break:break-all;font-size:12px;color:#555;max-height:84px;overflow:auto;background:#f5f5f5;border-radius:8px;padding:8px;text-align:left;"></div>'
      + '<div style="display:flex;gap:8px;justify-content:center;margin-top:14px;">'
      + '<button id="share-copy" type="button" style="padding:8px 16px;border:none;border-radius:8px;background:#2563eb;color:#fff;font-size:13px;cursor:pointer;">複製連結</button>'
      + '<button id="share-close" type="button" style="padding:8px 16px;border:1px solid #ccc;border-radius:8px;background:#fff;color:#333;font-size:13px;cursor:pointer;">關閉</button>'
      + '</div>');
    ov.appendChild(box);
    document.body.appendChild(ov);
    document.getElementById('share-link').textContent = link;
    const qrBox = document.getElementById('share-qr');
    try {
      // FILES_LAZY_LIB_V1: QR 產生器改為按需載入（~20KB），只有按「分享」才抓。
      if (window.__tronEnsureFileLib) await window.__tronEnsureFileLib('qrcode');
      if (typeof qrcode === 'function') {
        const qr = qrcode(0, 'M'); qr.addData(link); qr.make();
        setHTMLSafe(qrBox, qr.createSvgTag({ cellSize: 5, margin: 2, scalable: true }));
        const svg = qrBox.querySelector('svg'); if (svg) { svg.style.width = '220px'; svg.style.height = '220px'; }
      } else { qrBox.textContent = '（QR 需 qrcode.min.js）'; }
    } catch (_) { qrBox.textContent = 'QR 產生失敗'; }
    document.getElementById('share-copy').addEventListener('click', async () => {
      try { await navigator.clipboard.writeText(link); toast('已複製分享連結'); }
      catch (_) { window.prompt('複製分享連結：', link); }
    });
    document.getElementById('share-close').addEventListener('click', () => ov.remove());
    ov.addEventListener('click', (e) => { if (e.target === ov) ov.remove(); });
  }

  async function preview(path){
    const f = files.find(x => x.path === path) || { path, name: path };
    const e = ext(f);
    const url = pv(path);
    // URL_SHORTCUT_V1: .url 不開彈窗，直接跳轉到檔案內的網址。
    if (e === 'url') { await previewUrlFile(path, f); return; }
    if (q('previewTitle')) q('previewTitle').textContent = f.name || path;
    if (q('previewDownload')) q('previewDownload').href = dl(path);
    setPreviewOpen(true);

    if (IMAGE_PREVIEW_EXTS.includes(e)) {
      // SVG 只透過 <img> 載入：img 上下文不會執行 script，即使檔案內含腳本也無效。
      q('previewBody').innerHTML = '<img src="' + url + '">';
    } else if (e === 'pdf') {
      // PDFJS_PREVIEW_V1: 用 PDF.js（Firefox 同款）本機渲染，canvas 逐頁畫出，
      // 不靠瀏覽器原生 PDF 外掛 → 所有瀏覽器/手機一致，解決 iframe 空白。
      // 若 PDF.js 未載入或解析失敗，退回原生 <iframe>。
      await renderPdfPreview(url, path, e);
    } else if (isOfficePreview(e)) {
      // SINGLE_CONTAINER_V1: PDF→PDF.js；舊版→Office Online；現代→本機解析。
      await renderOfficeSmart(url, path, e);
    } else if (VIDEO_PREVIEW_EXTS.includes(e)) {
      q('previewBody').innerHTML = '<video src="' + url + '" controls autoplay></video>';
    } else if (AUDIO_PREVIEW_EXTS.includes(e)) {
      q('previewBody').innerHTML = '<audio src="' + url + '" controls autoplay></audio>';
    } else if (e === 'csv' || e === 'tsv') {
      try {
        const res = await fetch(url, { cache: 'no-store' });
        const table = csvToTable(await res.text(), e === 'tsv' ? '\t' : ',');
        q('previewBody').innerHTML = table || '<div class="empty">此檔案沒有內容</div>';
      } catch(_) {
        q('previewBody').innerHTML = '<div class="empty">無法預覽此檔案</div>';
      }
    } else if (TEXT_PREVIEW_EXTS.includes(e)) {
      try {
        const res = await fetch(url, { cache: 'no-store' });
        // 一律以純文字顯示：html/xml 等格式也只看原始碼，不會被執行。
        q('previewBody').innerHTML = '<pre>' + esc(await res.text()) + '</pre>';
      } catch(_) {
        q('previewBody').innerHTML = '<div class="empty">無法預覽此檔案</div>';
      }
    } else {
      // 理論上不會走到（canPreview 已擋掉），但留個明確訊息而不是空白彈窗。
      q('previewBody').innerHTML = '<div class="empty">此檔案格式不支援預覽，請按上方「下載」查看。</div>';
    }
  }

  function syncLinks(){
    if (q('dashboardLink')) q('dashboardLink').href = withToken('/admin');
    if (q('contentHeadBackDashboard')) q('contentHeadBackDashboard').href = withToken('/admin');
    if (q('sidebarBackDashboard')) q('sidebarBackDashboard').href = withToken('/admin');
    if (q('mDashboard')) q('mDashboard').href = withToken('/admin');
  }

  function driveHome(){
    // MOBILE_FILES_BACK_BUTTON_V1: 手機版改為「回上一層」資料夾(而非硬碟首頁)。
    const parts = (currentFolder || '').split('/').filter(Boolean);
    parts.pop();
    setFolder(parts.join('/'));
  }

  function init(){
    syncLinks();
    
    if (q('newBtn')) q('newBtn').onclick = () => q('newMenu').classList.toggle('open');
    if (q('newUpload')) q('newUpload').onclick = () => { q('newMenu').classList.remove('open'); q('fileInput').click(); };
    if (q('newFolder')) q('newFolder').onclick = () => { q('newMenu').classList.remove('open'); mkdir(); };
    if (q('uploadHere')) q('uploadHere').onclick = () => q('fileInput').click();
    if (q('importHere')) q('importHere').onclick = importMaterials;
    if (q('mImportTop')) q('mImportTop').onclick = importMaterials; // MOBILE_IMPORT_TOP_V1
    if (q('mkdirHere')) q('mkdirHere').onclick = mkdir;
    if (q('refresh')) q('refresh').onclick = loadFiles;
    // SHARE_ADMIN_V1（手機）：管理員→「連結管理」跳 /admin/shares；
    // 一般使用者→「重新整理」重載清單。
    if (q('mRefresh')) {
      var setRefresh = function(){
        setHTMLSafe(q('mRefresh'), '<svg viewBox="0 0 24 24"><path d="M20 12a8 8 0 0 1-14.6 4.6"></path><path d="M4 12A8 8 0 0 1 18.6 7.4"></path><path d="M18 3v5h-5"></path><path d="M6 21v-5h5"></path></svg><span>重新整理</span>');
        q('mRefresh').onclick = loadFiles;
      };
      var setShares = function(){
        setHTMLSafe(q('mRefresh'), '<svg viewBox="0 0 24 24"><path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"></path><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"></path></svg><span>連結管理</span>');
        q('mRefresh').onclick = function(){ location.href = withToken('/admin/shares'); };
      };
      setRefresh();
      fetch('/admin/api/shares', { credentials:'same-origin', cache:'no-store' })
        .then(function(r){ if(r.ok){ setShares(); } })
        .catch(function(){});
    }
    // MOBILE_FILES_BACK_BUTTON_V2: 頂欄「回上一頁」→ 上一層資料夾；底部「返回控制台」→ 控制台
    if (q('filesBackDashboardBtn')) q('filesBackDashboardBtn').onclick = driveHome;
    if (q('mDriveHome')) q('mDriveHome').onclick = () => location.href = withToken('/admin');
    if (q('mUpload')) q('mUpload').onclick = () => q('fileInput').click();
    if (q('mFolder')) q('mFolder').onclick = mkdir;

    if (q('navAll')) q('navAll').onclick = () => setFolder('');
    if (q('gridView')) q('gridView').onclick = () => { view = 'grid'; localStorage.setItem('drive_view', view); render(); };
    if (q('listView')) q('listView').onclick = () => { view = 'list'; localStorage.setItem('drive_view', view); render(); };
    if (q('sortBy')) q('sortBy').onchange = e => { sort = e.target.value; render(); };
    var _filesDebounce = null;
    if (q('search')) q('search').oninput = () => {
      clearTimeout(_filesDebounce);
      _filesDebounce = setTimeout(render, 120);
    };
    if (q('fileInput')) q('fileInput').onchange = e => uploadFiles(e.target.files);

    if (q('previewClose')) q('previewClose').onclick = () => setPreviewOpen(false);
    if (q('previewModal')) q('previewModal').onclick = e => { if (e.target === q('previewModal')) setPreviewOpen(false); };

    document.body.addEventListener('click', e => {
      const open = e.target.closest('[data-open-folder]');
      if (open && !e.target.closest('button,a')) return setFolder(open.dataset.openFolder);
      const btn = e.target.closest('[data-folder],[data-delete-file],[data-delete-folder],[data-preview],[data-share]');
      if (!btn) return;
      if (btn.dataset.folder !== undefined) return setFolder(btn.dataset.folder);
      if (btn.dataset.deleteFile) return delFile(btn.dataset.deleteFile);
      if (btn.dataset.deleteFolder) return delFolder(btn.dataset.deleteFolder);
      if (btn.dataset.preview) return preview(btn.dataset.preview);
      if (btn.dataset.share) return shareFile(btn.dataset.share);
    });

    const drop = q('dropzone');
    if (drop) {
      ['dragenter','dragover'].forEach(n => document.addEventListener(n, e => { e.preventDefault(); drop.classList.add('dragover'); }));
      document.addEventListener('dragleave', e => { if (e.clientX === 0 && e.clientY === 0) drop.classList.remove('dragover'); });
      document.addEventListener('drop', e => {
        e.preventDefault();
        drop.classList.remove('dragover');
        if (e.dataTransfer && e.dataTransfer.files.length) uploadFiles(e.dataTransfer.files);
      });
    }

    const folderParam = new URLSearchParams(location.search).get('folder');
    if (folderParam) currentFolder = norm(folderParam);

    loadFiles();
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();

/* ── inline block: RETURN_DASHBOARD_HREF_V1 (原 files.html:2023) ── */
(function(){
  function tok(){
    return '';
  }
  function patch(){
    var t=tok();
    document.querySelectorAll('a[href="/admin"],a[href="/admin/"],a[href="/admin#overview"],a[href^="/admin?"]').forEach(function(a){
      try{
        var u=new URL(a.getAttribute('href'), location.origin);
        if(!(u.pathname==='/admin'||u.pathname==='/admin/'||u.pathname==='/admin/dashboard')) return;
        // F-03: token removed, auth via HttpOnly cookie
        if(!u.hash) u.hash='overview';
        a.setAttribute('href', u.pathname+u.search+u.hash);
      }catch(_){}
    });
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', patch);
  else patch();
  setTimeout(patch, 200);
})();

/* ── inline block: FILES_MOBILE_HEADER_BACK_V1 (原 files.html:2046) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="FILES_MOBILE_HEADER_BACK_V1">，瀏覽器會把有 id 的元素暴露成
     window.FILES_MOBILE_HEADER_BACK_V1（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.FILES_MOBILE_HEADER_BACK_V1) return;
  window.FILES_MOBILE_HEADER_BACK_V1 = true;
  function tok(){
    return '';
  }
  function href(){
    var t=tok();
    return '/admin#overview';
  }
  function tidy(){
    var top=document.querySelector('.top-actions');
    if(top){
      Array.prototype.slice.call(top.children).forEach(function(n){
        if(n.id==='dashboardLink') return;
        n.style.display='none';
      });
      var a=document.getElementById('dashboardLink');
      if(!a){
        a=document.createElement('a');
        a.id='dashboardLink';
        a.className='button primary';
        top.appendChild(a);
      }
      if(a.textContent!=='回控制台') a.textContent='回控制台'; /* INLINE_SPLIT_DEAD_BLOCK_V1：冪等寫入 */
      a.href=href();
      a.style.display='inline-flex';
    }
    var dash=document.getElementById('mDashboard');
    if(dash){
      dash.style.display='none';
      dash.setAttribute('hidden','hidden');
    }
  }
  if(document.readyState==='loading') document.addEventListener('DOMContentLoaded', tidy);
  else tidy();
  setTimeout(tidy, 80);
  setTimeout(tidy, 400);
  try{ new MutationObserver(tidy).observe(document.documentElement,{childList:true,subtree:true}); }catch(_){}
})();

/* ── inline block: FILES_TOP_BACK_V2 (原 files.html:2089) ── */
(function(){
  /* INLINE_SPLIT_DEAD_BLOCK_V1：這段原本是 <script id="FILES_TOP_BACK_V2">，瀏覽器會把有 id 的元素暴露成
     window.FILES_TOP_BACK_V2（truthy），所以下一行的守衛在正式環境「永遠」提前 return，這段程式從未執行過。
     INLINE_SPLIT_V1 拆成外部檔後 id 消失、守衛失效，整段被意外復活；其中的 MutationObserver 與
     無條件 textContent 寫入會互相觸發，形成同步無限迴圈，把主執行緒卡死（手機版總覽永遠顯示「—」）。
     為維持與拆分前完全相同的行為，這裡明確停用，不再依賴 id 陰影。 */
  if(true) return;
  if(window.FILES_TOP_BACK_V2) return;
  window.FILES_TOP_BACK_V2 = true;
  function tok(){
    try{
      return ''; // F-03: token removed
    }catch(_){ return ''; }
  }
  function ensure(){
    var header = document.querySelector('header.topbar, header, .topbar');
    if(!header) return;
    var btn = document.getElementById('filesBackDashboardBtn');
    if(!btn){
      btn = document.createElement('button');
      btn.id = 'filesBackDashboardBtn';
      btn.type = 'button';
      header.appendChild(btn);
    }else if(btn.parentElement !== header){
      header.appendChild(btn);
    }
    // MOBILE_FILES_BACK_BUTTON_V2: 頂欄這顆改為「回上一頁」資料夾(非回控制台)。
    btn.className = 'button primary';
    btn.setAttribute('type','button');
    btn.title = '回上一層資料夾';
    btn.innerHTML = '<svg class="btn-ico" viewBox="0 0 24 24"><path d="M12 19V5"></path><path d="m5 12 7-7 7 7"></path></svg>回上一頁';
    btn.removeAttribute('href');
    btn.removeAttribute('hidden');
    btn.style.display = '';
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', ensure);
  else ensure();
  setTimeout(ensure, 60);
  setTimeout(ensure, 300);
})();

/* ── inline block: filesParticlesScriptV1 (原 files.html:2205) ── */
/* MATERIALS_PARTICLES_V1: 複製 dashboard 的粒子光點動畫 */
(function(){
  'use strict';
  if(window.FILES_PARTICLES_V1) return;
  window.FILES_PARTICLES_V1 = true;
  var raf = 0, particles = [];
  function q(id){ return document.getElementById(id); }
  function darkMode(){
    return document.documentElement.getAttribute('data-theme')==='dark' ||
      document.documentElement.classList.contains('dark-mode') ||
      (window.matchMedia && matchMedia('(prefers-color-scheme:dark)').matches);
  }
  function resize(c){
    var dpr = Math.min(window.devicePixelRatio||1, 2);
    c.width = Math.floor(window.innerWidth*dpr);
    c.height = Math.floor(window.innerHeight*dpr);
    c.style.width = window.innerWidth+'px';
    c.style.height = window.innerHeight+'px';
    var ctx = c.getContext('2d'); if(ctx) ctx.setTransform(dpr,0,0,dpr,0,0);
  }
  function init(c){
    particles = [];
    var count = Math.min(90, Math.floor((window.innerWidth*window.innerHeight)/16000));
    for(var i=0;i<count;i++){
      particles.push({
        x: Math.random()*window.innerWidth,
        y: Math.random()*window.innerHeight,
        s: Math.random()*2.2+.8,
        vx: (Math.random()-.5)*.36,
        vy: (Math.random()-.5)*.36,
        a: Math.random()*.18+.05
      });
    }
  }
  function loop(){
    var c = q('filesBgParticlesV1');
    if(!c){ raf = 0; return; }
    var ctx = c.getContext('2d');
    ctx.clearRect(0,0,window.innerWidth,window.innerHeight);
    var fill = darkMode() ? 'rgba(226,239,255,' : 'rgba(37,99,235,';
    for(var i=0;i<particles.length;i++){
      var p = particles[i];
      p.x += p.vx; p.y += p.vy;
      if(p.x<0) p.x=window.innerWidth; if(p.x>window.innerWidth) p.x=0;
      if(p.y<0) p.y=window.innerHeight; if(p.y>window.innerHeight) p.y=0;
      ctx.fillStyle = fill + p.a + ')';
      ctx.beginPath(); ctx.arc(p.x,p.y,p.s,0,Math.PI*2); ctx.fill();
    }
    raf = requestAnimationFrame(loop);
  }
  function start(){
    if(window.matchMedia && matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    var c = q('filesBgParticlesV1');
    if(!c){
      c = document.createElement('canvas');
      c.id = 'filesBgParticlesV1';
      c.setAttribute('aria-hidden','true');
      document.body.prepend(c);
    }
    var ctx = c.getContext('2d'); if(!ctx || raf) return;
    resize(c); init(c);
    raf = requestAnimationFrame(loop);
    window.addEventListener('resize', function(){ if(c){ resize(c); init(c); } }, {passive:true});
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();

/* ── inline block: manageSharesInit (原 files.html:2276) ── */
(function(){
  function init(){
    var btn = document.getElementById('manageShares'); if(!btn) return;
    btn.addEventListener('click', function(){ location.href = '/admin/shares'; });
    fetch('/admin/api/shares', { credentials:'same-origin', cache:'no-store' })
      .then(function(r){ if(r.ok){ btn.style.display = ''; } })
      .catch(function(){});
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();

/* ── inline block: mobileNavAutoHideV1 (原 files.html:2291) ── */
(function(){
  function setup(){
    var bar = document.getElementById('mobilebar'); if(!bar) return;
    bar.style.transition = 'transform .25s ease';
    var last = 0;
    function apply(y){
      var down = y > last + 4;
      var up = y < last - 4;
      if(down && y > 80){ bar.style.transform = 'translateY(calc(100% + 24px))'; }
      else if(up){ bar.style.transform = ''; }
      if(down || up) last = y;
    }
    window.addEventListener('scroll', function(){ apply(window.scrollY || document.documentElement.scrollTop); }, {passive:true});
    var c = document.querySelector('.content');
    if(c) c.addEventListener('scroll', function(){ apply(c.scrollTop); }, {passive:true});
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', setup);
  else setup();
})();

/* ── inline block: mobileNavSpacerV1 (原 files.html:2314) ── */
(function(){
  function size(){
    var bar = document.getElementById('mobilebar');
    var sp = document.getElementById('mobileBottomSpacer');
    if(!sp) return;
    if(!bar){ sp.style.height='0px'; return; }
    var visible = getComputedStyle(bar).display !== 'none';
    sp.style.height = visible ? (bar.offsetHeight + 24) + 'px' : '0px';
  }
  function bind(){ size(); window.addEventListener('resize', size); window.addEventListener('load', size);
    setTimeout(size, 300); setTimeout(size, 1000); }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', bind);
  else bind();
})();
