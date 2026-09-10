/*
 * TRONCLASS OFFICE VIEW V2
 * 雲端硬碟的 Office 預覽：完全不依賴 LibreOffice 或任何伺服器端轉檔服務。
 * 所有解析都在瀏覽器本機完成，唯一的相依是同源的 JSZip 全域。
 *
 *   - Word       (.docx)           -> OOXML：word/document.xml
 *   - PowerPoint (.pptx)           -> OOXML：ppt/presentation.xml + slides
 *   - Excel      (.xlsx)           -> OOXML：xl/workbook.xml + worksheets（含合併儲存格）
 *   - ODF        (.odt/.ods/.odp)  -> content.xml（OpenDocument 是 zip + XML）
 *   - RTF        (.rtf)            -> 純文字控制字解析（\uN? / \'hh / \par）
 *   - 舊二進位   (.doc/.ppt/.xls)  -> 無法可靠解析，顯示「請下載查看」
 *
 * 這個「本地、同源、純前端」的作法符合檔案頁的嚴格 CSP（script-src 'self'），
 * 資料不離開使用者瀏覽器，也不佔伺服器磁碟。
 *
 * 輸出一律只包含結構性標籤與 inline style，文字都經 escapeHtml() 跳脫；
 * 賦值 innerHTML 時會再經過 Trusted Types 政策（DOMPurify）過濾一次。
 */
