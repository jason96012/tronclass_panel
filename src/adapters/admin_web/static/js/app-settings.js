/* APP_SETTINGS_BUILD: 20260829settings9 — fix14 (測試通知卡改內嵌 settings.html 並移至外觀卡之後) */
 /* APP_SETTINGS_V1 — 全站外觀與本地偏好設定
 *
 * 這支腳本被 inject_theme() 放進每個頁面 <head> 的最前面（早於 theme.css），
 * 職責：
 * 1) 在 body 渲染前讀取 localStorage 偏好，把 data-theme / data-accent /
 *    data-fontscale / data-motion 掛到 <html>，避免主題閃爍（FOUC）。
 * 2) 「跟隨系統」模式監聽 prefers-color-scheme 變化，即時切換深淺色。
 * 3) 其他分頁變更設定時，透過 storage 事件同步到目前分頁。
 * 4) 注入「設定」入口：僅存在於 Dashboard 分頁列（緊鄰「通知」分頁）與
 *    手機「更多」面板。所有頁面（含課表／作業／檔案）的右上角 header
 *    一律不加入設定按鈕。
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

  // ── 注入「設定」入口 ─────────────────────────────────────────────────────
  // 僅 Dashboard：分頁列（.tabs）「維護」分頁前（通知分頁已改為獨立頁面）。
  // 手機版（≤820px）分頁列隱藏，改在「更多」面板（#mobileMoreFinalV2）補入口，
  // 該面板由 dashboard JS 動態建立，以輪詢等它出現。
  // 其他頁面（課表／作業／檔案…）的 header 一律不加設定按鈕。
  var SHEET_POLL_MAX = 30;   // 30 × 400ms ≈ 12 秒（等手機更多面板）

  function buildGearTab() {
    var b = document.createElement('button');
    b.id = 'appSettingsGearTab';
    b.type = 'button';
    b.className = 'tab app-settings-tab';
    b.setAttribute('aria-label', '設定');
    b.textContent = '設定';
    b.addEventListener('click', function () { location.href = '/admin/settings'; });
    return b;
  }

  function startGearTabFlow(tabs) {
    if (document.getElementById('appSettingsGearTab')) return;
    var gear = buildGearTab();
    // NOTIFICATIONS_STANDALONE_PAGE_V1: 通知分頁已移除，設定分頁錨定在「維護」前。
    var ref = tabs.querySelector('.tab[data-view="maintenance"]');
    if (ref) tabs.insertBefore(gear, ref); else tabs.appendChild(gear);
  }

  function injectMobileSheetEntry() {
    if (document.getElementById('moreSettingsFinalV2')) return true;
    var grid = document.querySelector('#mobileMoreFinalV2 .more-grid') ||
               document.querySelector('#mobileSheet .sheet-grid');
    if (!grid) return false;
    var a = document.createElement('a');
    a.id = 'moreSettingsFinalV2';
    a.className = 'button secondary';
    a.href = '/admin/settings';
    a.textContent = '外觀設定';
    grid.insertBefore(a, grid.firstChild);
    return true;
  }

  function startMobileSheetFlow() {
    if (injectMobileSheetEntry()) return;
    var polls = 0;
    var timer = setInterval(function () {
      polls += 1;
      if (injectMobileSheetEntry() || polls >= SHEET_POLL_MAX) clearInterval(timer);
    }, 400);
  }

  function injectGear() {
    if (document.getElementById('appSettingsRoot')) return; // 設定頁本身不需要
    // SETTINGS_GEAR_DASHBOARD_ONLY_V1: 設定齒輪只出現在 Dashboard；
    // 其他頁面（如 /admin/activity 也有 .tabs）不再誤植「設定」分頁鈕。
    var p = location.pathname;
    if (p !== '/admin' && p !== '/admin/' && p !== '/admin/dashboard') return;
    var tabs = document.querySelector('.tabs');
    if (!tabs) return; // 非 Dashboard 頁面：右上角不加入任何設定按鈕
    startGearTabFlow(tabs);
    startMobileSheetFlow();
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
        ? '切換淺色'
        : '切換深色';
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
    /* LOGIN_PERF_V1：/admin/settings 由伺服器驗證 session 後才送出（<html data-auth="ok">），不必再打一次。 */
    try { if (document.documentElement.getAttribute('data-auth') === 'ok') return; } catch (_) { }
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
