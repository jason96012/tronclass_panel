/* DOM XSS hardening for the legacy dashboard. */
(function () {
  'use strict';

  // TT_DOMPURIFY_FALLBACK_V1：若 DOMPurify 因網路／快取問題未能載入，
  // 舊邏輯會直接 return，導致沒有 default policy —— 頁面上所有 innerHTML
  // 都會拋 TypeError（TrustedHTML assignment），整頁功能掛掉（實測案例：
  // /admin/timetable 課表頁全白）。因此 DOMPurify 缺席時仍建立 default
  // policy，但不再原樣放行（M1 修正）：改用下方 TT_MIN_SANITIZER_V1 的
  // 白名單消毒器。DOMPurify 正常載入時行為完全不變（照舊消毒）。
  if (!window.trustedTypes) return;
  if (window.trustedTypes.defaultPolicy) return;

  var dompurifyMissing = !window.DOMPurify;

  // TT_MIN_SANITIZER_V1：DOMPurify 缺席時的最小白名單消毒器。
  // 目標是「頁面可用且不執行任意腳本」，不是完整取代 DOMPurify：
  //   - 整個移除可執行／可載入外部資源的元素（script、iframe、object、
  //     embed、style、link、meta、base、svg 內的 foreignObject／animate／use…）
  //   - 移除所有 on* 事件屬性與 srcdoc
  //   - URL 類屬性（href、src、action、formaction…）拒絕 javascript:、
  //     vbscript: 與 data:（img src 的 data:image/png|jpeg|gif|webp 例外）
  //   - style 屬性拒絕 expression(、javascript:、@import 等
  //   - 註解、CDATA、processing instruction 一律移除；template 內容一併清理
  // 解析在 document.implementation.createHTMLDocument 的惰性文件中進行，
  // 不執行腳本、不觸發 onerror。那次解析的 innerHTML 會再經過本 policy，
  // 因此以 ttInSanitizer 旗標放行，避免無限遞迴（輸出仍是清理後的 DOM 序列化結果）。
  var TT_DROP_TAGS = {
    script: 1, iframe: 1, frame: 1, frameset: 1, object: 1, embed: 1, applet: 1,
    base: 1, link: 1, meta: 1, style: 1, noscript: 1, xml: 1, foreignobject: 1,
    animate: 1, animatemotion: 1, animatetransform: 1, set: 1, use: 1
  };
  var TT_URL_ATTRS = {
    href: 1, src: 1, 'xlink:href': 1, action: 1, formaction: 1, data: 1,
    poster: 1, background: 1, codebase: 1
  };
  var TT_DATA_IMG_RE = /^data:image\/(png|jpe?g|gif|webp);/;
  var ttInSanitizer = false;

  function ttAttrOk(tag, attr) {
    var name = attr.name.toLowerCase();
    var value = attr.value.replace(/[\u0000-\u0020\u007f-\u009f]/g, '').toLowerCase();
    if (name.indexOf('on') === 0 || name === 'srcdoc') return false;
    if (TT_URL_ATTRS[name]) {
      if (/^(javascript|vbscript):/.test(value)) return false;
      if (value.indexOf('data:') === 0 &&
          !(tag === 'img' && name === 'src' && TT_DATA_IMG_RE.test(value))) return false;
    }
    if (name === 'style' && /expression\(|javascript:|vbscript:|@import|behavior:|-moz-binding/.test(value)) return false;
    return true;
  }

  function ttCleanNode(parent) {
    var kids = Array.prototype.slice.call(parent.childNodes);
    for (var i = 0; i < kids.length; i++) {
      var c = kids[i];
      if (c.nodeType !== 1) {
        if (c.nodeType !== 3) parent.removeChild(c); // 保留文字節點，其餘（註解等）移除
        continue;
      }
      var tag = String(c.localName || c.nodeName).toLowerCase();
      if (TT_DROP_TAGS[tag]) { parent.removeChild(c); continue; }
      var attrs = Array.prototype.slice.call(c.attributes);
      for (var j = 0; j < attrs.length; j++) {
        if (!ttAttrOk(tag, attrs[j])) c.removeAttributeNode(attrs[j]);
      }
      if (tag === 'template' && c.content) ttCleanNode(c.content);
      ttCleanNode(c);
    }
  }

  function ttMinSanitize(html) {
    var doc = document.implementation.createHTMLDocument('');
    var box = doc.createElement('div');
    // 與 DOMPurify 分支相同：裸 <tr> 片段必須先包在 table/tbody 裡解析，否則會被丟棄。
    var isRow = /^\s*<tr(?:\s|>)/i.test(html);
    ttInSanitizer = true;
    try {
      box.innerHTML = isRow ? '<table><tbody>' + html + '</tbody></table>' : html;
    } finally {
      ttInSanitizer = false;
    }
    var root = isRow ? box.querySelector('tbody') : box;
    if (!root) return '';
    ttCleanNode(root);
    return root.innerHTML;
  }

  function sanitizeHtml(value) {
    var html = String(value);

    if (!dompurifyMissing) {
      // DOMPurify parses a bare <tr> fragment in a <body> context. The HTML
      // parser then drops the table row/cell elements, so assigning the result
      // to <tbody>.innerHTML turns the desktop account/activity tables into a
      // mobile-looking stream of stacked elements. Parse row fragments inside a
      // valid table first, then return only the sanitized rows.
      if (/^\s*<tr(?:\s|>)/i.test(html)) {
        var wrapped = '<table><tbody>' + html + '</tbody></table>';
        var cleanTable = window.DOMPurify.sanitize(wrapped, {
          RETURN_TRUSTED_TYPE: false,
        });
        var match = cleanTable.match(/<table[^>]*>\s*<tbody[^>]*>([\s\S]*?)<\/tbody>\s*<\/table>/i);
        if (match) return match[1];
      }

      return window.DOMPurify.sanitize(html, {
        RETURN_TRUSTED_TYPE: false,
      });
    }

    // Fallback（DOMPurify 未載入）：最小白名單消毒器（見 TT_MIN_SANITIZER_V1）。
    return ttMinSanitize(html);
  }

  window.trustedTypes.createPolicy('default', {
    createHTML: function (value) {
      // 消毒器自己的惰性解析會再進入這裡：原樣放行（輸出會再被清理）。
      if (ttInSanitizer) return String(value);
      return sanitizeHtml(value);
    },
    createScriptURL: function (value) {
      var url = new URL(String(value), window.location.origin);
      if (url.origin === window.location.origin ||
          url.origin === 'https://static.cloudflareinsights.com') {
        return url.href;
      }
      throw new TypeError('Untrusted script URL blocked');
    },
  });

  if (dompurifyMissing) {
    console.warn('[trusted-types-init] DOMPurify 未載入，default policy 改用最小白名單消毒器（頁面可用；消毒範圍較 DOMPurify 窄）');
  }
})();