(function () {
  'use strict';

  // EMU = 914400 per inch。把投影片縮放到容器寬度內，維持 16:9/4:3 比例。
  function buildSlideDocument(presentationXml, slides, media) {
    // OFFICE_ROBUST_V1: 部分匯出工具不寫 sldSz，缺了就退回 4:3 預設，不丟例外。
    var size = presentationXml.getElementsByTagNameNS(
      'http://schemas.openxmlformats.org/presentationml/2006/main', 'sldSz'
    )[0];
    var cx = size ? (parseInt(size.getAttribute('cx'), 10) || 9144000) : 9144000;
    var cy = size ? (parseInt(size.getAttribute('cy'), 10) || 6858000) : 6858000;
    return { cx: cx, cy: cy, slides: slides, media: media };
  }

  function shapePosition(xfrm, slideCx, slideCy, phType) {
    // 依序嘗試 off/ext（a:xfrm）；沒有座標的（沿用版面的 placeholder）改用預設區塊排版。
    var off = xfrm && (xfrm.getElementsByTagNameNS(NS_A, 'off')[0]);
    var ext = xfrm && (xfrm.getElementsByTagNameNS(NS_A, 'ext')[0]);
    if (off) {
      var x = parseInt(off.getAttribute('x'), 10) || 0;
      var y = parseInt(off.getAttribute('y'), 10) || 0;
      var w = ext ? (parseInt(ext.getAttribute('cx'), 10) || slideCx) : slideCx;
      var h = ext ? (parseInt(ext.getAttribute('cy'), 10) || slideCy) : slideCy;
      return {
        left: (x / slideCx * 100),
        top: (y / slideCy * 100),
        width: (w / slideCx * 100),
        height: (h / slideCy * 100)
      };
    }
    // 版面 placeholder 的粗略預設位置（近似 PowerPoint 版面，避免文字疊在一起）。
    var isTitle = phType === 'ctrTitle' || phType === 'title';
    var isBody = phType === 'body' || phType === 'subTitle';
    if (isTitle) return { left: 4, top: 3, width: 92, height: 18 };
    if (isBody) return { left: 5, top: 24, width: 90, height: 70 };
    return { left: 4, top: 4, width: 92, height: 90 };
  }

  function runStyle(rPr) {
    if (!rPr) return {};
    var style = {};
    // sz 單位為 1/100 pt
    var sz = parseInt(rPr.getAttribute('sz'), 10);
    if (!isNaN(sz) && sz > 0) style.fontSize = (sz / 100) + 'pt';
    if (rPr.getAttribute('b') === '1') style.fontWeight = 'bold';
    if (rPr.getAttribute('i') === '1') style.fontStyle = 'italic';
    var font = rPr.getElementsByTagNameNS(NS_A, 'latin')[0];
    if (font && font.getAttribute('typeface')) style.fontFamily = font.getAttribute('typeface');
    var fill = rPr.getElementsByTagNameNS(NS_A, 'solidFill')[0];
    if (fill) {
      var srgb = fill.getElementsByTagNameNS(NS_A, 'srgbClr')[0];
      if (srgb && srgb.getAttribute('val')) style.color = '#' + srgb.getAttribute('val');
    }
    return style;
  }

  function paraProps(pPr) {
    if (!pPr) return {};
    var props = {};
    var algn = pPr.getAttribute('algn');
    if (algn === 'ctr') props.textAlign = 'center';
    else if (algn === 'r') props.textAlign = 'right';
    else if (algn === 'just') props.textAlign = 'justify';
    else props.textAlign = 'left';
    var lvl = parseInt(pPr.getAttribute('lvl'), 10);
    if (!isNaN(lvl) && lvl > 0) props.indent = (lvl * 16) + 'px';
    return props;
  }

  // 解析一個 <a:p> 段落 -> HTML 片段
  function runToHtml(p) {
    var pPr = p.getElementsByTagNameNS(NS_A, 'pPr')[0];
    var props = paraProps(pPr);
    var pstyle = '';
    for (var k in props) pstyle += k + ':' + props[k] + ';';
    var runs = p.getElementsByTagNameNS(NS_A, 'r');
    var html = '';
    for (var i = 0; i < runs.length; i++) {
      var rPr = runs[i].getElementsByTagNameNS(NS_A, 'rPr')[0];
      var t = runs[i].getElementsByTagNameNS(NS_A, 't')[0];
      var text = t ? t.textContent || '' : '';
      var st = runStyle(rPr);
      html += '<span style="' + styleAttr(st) + '">' + escapeHtml(text.replace(/\u00a0/g, ' ')) + '</span>';
    }
    // 沒有 run 的文字（例如只在 pPr 內）也會顯示
    if (!html) {
      var tDirect = p.getElementsByTagNameNS(NS_A, 't')[0];
      html = '<span>' + escapeHtml((tDirect ? tDirect.textContent || '' : '').replace(/\u00a0/g, ' ')) + '</span>';
    }
    if (!html) html = '<span>&nbsp;</span>';
    return '<p style="margin:0 0 6px 0;' + pstyle + '">' + html + '</p>';
  }

  function placeholderType(sp) {
    // 取得 <p:ph type="...">；沒有 ph 屬性時推斷是標題/內文 placeholder。
    var ph = sp.getElementsByTagNameNS(NS_P, 'ph')[0];
    if (ph) return ph.getAttribute('type') || '';
    var nvSpPr = sp.getElementsByTagNameNS(NS_P, 'nvSpPr')[0];
    var cNvPr = nvSpPr && nvSpPr.getElementsByTagNameNS(NS_P, 'cNvPr')[0];
    var name = cNvPr && cNvPr.getAttribute('name') || '';
    if (/title/i.test(name)) return 'title';
    if (/body|subtitle|content|text/i.test(name)) return 'body';
    return '';
  }

  function shapeToHtml(sp, doc) {
    // 文字方塊
    var txBody = sp.getElementsByTagNameNS(NS_P, 'txBody')[0];
    var xfrm = sp.getElementsByTagNameNS(NS_A, 'xfrm')[0];
    if (txBody) {
      var phType = placeholderType(sp);
      var pos = shapePosition(xfrm, doc.cx, doc.cy, phType);
      var paras = txBody.getElementsByTagNameNS(NS_A, 'p');
      var inner = '';
      for (var i = 0; i < paras.length; i++) inner += runToHtml(paras[i]);
      var isTitle = phType === 'ctrTitle' || phType === 'title';
      var titleStyle = isTitle
        ? ';font-size:1.8em;font-weight:700;font-family:system-ui,-apple-system,"Segoe UI",sans-serif;'
        : (phType ? ';font-size:1.05em;' : '');
      return '<div class="tron-slide-text" style="left:' + pos.left + '%;top:' + pos.top +
        '%;width:' + pos.width + '%;height:' + pos.height + '%;overflow:hidden;' + titleStyle + '">' +
        inner + '</div>';
    }
    // 圖片
    var blip = sp.getElementsByTagNameNS(NS_A, 'blip')[0];
    if (blip) {
      var relId = blip.getAttributeNS(NS_R, 'embed');
      var media = relId && doc.media[relId];
      var p2 = shapePosition(xfrm, doc.cx, doc.cy, placeholderType(sp));
      if (media) {
        return '<div class="tron-slide-img" style="left:' + p2.left + '%;top:' + p2.top +
          '%;width:' + p2.width + '%;height:' + p2.height + '%;">' +
          '<img src="' + media + '" alt="" style="width:100%;height:100%;object-fit:contain;"></div>';
      }
    }
    return '';
  }

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  // JS 物件鍵（fontWeight / fontSize / textAlign）轉為 CSS 小寫連字號（font-weight / font-size / text-align）。
  function cssKey(k) {
    return String(k).replace(/([A-Z])/g, function (m, C) { return '-' + C.toLowerCase(); });
  }

  function styleAttr(st) {
    var out = '';
    for (var k in st) out += cssKey(k) + ':' + st[k] + ';';
    return out;
  }

  var NS_A = 'http://schemas.openxmlformats.org/drawingml/2006/main';
  var NS_P = 'http://schemas.openxmlformats.org/presentationml/2006/main';
  var NS_R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';

  function xml(str) {
    // XML_PARSER_TOLERANT_V1: 部分匯出檔帶 BOM 或前導空白，會讓 DOMParser
    // 直接回 parsererror；先剝掉再解析，parsererror 時再試一次乾淨版。
    if (typeof str === 'string') str = str.replace(/^\uFEFF+/, '').replace(/\u0000/g, '').replace(/^\s+/, '');
    var doc = new DOMParser().parseFromString(str, 'application/xml');
    if (doc.getElementsByTagName('parsererror').length) {
      doc = new DOMParser().parseFromString(str.trim(), 'application/xml');
    }
    return doc;
  }

  // 讀取 slideN.xml.rels 的 rId -> 媒體路徑 對照。
  // 例：ppt/slides/slide2.xml 的 rels 在 ppt/slides/_rels/slide2.xml.rels（<目錄>/_rels/<檔名>.rels）。
  async function readSlideRels(zip, slidePart) {
    var slash = slidePart.lastIndexOf('/');
    var relsPath = slidePart.slice(0, slash) + '/_rels/' + slidePart.slice(slash + 1) + '.rels';
    var relsText = zip.file(relsPath) && await zip.file(relsPath).async('text');
    var map = {};
    if (!relsText) return map;
    var doc = xml(relsText);
    var rels = doc.getElementsByTagName('Relationship');
    for (var i = 0; i < rels.length; i++) {
      var rel = rels[i];
      if (rel.getAttribute('Target') && rel.getAttribute('Id')) {
        map[rel.getAttribute('Id')] = rel.getAttribute('Target');
      }
    }
    return map;
  }

  function base64DataUri(bytes) {
    var binary = '';
    for (var i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
    return 'data:image/png;base64,' + btoa(binary);
  }

  // DOCX_STRICT_NS_V1：Word「Strict Open XML」另存（或部分第三方工具）使用的命名空間是
  // http://purl.oclc.org/ooxml/wordprocessingml/main，而非 Transitional 的
  // schemas.openxmlformats.org/…；原本寫死 Transitional 命名空間會讓 body 找不到，
  // 誤報「document.xml 沒有 body 節點」。Word 元素在 docx 內 localName 不會與其他
  // 命名空間衝突（DrawingML 文字走 w:txbxContent），因此一律用 '*' 依 localName 查找。
  var W_NS = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main';
  var W_NS_STRICT = 'http://purl.oclc.org/ooxml/wordprocessingml/main';

  // 讀取「可能在命名空間前綴內」的屬性（如 w:val）。getAttribute('val') 對前綴屬性會回傳 null，
  // 改用 attributes 依 localName 比對，才能穩定取到值。
  function attrLocal(el, name) {
    if (!el || !el.attributes) return '';
    var attrs = el.attributes;
    for (var i = 0; i < attrs.length; i++) {
      if (attrs[i].localName === name || attrs[i].name === name) return attrs[i].value;
    }
    return '';
  }

  // Word run 屬性 -> inline style（避免用 <style>，以免命中原 CSS 非元嚴格的 style-src 限制）
  function docxRunStyle(rPr) {
    if (!rPr) return {};
    var st = {};
    if (rPr.getElementsByTagNameNS('*', 'b')[0]) st.fontWeight = 'bold';
    if (rPr.getElementsByTagNameNS('*', 'i')[0]) st.fontStyle = 'italic';
    if (rPr.getElementsByTagNameNS('*', 'u')[0]) st.textDecoration = 'underline';
    var color = rPr.getElementsByTagNameNS('*', 'color')[0];
    var colorVal = color && attrLocal(color, 'val');
    if (colorVal && colorVal !== 'auto') {
      st.color = '#' + colorVal;
    }
    var sz = rPr.getElementsByTagNameNS('*', 'sz')[0];
    var szV = sz && parseInt(attrLocal(sz, 'val'), 10);
    if (!isNaN(szV) && szV > 0) st.fontSize = (szV / 2) + 'pt'; // 單位為半 pt
    return st;
  }

  function docxParaHtml(p, media) {
    var pPr = p.getElementsByTagNameNS('*', 'pPr')[0];
    var styleName = '';
    var pstyle = '';
    if (pPr) {
      var pStyle = pPr.getElementsByTagNameNS('*', 'pStyle')[0];
      if (pStyle) styleName = attrLocal(pStyle, 'val');
      if (pPr.getElementsByTagNameNS('*', 'numPr')[0]) styleName += ' ListNum';
      var jc = pPr.getElementsByTagNameNS('*', 'jc')[0];
      if (jc) {
        var v = attrLocal(jc, 'val');
        if (v === 'center') pstyle += 'text-align:center;';
        else if (v === 'right') pstyle += 'text-align:right;';
        else if (v === 'both' || v === 'justify') pstyle += 'text-align:justify;';
        else pstyle += 'text-align:left;';
      }
    }

    var isTitle = /Heading1|Title/i.test(styleName);
    var isH2 = /Heading2/i.test(styleName);
    var isH3 = /Heading3/i.test(styleName);
    var isH4 = /Heading[4-9]/i.test(styleName);
    var bullet = /ListBullet|List/i.test(styleName);

    var html = '';
    var runs = p.getElementsByTagNameNS('*', 'r');
    for (var i = 0; i < runs.length; i++) {
      // DOCX_BLOCK_WALK_V1：mc:AlternateContent 的 Choice 與 Fallback 內容重複（文字方塊常見），
      // 只取 Fallback（瀏覽器本來就畫不了 Choice 的 wps 圖形），避免文字出現兩次。
      if (docxInsideChoice(runs[i], p)) continue;
      var rPr = runs[i].getElementsByTagNameNS('*', 'rPr')[0];
      var st = docxRunStyle(rPr);
      // DOCX_BLOCK_WALK_V1：一個 run 可能有多個 <w:t>，中間夾 <w:tab>／<w:br>；原本只取第一個 t
      // 會吃掉後面的文字。改為依序走 run 的直接子節點。
      var txt = '';
      var rk = runs[i].childNodes;
      for (var k = 0; k < rk.length; k++) {
        var rn = rk[k];
        if (rn.nodeType !== 1) continue;
        var rl = rn.localName;
        if (rl === 't') txt += (rn.textContent || '');
        else if (rl === 'tab') txt += '\t';
        else if (rl === 'br' || rl === 'cr') txt += '\n';
        else if (rl === 'noBreakHyphen') txt += '\u2011';
        else if (rl === 'softHyphen') txt += '\u00ad';
        else if (rl === 'sym') txt += '\u25a1';
      }
      if (txt) {
        txt = txt.replace(/\u00a0/g, ' ');
        if (i === 0) txt = txt.replace(/^[ \u2022]+/, ''); // 避免標題已含 • 又再加上
        var sty = styleAttr(st);
        if (/[\t\n]/.test(txt)) sty += 'white-space:pre-wrap;';
        html += '<span style="' + sty + '">' + escapeHtml(txt) + '</span>';
      }
      // 內嵌圖片
      var drawing = runs[i].getElementsByTagNameNS('*', 'drawing')[0];
      if (drawing) {
        var blip = drawing.getElementsByTagNameNS(NS_A, 'blip')[0];
        if (blip) {
          var relId = attrLocal(blip, 'embed'); // Strict OOXML 的 r: 命名空間不同，改依 localName 取
          var src = relId && media[relId];
          if (src) html += '<img src="' + src + '" alt="" style="display:block;max-width:100%;height:auto;margin:8px 0;">';
        }
      }
    }
    if (!html) html = '<span>&nbsp;</span>';

    if (bullet) return '<p style="margin:0 0 6px 0;padding-left:22px;text-indent:-14px;' + pstyle + '">• ' + html + '</p>';
    if (isTitle) return '<h2 style="font-size:1.5em;font-weight:700;margin:10px 0 6px 0;' + pstyle + '">' + html + '</h2>';
    if (isH2) return '<h3 style="font-size:1.3em;font-weight:700;margin:8px 0 6px 0;' + pstyle + '">' + html + '</h3>';
    if (isH3) return '<h4 style="font-size:1.15em;font-weight:700;margin:8px 0 6px 0;' + pstyle + '">' + html + '</h4>';
    if (isH4) return '<h5 style="font-weight:700;margin:8px 0 6px 0;' + pstyle + '">' + html + '</h5>';
    return '<p style="margin:0 0 6px 0;' + pstyle + '">' + html + '</p>';
  }

  function docxCellHtml(tc, media) {
    // DOCX_BLOCK_WALK_V1：儲存格內容走區塊遍歷（支援巢狀表格、內容控制項）。
    var html = docxBlocksHtml(tc, media, 0);
    if (!html) html = '&nbsp;';
    return '<td style="border:1px solid #cbd2e0;padding:6px 8px;vertical-align:top;">' + html + '</td>';
  }

  // DOCX_BLOCK_WALK_V1：只收「直接」子列／子格（可穿過 sdt／customXml 等包裝），
  // 原本 getElementsByTagNameNS('*','tr') 會把巢狀表格的列也拉到外層，內容重複。
  function docxDirectChildren(el, localName, depth) {
    var out = [];
    var kids = el.childNodes;
    for (var i = 0; i < kids.length; i++) {
      var n = kids[i];
      if (n.nodeType !== 1) continue;
      if (n.localName === localName) out.push(n);
      else if (depth < 6 && DOCX_BLOCK_WRAPPERS[n.localName]) {
        out = out.concat(docxDirectChildren(n, localName, depth + 1));
      }
    }
    return out;
  }

  function docxTableHtml(tbl, media) {
    var rows = docxDirectChildren(tbl, 'tr', 0);
    var html = '<table style="border-collapse:collapse;width:100%;margin:8px 0;"><tbody>';
    for (var r = 0; r < rows.length; r++) {
      var cells = docxDirectChildren(rows[r], 'tc', 0);
      html += '<tr>';
      for (var c = 0; c < cells.length; c++) html += docxCellHtml(cells[c], media);
      html += '</tr>';
    }
    html += '</tbody></table>';
    return html;
  }

  // DOCX_BLOCK_WALK_V1：Word 的段落／表格不一定是 <w:body> 的「直接」子節點——
  // 內容控制項（w:sdt/w:sdtContent，範本、表單、目錄極常見）、customXml、追蹤修訂
  // （w:ins/w:del）、smartTag 都會再包一層；原本只掃 body 直接子節點，這類檔案會被
  // 誤判成「無法解析（可能是舊版 .doc）」。這裡遞迴穿過包裝元素找 p／tbl。
  var DOCX_BLOCK_WRAPPERS = {
    sdt: 1, sdtContent: 1, customXml: 1, smartTag: 1, ins: 1, del: 1,
    moveTo: 1, moveFrom: 1, txbxContent: 1, Fallback: 1, Choice: 1
  };

  function docxInsideChoice(el, stopAt) {
    var n = el.parentNode;
    while (n && n !== stopAt) {
      if (n.localName === 'Choice') {
        // 只有在同層真的有 Fallback 時才略過 Choice；否則 Choice 是唯一內容，得保留。
        var par = n.parentNode;
        if (par && par.getElementsByTagNameNS('*', 'Fallback').length) return true;
        return false;
      }
      n = n.parentNode;
    }
    return false;
  }

  function docxBlocksHtml(container, media, depth) {
    var html = '';
    var kids = container.childNodes;
    for (var c = 0; c < kids.length; c++) {
      var node = kids[c];
      if (node.nodeType !== 1) continue;
      var ln = node.localName;
      if (ln === 'p') html += docxParaHtml(node, media);
      else if (ln === 'tbl') html += docxTableHtml(node, media);
      else if (ln === 'sectPr' || ln === 'sdtPr' || ln === 'sdtEndPr' || ln === 'tblPr' || ln === 'tblGrid' || ln === 'bookmarkStart' || ln === 'bookmarkEnd') continue;
      else if (ln === 'altChunk') {
        html += '<div class="tron-office-note" style="font-size:12px;opacity:.7;margin:6px 0;">（此處有外部嵌入內容 altChunk，僅能於 Word 中檢視）</div>';
      } else if (ln === 'AlternateContent') {
        var pick = node.getElementsByTagNameNS('*', 'Fallback')[0] || node.getElementsByTagNameNS('*', 'Choice')[0];
        if (pick && depth < 16) html += docxBlocksHtml(pick, media, depth + 1);
      } else if (depth < 16 && (DOCX_BLOCK_WRAPPERS[ln] || node.getElementsByTagNameNS('*', 'p').length)) {
        html += docxBlocksHtml(node, media, depth + 1);
      }
    }
    return html;
  }

  // OFFICE_ROBUST_V1: 真實檔案（Word/PowerPoint/LibreOffice/線上轉檔工具產出）
  // 不一定有齊全的 parts。所有 zip.file(...) 都可能回 null，任何一步缺失都
  // 要「優雅降級」（少圖、少一頁），而不是整份丟出例外變成「無法解析」。
  // XML_ENCODING_V1: 部分 pptx/docx 的 XML part 實為 UTF-16（每個字元夾一個
  // 0x00），但 JSZip 的 .async('text') 一律當 UTF-8 解，結果字串裡塞滿 \u0000，
  // DOMParser 直接報 "Document is empty / line 1 column 1"。改為先取 bytes，
  // 依 BOM 或 NUL 交錯判斷編碼再解，才能正確還原。
  function decodeXmlBytes(u) {
    if (!u || !u.length) return '';
    if (u.length >= 3 && u[0] === 0xEF && u[1] === 0xBB && u[2] === 0xBF) {
      return new TextDecoder('utf-8').decode(u.subarray(3)); // UTF-8 BOM
    }
    if (u.length >= 2 && u[0] === 0xFF && u[1] === 0xFE) {
      return new TextDecoder('utf-16le').decode(u.subarray(2)); // UTF-16LE BOM
    }
    if (u.length >= 2 && u[0] === 0xFE && u[1] === 0xFF) {
      return new TextDecoder('utf-16be').decode(u.subarray(2)); // UTF-16BE BOM
    }
    // 無 BOM：統計前 256 bytes 奇數位元的 0x00，判定是否 UTF-16LE。
    var nul = 0, n = Math.min(u.length, 256);
    for (var i = 1; i < n; i += 2) if (u[i] === 0x00) nul++;
    if (nul > 8) return new TextDecoder('utf-16le').decode(u);
    return new TextDecoder('utf-8').decode(u);
  }

  function zipText(zip, part) {
    var f = zip.file(part);
    return f ? f.async('uint8array').then(decodeXmlBytes) : Promise.resolve(null);
  }

  // OFFICE_SNIFF_V1: 在丟給 JSZip 之前先看魔數，分辨「真 zip / OLE2 舊格式 /
  // HTML 偽裝 / RTF / 其他」。真實世界常有 .docx 其實是舊版 .doc（OLE2）或
  // Word 另存網頁（HTML），JSZip 對這些只會丟出難懂的
  // "Can't find end of central directory"。先嗅探才能給出有意義的說明。
  function officeSniff(buf) {
    var u = new Uint8Array(buf.slice ? buf.slice(0, 8) : buf);
    if (u.length >= 2 && u[0] === 0x50 && u[1] === 0x4B) return 'zip';
    if (u.length >= 4 && u[0] === 0xD0 && u[1] === 0xCF &&
        u[2] === 0x11 && u[3] === 0xE0) return 'ole2';
    var head = '';
    try {
      var bytes = new Uint8Array(buf.slice ? buf.slice(0, 512) : buf);
      for (var i = 0; i < bytes.length; i++) head += String.fromCharCode(bytes[i]);
    } catch (e2) { return 'unknown'; }
    if (/^\{\\rtf/i.test(head.replace(/^\s+/, ''))) return 'rtf';
    if (/^\s*</.test(head) || /<html|<body|<\?xml|MIME-Version/i.test(head)) return 'html';
    return 'unknown';
  }

  // 針對「副檔名說是 zip 類 Office、實際卻不是」的檔案，產生清楚說明。
  function nonZipOfficeMessage(kind, ext) {
    var what = {
      ole2: '舊版二進位格式（OLE2，即 .doc/.ppt/.xls 時代的結構）',
      html: 'HTML／網頁存檔格式（Word「另存網頁」產生的檔案）',
      rtf: 'RTF 富文字格式',
      unknown: '無法辨識的二進位內容'
    }[kind] || '無法辨識的內容';
    var hint = kind === 'ole2'
      ? '請按上方「下載」後以 Word／PowerPoint／Excel 開啟。'
      : kind === 'html'
        ? '可下載後直接用瀏覽器或 Word 開啟。'
        : kind === 'rtf'
          ? '此副檔名應為 .rtf 才能走 RTF 預覽；請下載查看。'
          : '檔案內容與副檔名 .' + ext + ' 不符（也可能伺服器回傳了錯誤頁），請下載查看。';
    return '<div class="tron-office-error">這個 .' + ext + ' 檔案實際為' + what +
      '，不是標準 OOXML zip 結構，無法在本機預覽。' + hint + '</div>';
  }

  // DOCX_STRICT_NS_V1：最後手段的純文字擷取（不經 DOM）。命名空間前綴不限 w:。
  function docxRegexFallback(text) {
    if (!text) return '';
    var bodyAt = text.search(/<(?:[\w-]+:)?body[\s>]/);
    var src = bodyAt >= 0 ? text.slice(bodyAt) : text;
    var paras = src.split(/<(?:[\w-]+:)?p[\s>]/);
    var out = '', total = 0;
    for (var i = 1; i < paras.length; i++) {
      var texts = [], m, re = /<(?:[\w-]+:)?t(?:\s[^>]*)?>([\s\S]*?)<\/(?:[\w-]+:)?t>/g;
      while ((m = re.exec(paras[i])) !== null) { if (m[1]) texts.push(m[1]); }
      if (/<(?:[\w-]+:)?tab\b/.test(paras[i])) texts.push('\t');
      var line = texts.join('').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&apos;/g, "'").replace(/&amp;/g, '&');
      total += line.length;
      out += '<p style="margin:0 0 6px 0;white-space:pre-wrap;">' + (line ? escapeHtml(line) : '&nbsp;') + '</p>';
    }
    if (!total) return '';
    return '<div class="tron-docx"><div class="tron-office-note" style="font-size:12px;opacity:.7;margin-bottom:10px;">以簡化模式顯示（僅文字，無格式／圖片）。</div>' + out + '</div>';
  }

  async function renderWord(arrayBuffer) {
    var zip = await JSZip.loadAsync(arrayBuffer);
    var docText = await zipText(zip, 'word/document.xml');
    if (!docText) {
      return '<div class="tron-office-error">此 Word 檔缺少 word/document.xml（可能是舊版 .doc 二進位格式）。請按上方「下載」查看。</div>';
    }
    var docXml = xml(docText);
    var body = docXml.getElementsByTagNameNS('*', 'body')[0];
    if (!body) {
      // DOCX_STRICT_NS_V1：DOMParser 失敗（parsererror）或結構異常時，退回 regex 直接抓
      // <w:t> 文字並以 <w:p> 分段——至少看得到內容，而不是只給一句「無法解析」。
      var fallback = docxRegexFallback(docText);
      if (fallback) return fallback;
      var why = docXml.getElementsByTagName('parsererror').length
        ? 'document.xml 不是合法的 XML（瀏覽器解析失敗）'
        : 'document.xml 沒有 body 節點（根元素命名空間：' + escapeHtml((docXml.documentElement && docXml.documentElement.namespaceURI) || '無') + '）';
      return '<div class="tron-office-error">此 Word 檔的 ' + why + '，無法解析。請按上方「下載」查看。</div>';
    }

    // rels 只用來對照內嵌圖片；缺失時頂多沒有圖，不該整份失敗。
    var media = {};
    var relText = await zipText(zip, 'word/_rels/document.xml.rels');
    if (relText) {
      var relDoc = xml(relText);
      var rels = relDoc.getElementsByTagName('Relationship');
      for (var i = 0; i < rels.length; i++) {
        var rel = rels[i];
        var tgt = rel.getAttribute('Target') || '';
        if (/^media\//.test(tgt)) {
          var part = 'word/' + tgt.replace(/^\/+/, '');
          var file = zip.file(part);
          if (file) media[rel.getAttribute('Id')] = base64DataUri(await file.async('uint8array'));
        }
      }
    }

    // DOCX_BLOCK_WALK_V1：遞迴遍歷（穿過 sdt／customXml／ins 等包裝）。
    var html = docxBlocksHtml(body, media, 0);
    if (!html) {
      // 走到這裡代表 zip 與 XML 都正常（不是舊版 .doc），只是沒有找到段落／表格：
      // 先試 regex 純文字擷取，再不行就照實回報（空白文件／只有物件），不再誤導成舊格式。
      var fb = docxRegexFallback(docText);
      if (fb) return fb;
      var tCount = docXml.getElementsByTagNameNS('*', 't').length;
      var kidsNames = [];
      for (var bi = 0; bi < body.childNodes.length && kidsNames.length < 8; bi++) {
        if (body.childNodes[bi].nodeType === 1) kidsNames.push(body.childNodes[bi].localName);
      }
      var reason = tCount === 0
        ? '此 Word 檔沒有可顯示的文字內容（可能是空白文件，或內容全為圖片／物件／嵌入式檔案）。'
        : '此 Word 檔的結構無法辨識（body 子節點：' + escapeHtml(kidsNames.join(', ') || '無') + '）。';
      return '<div class="tron-office-error">' + reason + '請按上方「下載」查看。</div>';
    }
    return '<div class="tron-docx">' + html + '</div>';
  }

  // ─────────────────────────────────────────────────────────────────────────
  // XLSX（.xlsx）—— OOXML 試算表
  // ─────────────────────────────────────────────────────────────────────────

  var SS_NS = 'http://schemas.openxmlformats.org/spreadsheetml/2006/main';
  //  huge 檔案保護：教材常有整份成績單，全部塞進 DOM 會讓分頁凍住。
  var XLSX_MAX_ROWS = 400;
  var XLSX_MAX_COLS = 60;

  // "C12" -> 行號 0 起算的欄索引（C -> 2）
  function colFromRef(ref) {
    var m = /^([A-Z]+)/.exec(String(ref || ''));
    if (!m) return 0;
    var n = 0;
    var letters = m[1];
    for (var i = 0; i < letters.length; i++) n = n * 26 + (letters.charCodeAt(i) - 64);
    return n - 1;
  }

  function colLabel(idx) {
    var s = '';
    var n = idx + 1;
    while (n > 0) {
      var r = (n - 1) % 26;
      s = String.fromCharCode(65 + r) + s;
      n = Math.floor((n - 1) / 26);
    }
    return s;
  }

  async function readSharedStrings(zip) {
    var f = zip.file('xl/sharedStrings.xml');
    if (!f) return [];
    var doc = xml(await f.async('text'));
    var sis = doc.getElementsByTagNameNS(SS_NS, 'si');
    var out = [];
    for (var i = 0; i < sis.length; i++) {
      // 一個 <si> 可能由多個 <r><t> 富文字 run 組成，要全部接起來。
      var ts = sis[i].getElementsByTagNameNS(SS_NS, 't');
      var buf = '';
      for (var t = 0; t < ts.length; t++) buf += ts[t].textContent || '';
      out.push(buf);
    }
    return out;
  }

  function cellText(c, shared) {
    var type = attrLocal(c, 't');
    if (type === 'inlineStr') {
      var is = c.getElementsByTagNameNS(SS_NS, 'is')[0];
      return is ? (is.textContent || '') : '';
    }
    var v = c.getElementsByTagNameNS(SS_NS, 'v')[0];
    var raw = v ? (v.textContent || '') : '';
    if (type === 's') {
      var idx = parseInt(raw, 10);
      return isNaN(idx) ? '' : (shared[idx] || '');
    }
    if (type === 'b') return raw === '1' ? 'TRUE' : 'FALSE';
    // t="str"（公式結果）、t="e"（錯誤）與純數字都直接顯示原始值。
    return raw;
  }

  // mergeCell ref "B2:D4" -> {row:1, col:1, rowSpan:3, colSpan:3}
  function parseMerge(ref) {
    var m = /^([A-Z]+)(\d+):([A-Z]+)(\d+)$/.exec(String(ref || ''));
    if (!m) return null;
    var c1 = colFromRef(m[1]), r1 = parseInt(m[2], 10) - 1;
    var c2 = colFromRef(m[3]), r2 = parseInt(m[4], 10) - 1;
    return { row: r1, col: c1, rowSpan: r2 - r1 + 1, colSpan: c2 - c1 + 1 };
  }

  async function renderSheet(zip, part, shared) {
    var f = zip.file(part);
    if (!f) return null;
    var doc = xml(await f.async('text'));
    var sheetData = doc.getElementsByTagNameNS(SS_NS, 'sheetData')[0];
    if (!sheetData) return null;

    var merges = [];
    var mc = doc.getElementsByTagNameNS(SS_NS, 'mergeCell');
    for (var i = 0; i < mc.length; i++) {
      var parsed = parseMerge(attrLocal(mc[i], 'ref'));
      if (parsed) merges.push(parsed);
    }
    // 被合併蓋住的格子要跳過，否則表格會錯位。
    var covered = {};
    for (var g = 0; g < merges.length; g++) {
      var mg = merges[g];
      for (var r = mg.row; r < mg.row + mg.rowSpan; r++) {
        for (var c = mg.col; c < mg.col + mg.colSpan; c++) {
          if (r === mg.row && c === mg.col) continue;
          covered[r + ':' + c] = true;
        }
      }
    }
    var mergeAt = {};
    for (var k = 0; k < merges.length; k++) mergeAt[merges[k].row + ':' + merges[k].col] = merges[k];

    var rows = sheetData.getElementsByTagNameNS(SS_NS, 'row');
    var html = '';
    var maxCol = 0;
    var emitted = 0;
    var truncated = false;
    for (var ri = 0; ri < rows.length; ri++) {
      if (emitted >= XLSX_MAX_ROWS) { truncated = true; break; }
      var rowNum = parseInt(attrLocal(rows[ri], 'r'), 10) - 1;
      if (isNaN(rowNum) || rowNum < 0) rowNum = ri;
      var cells = rows[ri].getElementsByTagNameNS(SS_NS, 'c');
      var tds = '';
      var any = false;
      for (var ci = 0; ci < cells.length; ci++) {
        var col = colFromRef(attrLocal(cells[ci], 'r'));
        if (col >= XLSX_MAX_COLS) continue;
        if (col > maxCol) maxCol = col;
        if (covered[rowNum + ':' + col]) continue;
        var text = cellText(cells[ci], shared);
        if (text !== '') any = true;
        var span = mergeAt[rowNum + ':' + col];
        var attrs = '';
        if (span && span.colSpan > 1) attrs += ' colspan="' + span.colSpan + '"';
        if (span && span.rowSpan > 1) attrs += ' rowspan="' + span.rowSpan + '"';
        tds += '<td' + attrs + '>' + escapeHtml(text) + '</td>';
      }
      // 整列全空就不輸出，避免成績單下方的幾百行空白把版面撐開。
      if (!any && !mergeAt[rowNum + ':0']) continue;
      html += '<tr><th class="tron-xlsx-rowhead">' + (rowNum + 1) + '</th>' + tds + '</tr>';
      emitted++;
    }
    if (!html) return null;

    var head = '<tr><th class="tron-xlsx-rowhead"></th>';
    for (var h = 0; h <= Math.min(maxCol, XLSX_MAX_COLS - 1); h++) {
      head += '<th>' + colLabel(h) + '</th>';
    }
    head += '</tr>';

    return '<table class="tron-xlsx-table"><thead>' + head + '</thead><tbody>' + html +
      '</tbody></table>' +
      (truncated ? '<div class="tron-office-note">試算表過大，僅顯示前 ' + XLSX_MAX_ROWS + ' 列。完整內容請下載。</div>' : '');
  }

  async function renderExcel(arrayBuffer) {
    var zip = await JSZip.loadAsync(arrayBuffer);
    if (!zip.file('xl/workbook.xml')) {
      return '<div class="tron-office-error">此 Excel 檔無法解析（可能是舊版 .xls 二進位格式）。請按上方「下載」查看。</div>';
    }
    var shared = await readSharedStrings(zip);

    // sheet 名稱在 workbook.xml，實際路徑要透過 workbook.xml.rels 的 rId 對照。
    var wb = xml(await zip.file('xl/workbook.xml').async('text'));
    var relMap = {};
    var relFile = zip.file('xl/_rels/workbook.xml.rels');
    if (relFile) {
      var relDoc = xml(await relFile.async('text'));
      var rels = relDoc.getElementsByTagName('Relationship');
      for (var i = 0; i < rels.length; i++) {
        relMap[rels[i].getAttribute('Id')] = rels[i].getAttribute('Target') || '';
      }
    }

    var sheets = wb.getElementsByTagNameNS(SS_NS, 'sheet');
    var tabs = '';
    var panes = '';
    var count = 0;
    for (var s = 0; s < sheets.length; s++) {
      var name = attrLocal(sheets[s], 'name') || ('工作表' + (s + 1));
      var rid = sheets[s].getAttributeNS(NS_R, 'id') || '';
      var target = relMap[rid] || ('worksheets/sheet' + (s + 1) + '.xml');
      var part = target.replace(/^\/?xl\//, '');
      if (part.indexOf('worksheets/') !== 0 && part.indexOf('/') === -1) part = 'xl/' + part;
      if (part.indexOf('xl/') !== 0) part = 'xl/' + part;

      var table = await renderSheet(zip, part, shared);
      if (!table) continue;
      var active = count === 0 ? ' active' : '';
      tabs += '<span class="tron-xlsx-tab' + active + '" data-sheet="' + count + '" role="button" tabindex="0">' +
        escapeHtml(name) + '</span>';
      panes += '<div class="tron-xlsx-sheet" data-sheet="' + count + '"' +
        (count === 0 ? '' : ' hidden') + '>' + table + '</div>';
      count++;
    }
    if (!count) {
      return '<div class="tron-office-error">此 Excel 檔沒有可顯示的工作表。請按上方「下載」查看。</div>';
    }
    return '<div class="tron-xlsx">' +
      (count > 1 ? '<div class="tron-xlsx-tabs">' + tabs + '</div>' : '') +
      panes + '</div>';
  }

  // ─────────────────────────────────────────────────────────────────────────
  // ODF（.odt / .ods / .odp）—— 同样是 zip + XML，不需要 LibreOffice
  // ─────────────────────────────────────────────────────────────────────────

  var NS_OFFICE = 'urn:oasis:names:tc:opendocument:xmlns:office:1.0';
  var NS_TEXT = 'urn:oasis:names:tc:opendocument:xmlns:text:1.0';
  var NS_TABLE = 'urn:oasis:names:tc:opendocument:xmlns:table:1.0';
  var NS_DRAW = 'urn:oasis:names:tc:opendocument:xmlns:drawing:1.0';

  function odfParagraphs(container) {
    if (!container) return '';
    var html = '';
    var ps = container.getElementsByTagNameNS(NS_TEXT, 'p');
    for (var i = 0; i < ps.length; i++) {
      var txt = (ps[i].textContent || '').replace(/\u00a0/g, ' ').trim();
      if (txt) html += '<p class="tron-odf-p">' + escapeHtml(txt) + '</p>';
    }
    return html;
  }

  async function renderOdf(arrayBuffer, ext) {
    var zip = await JSZip.loadAsync(arrayBuffer);
    var f = zip.file('content.xml');
    if (!f) {
      return '<div class="tron-office-error">此 ODF 檔缺少 content.xml，無法解析。請按上方「下載」查看。</div>';
    }
    var doc = xml(await f.async('text'));

    // .ods：table:table -> table:table-row -> table:table-cell
    if (ext === 'ods') {
      var tables = doc.getElementsByTagNameNS(NS_TABLE, 'table');
      var out = '';
      for (var t = 0; t < tables.length; t++) {
        var sheetName = attrLocal(tables[t], 'name') || ('工作表' + (t + 1));
        var rows = tables[t].getElementsByTagNameNS(NS_TABLE, 'table-row');
        var body = '';
        var emitted = 0;
        for (var r = 0; r < rows.length && emitted < XLSX_MAX_ROWS; r++) {
          var cells = rows[r].getElementsByTagNameNS(NS_TABLE, 'table-cell');
          var tds = '';
          var any = false;
          for (var c = 0; c < cells.length && c < XLSX_MAX_COLS; c++) {
            var repeat = parseInt(attrLocal(cells[c], 'number-columns-repeated'), 10) || 1;
            var txt = (cells[c].textContent || '').replace(/\u00a0/g, ' ').trim();
            if (txt) any = true;
            // 空白儲存格的 number-columns-repeated 可以是上萬，只展開到上限。
            var times = txt ? Math.min(repeat, 8) : Math.min(repeat, XLSX_MAX_COLS);
            for (var q = 0; q < times; q++) tds += '<td>' + escapeHtml(txt) + '</td>';
          }
          if (!any) continue;
          body += '<tr>' + tds + '</tr>';
          emitted++;
        }
        if (!body) continue;
        out += '<h3 class="tron-odf-sheet">' + escapeHtml(sheetName) + '</h3>' +
          '<table class="tron-xlsx-table"><tbody>' + body + '</tbody></table>';
      }
      return out ? '<div class="tron-odf">' + out + '</div>' : odfEmpty();
    }

    // .odp：draw:page -> draw:frame -> draw:text-box
    if (ext === 'odp') {
      var pages = doc.getElementsByTagNameNS(NS_DRAW, 'page');
      var slides = '';
      for (var p = 0; p < pages.length; p++) {
        var inner = odfParagraphs(pages[p]);
        if (!inner) continue;
        slides += '<div class="tron-slide"><div class="tron-odf-slide-body">' + inner + '</div></div>';
      }
      return slides ? '<div class="tron-slides">' + slides + '</div>' : odfEmpty();
    }

    // .odt（預設）：office:text 下的段落與標題
    var textBody = doc.getElementsByTagNameNS(NS_OFFICE, 'text')[0];
    var paragraphs = odfParagraphs(textBody || doc);
    return paragraphs ? '<div class="tron-docx tron-odf">' + paragraphs + '</div>' : odfEmpty();
  }

  function odfEmpty() {
    return '<div class="tron-office-error">此 ODF 檔沒有可顯示的文字內容。請按上方「下載」查看。</div>';
  }

  // ─────────────────────────────────────────────────────────────────────────
  // RTF —— 純文字格式，用控制字解析即可，不需要任何外部程式
  // ─────────────────────────────────────────────────────────────────────────

  // cp1252 的 0x80–0x9F 是排版符號，與 Unicode 不同碼；\'hh 逸出常落在這裡。
  var CP1252_HIGH = {
    128: 8364, 130: 8218, 131: 402, 132: 8222, 133: 8230, 134: 8224, 135: 8225,
    136: 710, 137: 8240, 138: 352, 139: 8249, 140: 338, 142: 381, 145: 8216,
    146: 8217, 147: 8220, 148: 8221, 149: 8226, 150: 8211, 151: 8212, 152: 732,
    153: 8482, 154: 353, 155: 8250, 156: 339, 158: 382, 159: 376
  };

  function cp1252Char(byte) {
    if (byte >= 128 && byte <= 159 && CP1252_HIGH[byte]) return String.fromCharCode(CP1252_HIGH[byte]);
    return String.fromCharCode(byte);
  }

  // 這些群組裝的是「文件結構資料」而非可見文字，必須整段跳過。
  // 例：{\fonttbl{\f0 MingLiU;}} 若不跳過，字型名會混進預覽文字裡。
  // 可忽略目的地 {\*\xxx} 另外由 \* 規則處理。
  var RTF_SKIP_DESTINATIONS = {
    fonttbl: 1, colortbl: 1, stylesheet: 1, listtable: 1, listoverridetable: 1,
    rsidtbl: 1, generator: 1, info: 1, themedata: 1, colorschememapping: 1,
    latentstyles: 1, datastore: 1, filetbl: 1, revtbl: 1, xmlnstbl: 1,
    pict: 1, shppict: 1, nonshppict: 1, blipuid: 1,
    header: 1, footer: 1, headerl: 1, headerr: 1, headerf: 1,
    footerl: 1, footerr: 1, footerf: 1,
    annotation: 1, atnid: 1, atnauthor: 1, atndate: 1,
    wgrffmtfilter: 1, sn: 1, sv: 1, mmathPr: 1
  };

  // 判斷 `{` 之後是不是「要整段跳過的目的地群組」。
  function isSkippedDestination(src, braceIndex) {
    var lookahead = src.substr(braceIndex + 1, 40);
    var optional = /^\\\*\\([a-zA-Z]+)/.exec(lookahead);
    if (optional) return true;
    var named = /^\\([a-zA-Z]+)/.exec(lookahead);
    return !!(named && RTF_SKIP_DESTINATIONS[named[1]]);
  }

  function renderRtf(arrayBuffer) {
    // RTF 的控制字全是 ASCII，先用 latin1 解出來再掃控制字；非 ASCII 內容一律
    // 由 \uN? 或 \'hh 逸出承載，所以這樣解不會遺失中文。
    var src = new TextDecoder('latin1').decode(arrayBuffer);
    var out = '';
    var skipDepth = 0;   // {\*...} 目的地（樣式表、字型表等）整段跳過
    var depth = 0;
    var i = 0;
    var guard = 0;

    while (i < src.length) {
      if (++guard > 2000000) break;   // 防禦：異常檔案不讓分頁凍住
      var ch = src[i];

      if (ch === '{') {
        depth++;
        // {\*\xxx} 可忽略目的地，以及 {\fonttbl ...} 等結構群組，整段跳過。
        if (!skipDepth && isSkippedDestination(src, i)) skipDepth = depth;
        i++;
        continue;
      }
      if (ch === '}') {
        if (skipDepth && depth === skipDepth) skipDepth = 0;
        depth--;
        i++;
        continue;
      }
      if (skipDepth) { i++; continue; }

      if (ch === '\r' || ch === '\n') { i++; continue; }

      if (ch !== '\\') { out += ch; i++; continue; }

      // \'hh —— 單 byte 逸出
      if (src[i + 1] === "'") {
        var hex = src.substr(i + 2, 2);
        var code = parseInt(hex, 16);
        if (!isNaN(code)) out += cp1252Char(code);
        i += 4;
        continue;
      }

      var word = /^\\([a-zA-Z]+)(-?\d+)?[ ]?/.exec(src.substr(i, 32));
      if (!word) {
        // \<符號> 是字面逸出，例如 \\ \{ \}
        var lit = src[i + 1];
        if (lit === '\\' || lit === '{' || lit === '}') out += lit;
        i += 2;
        continue;
      }

      var ctrl = word[1];
      var param = word[2] !== undefined ? parseInt(word[2], 10) : null;
      i += word[0].length;

      if (ctrl === 'u' && param !== null) {
        // \uN? —— N 是有號 16 位元；負值要 +65536。後面那個 ? 是替代字元，跳過。
        var cp = param < 0 ? param + 65536 : param;
        out += String.fromCharCode(cp);
        if (src[i] === '?' || src[i] === ' ') i++;
        continue;
      }
      if (ctrl === 'par' || ctrl === 'line' || ctrl === 'sect') { out += '\n'; continue; }
      if (ctrl === 'tab') { out += '\t'; continue; }
      if (ctrl === 'cell' || ctrl === 'nestcell') { out += '\t'; continue; }
      if (ctrl === 'row' || ctrl === 'nestrow') { out += '\n'; continue; }
      // 字型表／色彩表／樣式表的內容不需要，但那些群組已被 {\* 或控制字本身略過。
      continue;
    }

    var text = out.replace(/\r/g, '').replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
    if (!text) {
      return '<div class="tron-office-error">此 RTF 檔沒有可顯示的文字內容。請按上方「下載」查看。</div>';
    }
    return '<pre class="tron-rtf">' + escapeHtml(text) + '</pre>';
  }

  // OFFICE_LEGACY_V1: 舊二進位 OLE（.doc / .ppt / .xls）不是 zip、沒有可依賴的
  // 字元邊界，瀏覽器無法可靠解析其排版，因此退回「下載查看」。
  function legacyOfficeUnsupported() {
    return '<div class="tron-office-error">此為舊版二進位 Office 格式（.doc/.ppt/.xls），瀏覽器無法解析其排版。請按上方「下載」查看。</div>';
  }

  var api = {
    // 依副檔名分流。全部在瀏覽器本機解析，不需要 LibreOffice 或任何轉檔服務。
    //   docx           -> renderWord（OOXML Word）
    //   pptx           -> api.render（OOXML PowerPoint）
    //   xlsx           -> renderExcel（OOXML Excel）
    //   odt/ods/odp    -> renderOdf（OpenDocument）
    //   rtf            -> renderRtf（純文字控制字）
    //   doc/ppt/xls    -> 舊二進位，顯示「請下載」
    preview: async function (arrayBuffer, ext) {
      var e = String(ext || '').toLowerCase();
      // OFFICE_SNIFF_V1: zip 類 Office 先嗅探魔數。內容其實是 RTF 就直接走
      // RTF 渲染；是 OLE2/HTML/其他則給清楚說明，不把難懂的 JSZip 例外丟給使用者。
      if (e === 'docx' || e === 'pptx' || e === 'xlsx' ||
          e === 'odt' || e === 'ods' || e === 'odp') {
        var kind = officeSniff(arrayBuffer);
        if (kind === 'rtf') return renderRtf(arrayBuffer);
        if (kind !== 'zip') return nonZipOfficeMessage(kind, e);
      }
      if (e === 'docx') return renderWord(arrayBuffer);
      if (e === 'pptx') return api.render(arrayBuffer);
      if (e === 'xlsx') return renderExcel(arrayBuffer);
      if (e === 'odt' || e === 'ods' || e === 'odp') return renderOdf(arrayBuffer, e);
      if (e === 'rtf') return renderRtf(arrayBuffer);
      return legacyOfficeUnsupported();
    },

    // 診斷用：回傳魔數嗅探結果（'zip' | 'ole2' | 'html' | 'rtf' | 'unknown'）。
    sniff: function (arrayBuffer) { return officeSniff(arrayBuffer); },

    // 把 HTML 放進容器，並綁定 Excel 工作表分頁。
    // 刻意由本檔統一處理 innerHTML：這裡的賦值會走 Trusted Types 政策
    // （DOMPurify），而分頁切換必須在賦值「之後」才能抓到節點。
    mount: function (container, html) {
      container.innerHTML = html;
      var tabs = container.querySelectorAll('.tron-xlsx-tab');
      if (!tabs.length) return;
      function activate(index) {
        for (var i = 0; i < tabs.length; i++) {
          tabs[i].classList.toggle('active', tabs[i].getAttribute('data-sheet') === String(index));
        }
        var panes = container.querySelectorAll('.tron-xlsx-sheet');
        for (var p = 0; p < panes.length; p++) {
          if (panes[p].getAttribute('data-sheet') === String(index)) panes[p].removeAttribute('hidden');
          else panes[p].setAttribute('hidden', '');
        }
      }
      for (var t = 0; t < tabs.length; t++) {
        (function (tab) {
          var index = parseInt(tab.getAttribute('data-sheet'), 10) || 0;
          tab.addEventListener('click', function () { activate(index); });
          // 鍵盤可達性：分頁是 <span role="button">，Enter/Space 也要能切換。
          tab.addEventListener('keydown', function (ev) {
            if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); activate(index); }
          });
        })(tabs[t]);
      }
    },
    // 產生一個垂直堆疊的「投影片」；回傳 Promise<HTML 字串>（由呼叫端設定，走 innerHTML/trusted-types）。
    render: async function (arrayBuffer) {
      var zip = await JSZip.loadAsync(arrayBuffer);
      var presentation = await zipText(zip, 'ppt/presentation.xml');
      if (!presentation) {
        // 副檔名可能與內容不符：內容其實是 Word 就改走 Word 渲染。
        if (zip.file('word/document.xml')) return renderWord(arrayBuffer);
        // 否則給一個空的 presentation，讓下方「掃 ppt/slides」的備援路徑接手，
        // 不因缺 presentation.xml 就整份放棄。
        presentation = '<?xml version="1.0"?><p:presentation xmlns:p="' + NS_P + '"></p:presentation>';
      }
      var presXml = xml(presentation);
      var doc = buildSlideDocument(presXml, [], {});

      // 投影片順序（presentation.xml -> sldIdLst）
      var sldIdLst = presXml.getElementsByTagNameNS(
        NS_P, 'sldIdLst')[0];
      var slideIds = sldIdLst
        ? sldIdLst.getElementsByTagNameNS(NS_P, 'sldId')
        : [];

      // 由 presentation.xml.rels 拿到 rId -> slideN（rId 對應 sldId 的 r:id）。
      // OFFICE_ROBUST_V1: rels 缺失時退回「依序猜 slideN.xml」。
      var presRels = {};
      var presRelsText = await zipText(zip, 'ppt/_rels/presentation.xml.rels');
      if (presRelsText) {
        var presRelsDoc = xml(presRelsText);
        var rels = presRelsDoc.getElementsByTagName('Relationship');
        for (var i = 0; i < rels.length; i++) {
          var rel = rels[i];
          var target = rel.getAttribute('Target') || '';
          if (/^slides\/slide\.xml$/i.test(target) || /^slides\/slide\d+\.xml$/i.test(target)) {
            presRels[rel.getAttribute('Id')] = target;
          }
        }
      }

      var html = '';
      var count = 0;
      var slideErrors = [];
      for (var s = 0; s < slideIds.length; s++) {
        // OFFICE_ROBUST_V1: 單一投影片解析失敗（壞 XML、缺節點）只跳過那一頁，
        // 不讓整份簡報變成「無法解析」。
        try {
          var rid = slideIds[s].getAttributeNS(NS_R, 'id');
          var slidePath = presRels[rid];
          if (!slidePath) slidePath = 'ppt/slides/slide' + (s + 1) + '.xml';
          if (/^slides\//.test(slidePath)) slidePath = 'ppt/' + slidePath;
          var slideText = await zipText(zip, slidePath);
          if (!slideText) continue;
          var slideXml = xml(slideText);
          var cSld = slideXml.getElementsByTagNameNS(NS_P, 'cSld')[0];
          if (!cSld) continue;

          // 媒體對照
          var media = {};
          var relMap = await readSlideRels(zip, slidePath);
          var relNames = Object.keys(relMap);
          for (var mi = 0; mi < relNames.length; mi++) {
            var tgt = relMap[relNames[mi]];
            if (/^\.\.\/media\//.test(tgt) || /^media\//.test(tgt)) {
              var part = tgt.replace(/^\.\.\//, 'ppt/');
              if (!/^ppt\//.test(part)) part = 'ppt/' + tgt.replace(/^\.\.\//, '');
              if (part.indexOf('ppt/') !== 0) part = 'ppt/' + part.replace(/^ppt\/ppt\//, 'ppt/');
              var file = zip.file(part);
              if (file) media[relNames[mi]] = base64DataUri(await file.async('uint8array'));
            }
          }

          // 組合投影片
          var slideHtml = '';
          var spTree = cSld.getElementsByTagNameNS(NS_P, 'spTree')[0];
          var shapes = spTree ? spTree.getElementsByTagNameNS(NS_P, 'sp') : [];
          for (var sh = 0; sh < shapes.length; sh++) slideHtml += shapeToHtml(shapes[sh], {
            cx: doc.cx, cy: doc.cy, media: media
          });
          var pics = spTree ? spTree.getElementsByTagNameNS(NS_P, 'pic') : [];
          for (var pc = 0; pc < pics.length; pc++) slideHtml += shapeToHtml(pics[pc], {
            cx: doc.cx, cy: doc.cy, media: media
          });

          html += '<div class="tron-slide" style="aspect-ratio:' +
            (doc.cx / doc.cy).toFixed(4) + ';">' + slideHtml + '</div>';
          count++;
        } catch (slideErr) {
          // 記下被跳過的原因，count==0 時附在錯誤卡片裡送進後台日誌。
          slideErrors.push('slide: ' + (slideErr && slideErr.message));
          continue;
        }
      }

      // OFFICE_ROBUST_V1: 沒有 sldIdLst（或全數失敗）時，直接掃 ppt/slides/*.xml，
      // 讓「順序檔壞掉但投影片都在」的檔案至少能顯示內容。
      if (!count) {
        var names = [];
        zip.forEach(function (rel, entry) {
          if (/^ppt\/slides\/[^/]+\.xml$/.test(rel)) names.push(rel);
        });
        names.sort(function (a, b) {
          return parseInt(a.replace(/\D+/g, ''), 10) - parseInt(b.replace(/\D+/g, ''), 10);
        });
        for (var n = 0; n < names.length; n++) {
          try {
            var st = await zipText(zip, names[n]);
            if (!st) continue;
            var sx = xml(st);
            var cs = sx.getElementsByTagNameNS(NS_P, 'cSld')[0];
            if (!cs) continue;
            var tree = cs.getElementsByTagNameNS(NS_P, 'spTree')[0];
            var inner = '';
            var sps = tree ? tree.getElementsByTagNameNS(NS_P, 'sp') : [];
            for (var a2 = 0; a2 < sps.length; a2++) inner += shapeToHtml(sps[a2], { cx: doc.cx, cy: doc.cy, media: {} });
            if (inner) {
              html += '<div class="tron-slide" style="aspect-ratio:' + (doc.cx / doc.cy).toFixed(4) + ';">' + inner + '</div>';
              count++;
            }
          } catch (e2) { slideErrors.push('fallback: ' + (e2 && e2.message)); continue; }
        }
      }
      // LAST_RESORT_TEXT_V1: 若 DOMParser 路徑全數失敗（例如瀏覽器對這份 XML
      // 回 parsererror），改用 regex 直接抓 <a:t> 文字，保證「至少看得到內容」，
      // 而不是丟「無法解析」。空白頁也渲染一個框，避免整份被判為失敗。
      if (!count) {
        var rnames = [];
        zip.forEach(function (rel) { if (/^ppt\/slides\/[^/]+\.xml$/.test(rel)) rnames.push(rel); });
        rnames.sort(function (a, b) {
          return parseInt(a.replace(/\D+/g, ''), 10) - parseInt(b.replace(/\D+/g, ''), 10);
        });
        for (var r = 0; r < rnames.length; r++) {
          try {
            var rt = await zipText(zip, rnames[r]);
            if (!rt) continue;
            var texts = [], m, re = /<a:t[^>]*>([\s\S]*?)<\/a:t>/g;
            while ((m = re.exec(rt)) !== null) { if (m[1]) texts.push(m[1]); }
            var body = texts.length
              ? texts.map(function (t) { return '<p style="margin:0 0 6px;">' + t + '</p>'; }).join('')
              : '<p style="margin:0;opacity:.6;">（此頁無可解析文字）</p>';
            html += '<div class="tron-slide" style="aspect-ratio:' + (doc.cx / doc.cy).toFixed(4) +
              ';padding:24px;box-sizing:border-box;overflow:auto;">' + body + '</div>';
            count++;
          } catch (e3) { slideErrors.push('lastresort: ' + (e3 && e3.message)); }
        }
      }
      if (!count) {
        var why = slideErrors.length ? '（內部例外：' + slideErrors[0] + '）' : '';
        return '<div class="tron-office-error">此簡報無法解析' + why + '。請按上方「下載」查看。</div>';
      }
      return '<div class="tron-slides">' + html + '</div>';
    }
  };

  window.TronOfficeView = api;
})();
