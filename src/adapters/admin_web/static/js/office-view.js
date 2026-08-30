/*
 * TRONCLASS OFFICE VIEW V1
 * 雲端硬碟的 Office 預覽：不依賴 LibreOffice / 轉檔服務。
 *   - Word (.docx)  -> docx-preview.min.js（同源載入，UMD 全域 docx）
 *   - PowerPoint (.pptx) -> 本檔自製無依賴解析器（只仰賴同源的 JSZip 全域）
 * 這個「本地、同源、純前端」的作法符合檔案頁的嚴格 CSP（script-src 'self'），
 * 資料不離開使用者瀏覽器，也不佔伺服器磁碟。
 * 僅支援 OOXML（.docx / .pptx）；舊二進位 .doc / .ppt 無法在瀏覽器解析，
 * 會退回「下載查看」。
 */
(function () {
  'use strict';

  // EMU = 914400 per inch。把投影片縮放到容器寬度內，維持 16:9/4:3 比例。
  function buildSlideDocument(presentationXml, slides, media) {
    var size = presentationXml.getElementsByTagNameNS(
      'http://schemas.openxmlformats.org/presentationml/2006/main', 'sldSz'
    )[0];
    var cx = parseInt(size.getAttribute('cx'), 10) || 9144000;
    var cy = parseInt(size.getAttribute('cy'), 10) || 6858000;
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
    return new DOMParser().parseFromString(str, 'application/xml');
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

  var W_NS = 'http://schemas.openxmlformats.org/wordprocessingml/2006/main';
  var W_R_NS = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships';

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
    if (rPr.getElementsByTagNameNS(W_NS, 'b')[0]) st.fontWeight = 'bold';
    if (rPr.getElementsByTagNameNS(W_NS, 'i')[0]) st.fontStyle = 'italic';
    if (rPr.getElementsByTagNameNS(W_NS, 'u')[0]) st.textDecoration = 'underline';
    var color = rPr.getElementsByTagNameNS(W_NS, 'color')[0];
    var colorVal = color && attrLocal(color, 'val');
    if (colorVal && colorVal !== 'auto') {
      st.color = '#' + colorVal;
    }
    var sz = rPr.getElementsByTagNameNS(W_NS, 'sz')[0];
    var szV = sz && parseInt(attrLocal(sz, 'val'), 10);
    if (!isNaN(szV) && szV > 0) st.fontSize = (szV / 2) + 'pt'; // 單位為半 pt
    return st;
  }

  function docxParaHtml(p, media) {
    var pPr = p.getElementsByTagNameNS(W_NS, 'pPr')[0];
    var styleName = '';
    var pstyle = '';
    if (pPr) {
      var pStyle = pPr.getElementsByTagNameNS(W_NS, 'pStyle')[0];
      if (pStyle) styleName = attrLocal(pStyle, 'val');
      var jc = pPr.getElementsByTagNameNS(W_NS, 'jc')[0];
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
    var runs = p.getElementsByTagNameNS(W_NS, 'r');
    for (var i = 0; i < runs.length; i++) {
      var rPr = runs[i].getElementsByTagNameNS(W_NS, 'rPr')[0];
      var st = docxRunStyle(rPr);
      var t = runs[i].getElementsByTagNameNS(W_NS, 't')[0];
      if (t) {
        var txt = (t.textContent || '').replace(/\u00a0/g, ' ');
        if (i === 0) txt = txt.replace(/^[\s\u2022]+/, ''); // 避免標題已含 • 又再加上
        html += '<span style="' + styleAttr(st) + '">' + escapeHtml(txt) + '</span>';
      }
      // 內嵌圖片
      var drawing = runs[i].getElementsByTagNameNS(W_NS, 'drawing')[0];
      if (drawing) {
        var blip = drawing.getElementsByTagNameNS(NS_A, 'blip')[0];
        if (blip) {
          var relId = blip.getAttributeNS(W_R_NS, 'embed');
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
    var html = '';
    var ps = tc.getElementsByTagNameNS(W_NS, 'p');
    for (var pi = 0; pi < ps.length; pi++) {
      html += docxParaHtml(ps[pi], media);
    }
    if (!html) html = '&nbsp;';
    return '<td style="border:1px solid #cbd2e0;padding:6px 8px;vertical-align:top;">' + html + '</td>';
  }

  function docxTableHtml(tbl, media) {
    var rows = tbl.getElementsByTagNameNS(W_NS, 'tr');
    var html = '<table style="border-collapse:collapse;width:100%;margin:8px 0;"><tbody>';
    for (var r = 0; r < rows.length; r++) {
      var cells = rows[r].getElementsByTagNameNS(W_NS, 'tc');
      html += '<tr>';
      for (var c = 0; c < cells.length; c++) html += docxCellHtml(cells[c], media);
      html += '</tr>';
    }
    html += '</tbody></table>';
    return html;
  }

  async function renderWord(arrayBuffer) {
    var zip = await JSZip.loadAsync(arrayBuffer);
    var docText = await zip.file('word/document.xml').async('text');
    var docXml = xml(docText);
    var body = docXml.getElementsByTagNameNS(W_NS, 'body')[0];

    var media = {};
    var relDoc = xml(await zip.file('word/_rels/document.xml.rels').async('text'));
    var rels = relDoc.getElementsByTagName('Relationship');
    for (var i = 0; i < rels.length; i++) {
      var rel = rels[i];
      var tgt = rel.getAttribute('Target') || '';
      if (/^media\//.test(tgt)) {
        var part = 'word/' + tgt;
        var file = zip.file(part);
        if (file) media[rel.getAttribute('Id')] = base64DataUri(await file.async('uint8array'));
      }
    }

    var html = '';
    var children = body.childNodes;
    for (var c = 0; c < children.length; c++) {
      var node = children[c];
      if (node.nodeType !== 1) continue;
      if (node.localName === 'p') html += docxParaHtml(node, media);
      else if (node.localName === 'tbl') html += docxTableHtml(node, media);
    }
    if (!html) {
      return '<div class="tron-office-error">此 Word 檔無法解析（可能是舊版 .doc 二進位格式）。請按上方「下載」查看。</div>';
    }
    return '<div class="tron-docx">' + html + '</div>';
  }

  var api = {
    // 依副檔名分流：docx -> renderWord；pptx/ppt -> render（自製）。
    preview: async function (arrayBuffer, ext) {
      if (/^docx$/i.test(ext || '')) return renderWord(arrayBuffer);
      return api.render(arrayBuffer);
    },
    // 產生一個垂直堆疊的「投影片」；回傳 Promise<HTML 字串>（由呼叫端設定，走 innerHTML/trusted-types）。
    render: async function (arrayBuffer) {
      var zip = await JSZip.loadAsync(arrayBuffer);
      var presentation = await zip.file('ppt/presentation.xml').async('text');
      var presXml = xml(presentation);
      var doc = buildSlideDocument(presXml, [], {});

      // 投影片順序（presentation.xml -> sldIdLst）
      var sldIdLst = presXml.getElementsByTagNameNS(
        NS_P, 'sldIdLst')[0];
      var slideIds = sldIdLst
        ? sldIdLst.getElementsByTagNameNS(NS_P, 'sldId')
        : [];

      // 由 presentation.xml.rels 拿到 rId -> slideN（rId 對應 sldId 的 r:id）
      var presRelsText = await zip.file('ppt/_rels/presentation.xml.rels').async('text');
      var presRels = {};
      var presRelsDoc = xml(presRelsText);
      var rels = presRelsDoc.getElementsByTagName('Relationship');
      for (var i = 0; i < rels.length; i++) {
        var rel = rels[i];
        var target = rel.getAttribute('Target') || '';
        if (/^slides\/slide\.xml$/i.test(target) || /^slides\/slide\d+\.xml$/i.test(target)) {
          presRels[rel.getAttribute('Id')] = target;
        }
      }

      var html = '';
      var count = 0;
      for (var s = 0; s < slideIds.length; s++) {
        var rid = slideIds[s].getAttributeNS(NS_R, 'id');
        var slidePath = presRels[rid];
        if (!slidePath) slidePath = 'ppt/slides/slide' + (s + 1) + '.xml';
        if (/^slides\//.test(slidePath)) slidePath = 'ppt/' + slidePath;
        var slideText = zip.file(slidePath) && await zip.file(slidePath).async('text');
        if (!slideText) continue;
        var slideXml = xml(slideText);
        var cSld = slideXml.getElementsByTagNameNS(NS_P, 'cSld')[0];

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
      }
      if (!count) {
        return '<div class="tron-office-error">此簡報無法解析（可能是舊版 .ppt 二進位格式）。請按上方「下載」查看。</div>';
      }
      return '<div class="tron-slides">' + html + '</div>';
    }
  };

  window.TronOfficeView = api;
})();
