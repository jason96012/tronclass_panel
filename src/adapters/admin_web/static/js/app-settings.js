/* APP_SETTINGS_V1 — 全站外觀與本地偏好設定
 *
 * 這支腳本被 inject_theme() 放進每個頁面 <head> 的最前面（早於 theme.css），
 * 職責：
 * 1) 在 body 渲染前讀取 localStorage 偏好，把 data-theme / data-accent /
 *    data-fontscale / data-motion 掛到 <html>，避免主題閃爍（FOUC）。
 * 2) 「跟隨系統」模式監聽 prefers-color-scheme 變化，即時切換深淺色。
 * 3) 其他分頁變更設定時，透過 storage 事件同步到目前分頁。
 * 4) 在有 .top-actions 的頁面注入「⚙️ 設定」入口（/admin/settings）。
 * 5) 在 /admin/settings 頁面（有 #appSettingsRoot）掛上互動邏輯。
 *
 * 對應的 CSS 覆蓋規則在 pages.rs 的 APP_SETTINGS_DYNAMIC_CSS（隨 theme.css
 * 之後注入每個頁面），兩者透過 <html data-app-settings> 屬性連動。
 */
(function () {
  'use strict';
  if (window.TronclassAppSettings) return;

  var KEY = 'tronclass_app_settings_v1';
  var DEFAULTS = { theme: 'auto', accent: 'blue', fontScale: 'std', motion: 'normal' };
  var THEMES = ['auto', 'light', 'dark'];
  var ACCENTS = ['blue', 'indigo', 'green', 'amber', 'purple', 'rose', 'cyan'];
  var FONT_SCALES = ['std', 'lg', 'xl'];
  var MOTIONS = ['normal', 'reduced'];

  // localStorage 在私密瀏覽模式／沙箱環境可能直接拋錯，退回記憶體儲存。
  var memoryFallback = null;
  function storeGet() {
    try { return localStorage.getItem(KEY); } catch (_) { return memoryFallback; }
  }
  function storePut(value) {
    try { localStorage.setItem(KEY, value); } catch (_) { memoryFallback = value; }
  }

  function pick(value, allowed, fallback) {
    return allowed.indexOf(value) >= 0 ? value : fallback;
  }

  function sanitize(raw) {
    var out = {
      theme: 'auto',
      accent: 'blue',
      fontScale: 'std',
      motion: 'normal'
    };
    if (raw && typeof raw === 'object') {
      out.theme = pick(raw.theme, THEMES, 'auto');
      out.accent = pick(raw.accent, ACCENTS, 'blue');
      out.fontScale = pick(raw.fontScale, FONT_SCALES, 'std');
      out.motion = pick(raw.motion, MOTIONS, 'normal');
    }
    return out;
  }

  function load() {
    var raw = storeGet();
    if (!raw) return sanitize(null);
    try { return sanitize(JSON.parse(raw)); } catch (_) { return sanitize(null); }
  }

  var darkMedia = window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
  function resolvedTheme(settings) {
    if (settings.theme === 'dark') return 'dark';
    if (settings.theme === 'light') return 'light';
    return (darkMedia && darkMedia.matches) ? 'dark' : 'light';
  }

  function apply(settings) {
    var root = document.documentElement;
    root.setAttribute('data-app-settings', '1');
    root.setAttribute('data-theme', resolvedTheme(settings));
    root.setAttribute('data-accent', String(settings.accent || 'blue'));
    root.setAttribute('data-fontscale', settings.fontScale || 'std');
    root.setAttribute('data-motion', settings.motion || 'normal');
  }

  var current = load();
  // 盡早套用：此時 body 尚未解析，不會出現深淺色閃爍。
  apply(current);

  if (darkMedia) {
    var onSchemeChange = function () {
      if (current.theme === 'auto') apply(current);
    };
    if (darkMedia.addEventListener) darkMedia.addEventListener('change', onSchemeChange);
    else if (darkMedia.addListener) darkMedia.addListener(onSchemeChange);
  }

  window.addEventListener('storage', function (event) {
    if (event.key === KEY || event.key === null) {
      current = load();
      apply(current);
      if (typeof refreshSettingsUi === 'function') refreshSettingsUi();
    }
  });

  function persist() {
    storePut(JSON.stringify(current));
    apply(current);
  }

  window.TronclassAppSettings = {
    get: function () { return Object.assign({}, current); },
    set: function (patch) {
      current = sanitize(Object.assign({}, current, patch || {}));
      persist();
    },
    reset: function () {
      current = sanitize(null);
      persist();
    },
    resolvedTheme: function () { return resolvedTheme(current); }
  };

  // ── 各頁面 header 注入「設定」入口 ────────────────────────────────────────
  function injectGear() {
    if (document.getElementById('appSettingsRoot')) return; // 設定頁本身不需要
    if (document.getElementById('appSettingsGearLink')) return;
    var actions = document.querySelector('.top-actions');
    if (!actions) return;
    var link = document.createElement('a');
    link.id = 'appSettingsGearLink';
    link.className = 'app-settings-gear';
    link.href = '/admin/settings';
    link.setAttribute('aria-label', '外觀與設定');
    link.textContent = '⚙️ 設定';
    actions.appendChild(link);
  }

  // ── /admin/settings 頁面互動 ─────────────────────────────────────────────
  function markActive() {
    var settings = window.TronclassAppSettings.get();
    var themeButtons = document.querySelectorAll('[data-set-theme]');
    for (var i = 0; i < themeButtons.length; i++) {
      var active = themeButtons[i].getAttribute('data-set-theme') === settings.theme;
      themeButtons[i].classList.toggle('active', active);
      themeButtons[i].setAttribute('aria-pressed', active ? 'true' : 'false');
    }
    var accentButtons = document.querySelectorAll('[data-set-accent]');
    for (var j = 0; j < accentButtons.length; j++) {
      var accentActive = accentButtons[j].getAttribute('data-set-accent') === settings.accent;
      accentButtons[j].classList.toggle('active', accentActive);
      accentButtons[j].setAttribute('aria-pressed', accentActive ? 'true' : 'false');
    }
    var fontButtons = document.querySelectorAll('[data-set-fontscale]');
    for (var k = 0; k < fontButtons.length; k++) {
      var fontActive = fontButtons[k].getAttribute('data-set-fontscale') === settings.fontScale;
      fontButtons[k].classList.toggle('active', fontActive);
      fontButtons[k].setAttribute('aria-pressed', fontActive ? 'true' : 'false');
    }
    var motionToggle = document.getElementById('motionToggle');
    if (motionToggle) motionToggle.checked = settings.motion === 'reduced';
    var quickToggle = document.getElementById('quickThemeToggle');
    if (quickToggle) {
      quickToggle.textContent = window.TronclassAppSettings.resolvedTheme() === 'dark'
        ? '☀️ 切換淺色'
        : '🌙 切換深色';
    }
  }

  function refreshSettingsUi() {
    if (!document.getElementById('appSettingsRoot')) return;
    markActive();
  }
  window.TronclassAppSettings.refreshUi = refreshSettingsUi;

  function wireSettingsPage() {
    var root = document.getElementById('appSettingsRoot');
    if (!root) return;

    var themeButtons = root.querySelectorAll('[data-set-theme]');
    for (var i = 0; i < themeButtons.length; i++) {
      themeButtons[i].addEventListener('click', function () {
        window.TronclassAppSettings.set({ theme: this.getAttribute('data-set-theme') });
        markActive();
      });
    }

    var accentButtons = root.querySelectorAll('[data-set-accent]');
    for (var j = 0; j < accentButtons.length; j++) {
      accentButtons[j].addEventListener('click', function () {
        window.TronclassAppSettings.set({ accent: this.getAttribute('data-set-accent') });
        markActive();
      });
    }

    var fontButtons = root.querySelectorAll('[data-set-fontscale]');
    for (var k = 0; k < fontButtons.length; k++) {
      fontButtons[k].addEventListener('click', function () {
        window.TronclassAppSettings.set({ fontScale: this.getAttribute('data-set-fontscale') });
        markActive();
      });
    }

    var motionToggle = document.getElementById('motionToggle');
    if (motionToggle) {
      motionToggle.addEventListener('change', function () {
        window.TronclassAppSettings.set({ motion: motionToggle.checked ? 'reduced' : 'normal' });
        markActive();
      });
    }

    var quickToggle = document.getElementById('quickThemeToggle');
    if (quickToggle) {
      quickToggle.addEventListener('click', function () {
        var next = window.TronclassAppSettings.resolvedTheme() === 'dark' ? 'light' : 'dark';
        window.TronclassAppSettings.set({ theme: next });
        markActive();
      });
    }

    var resetButton = document.getElementById('resetBtn');
    if (resetButton) {
      resetButton.addEventListener('click', function () {
        window.TronclassAppSettings.reset();
        markActive();
      });
    }

    markActive();
    verifySession();
  }

  // 與 password.html 相同的登入檢查：未登入就導回登入頁。
  // fetch 失敗（離線／靜態預覽）時留在原頁面，設定仍可預覽。
  function verifySession() {
    try {
      fetch('/admin/api/status', {
        credentials: 'same-origin',
        cache: 'no-store',
        headers: { 'Accept': 'application/json' }
      }).then(function (res) {
        if (res.status === 401 || res.status === 403) {
          location.replace('/admin?next=' + encodeURIComponent('/admin/settings'));
        }
      }).catch(function () { /* 無法驗證：保持原頁 */ });
    } catch (_) { /* 忽略 */ }
  }

  function init() {
    try { injectGear(); } catch (_) { }
    try { wireSettingsPage(); } catch (_) { }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
