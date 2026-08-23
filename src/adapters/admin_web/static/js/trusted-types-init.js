/* DOM XSS hardening for the legacy dashboard. */
(function () {
  'use strict';

  if (!window.trustedTypes || !window.DOMPurify) return;
  if (window.trustedTypes.defaultPolicy) return;

  window.trustedTypes.createPolicy('default', {
    createHTML: function (value) {
      return window.DOMPurify.sanitize(String(value), {
        RETURN_TRUSTED_TYPE: false,
      });
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
