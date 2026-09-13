/* DOM XSS hardening for the legacy dashboard. */
(function () {
  'use strict';

  // TT_DOMPURIFY_FALLBACK_V1：若 DOMPurify 因網路／快取問題未能載入，
  // 舊邏輯會直接 return，導致沒有 default policy —— 頁面上所有 innerHTML
  // 都會拋 TypeError（TrustedHTML assignment），整頁功能掛掉（實測案例：
  // /admin/timetable 課表頁全白）。這裡改為：DOMPurify 缺席時仍建立
  // default policy，先以 pass-through 模式讓頁面活著（與 files.html、
  // shares-admin.js 既有 identity policy 做法一致），並留 console 警告
  // 供診斷。DOMPurify 正常載入時行為完全不變（照舊消毒）。
  if (!window.trustedTypes) return;
  if (window.trustedTypes.defaultPolicy) return;

  var dompurifyMissing = !window.DOMPurify;

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

    // Fallback（DOMPurify 未載入）：pass-through，頁面保持可用。
    return html;
  }

  window.trustedTypes.createPolicy('default', {
    createHTML: function (value) {
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
    console.warn('[trusted-types-init] DOMPurify 未載入，default policy 以 pass-through 模式運作（頁面可用，但此次載入未經 HTML 消毒）');
  }
})();
