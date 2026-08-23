/* DOM XSS hardening for the legacy dashboard. */
(function () {
  'use strict';

  if (!window.trustedTypes || !window.DOMPurify) return;
  if (window.trustedTypes.defaultPolicy) return;

  function sanitizeHtml(value) {
    var html = String(value);

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
})();
