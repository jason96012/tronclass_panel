/**
 * 鍵盤快捷鍵模組 - 全域熱鍵管理
 */

export function createKeyboardManager() {
  const shortcuts = new Map();
  const chordBuffer = [];
  let chordTimeout = null;

  function normalizeKey(event) {
    const parts = [];
    if (event.ctrlKey || event.metaKey) parts.push('mod');
    if (event.shiftKey) parts.push('shift');
    if (event.altKey) parts.push('alt');
    if (event.key.length === 1) parts.push(event.key.toLowerCase());
    else parts.push(event.key.toLowerCase());
    return parts.join('+');
  }

  function register(key, handler, options = {}) {
    const { preventDefault = true, stopPropagation = false, description = '' } = options;
    const normalized = key.toLowerCase();
    if (!shortcuts.has(normalized)) shortcuts.set(normalized, []);
    shortcuts.get(normalized).push({ handler, preventDefault, stopPropagation, description });
    return () => unregister(normalized, handler);
  }

  function unregister(key, handler) {
    const normalized = key.toLowerCase();
    const list = shortcuts.get(normalized);
    if (list) {
      const idx = list.findIndex(s => s.handler === handler);
      if (idx >= 0) list.splice(idx, 1);
      if (!list.length) shortcuts.delete(normalized);
    }
  }

  function handleKeyDown(event) {
    // 忽略輸入框中的按鍵（除了特定快捷鍵）
    const target = event.target;
    const isInput = target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target.isContentEditable;
    const isComposing = event.isComposing;

    const key = normalizeKey(event);
    const list = shortcuts.get(key);
    if (list) {
      for (const { handler, preventDefault, stopPropagation } of list) {
        if (isInput && !handler.allowInInput) continue;
        if (preventDefault) event.preventDefault();
        if (stopPropagation) event.stopPropagation();
        handler(event);
      }
    }
  }

  function handleKeyUp(event) {
    // 處理和弦鍵 (如 gg, gb)
    if (chordBuffer.length) {
      const key = event.key.toLowerCase();
      chordBuffer.push(key);
      clearTimeout(chordTimeout);
      chordTimeout = setTimeout(() => chordBuffer.length = 0, 500);

      const chord = chordBuffer.join('');
      const list = shortcuts.get(chord);
      if (list) {
        for (const { handler, preventDefault, stopPropagation } of list) {
          if (preventDefault) event.preventDefault();
          if (stopPropagation) event.stopPropagation();
          handler(event);
        }
        chordBuffer.length = 0;
      }
    }
  }

  function registerChord(chord, handler, options = {}) {
    return register(chord, handler, options);
  }

  function getRegisteredShortcuts() {
    const result = [];
    for (const [key, list] of shortcuts) {
      list.forEach(s => result.push({ key, description: s.description }));
    }
    return result.sort((a, b) => a.key.localeCompare(b.key));
  }

  function destroy() {
    document.removeEventListener('keydown', handleKeyDown);
    document.removeEventListener('keyup', handleKeyUp);
    shortcuts.clear();
    chordBuffer.length = 0;
  }

  document.addEventListener('keydown', handleKeyDown);
  document.addEventListener('keyup', handleKeyUp);

  return {
    register,
    unregister,
    registerChord,
    getRegisteredShortcuts,
    destroy
  };
}

export function createGlobalShortcuts(context) {
  const kb = createKeyboardManager();
  const { composer, timeline, panels, commandPalette, sessions, models } = context;

  // 全域快捷鍵
  kb.register('mod+k', () => commandPalette.toggle(), { description: '開啟命令面板' });
  kb.register('mod+/', () => showShortcutsHelp(), { description: '顯示快捷鍵說明' });

  // 會話管理
  kb.register('mod+n', () => sessions.create(), { description: '新建會話' });
  kb.register('mod+shift+d', () => sessions.deleteCurrent(), { description: '刪除當前會話' });
  kb.register('mod+r', () => sessions.renameCurrent(), { description: '重新命名會話' });

  // 介面切換
  kb.register('mod+\\', () => panels.toggleLeftSidebar(), { description: '切換左側欄' });
  kb.register('mod+alt+\\', () => panels.toggleRightSidebar(), { description: '切換右側欄' });
  kb.register('mod+j', () => panels.toggleBottomPanel(), { description: '切換底部面板' });
  kb.register('mod+shift+t', () => toggleTheme(), { description: '切換主題' });

  // AI 操作
  kb.register('mod+m', () => models.openPicker(), { description: '選擇模型' });

  // 編輯器
  kb.register('mod+u', () => composer.attach(), { description: '附加檔案' });

  // 導航
  kb.register('mod+f', () => timeline.search(), { description: '搜尋訊息' });
  kb.register('mod+arrowup', () => timeline.scrollToTop(), { description: '跳轉到頂部' });
  kb.register('mod+arrowdown', () => timeline.scrollToBottom(), { description: '跳轉到底部' });

  // Escape 鍵特別處理
  kb.register('escape', (e) => {
    if (composer.isStreaming()) composer.stop();
    else if (panels.isAnyOpen()) panels.closeAll();
    else if (commandPalette.isOpen()) commandPalette.close();
  }, { description: '停止生成/關閉面板/關閉命令面板' });

  return kb;
}

// 沒有明確設定時，以系統偏好為準（與 opencode.css 的 prefers-color-scheme 一致）。
function toggleTheme() {
  const html = document.documentElement;
  const system = window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  const current = html.dataset.theme || system;
  html.dataset.theme = current === 'dark' ? 'light' : 'dark';
  try {
    localStorage.setItem('theme', html.dataset.theme);
  } catch {
    // 無法寫入儲存空間時，本次頁面仍生效
  }
}

function showShortcutsHelp() {
  const kb = createKeyboardManager();
  const shortcuts = kb.getRegisteredShortcuts();
  let msg = '鍵盤快捷鍵:\n\n';
  let currentSection = '';
  shortcuts.forEach(s => {
    // 簡單分組
    if (s.key.startsWith('mod+shift') || s.key.startsWith('mod+alt')) {
      if (currentSection !== '進階') { msg += '\n--- 進階 ---\n'; currentSection = '進階'; }
    } else if (s.key.startsWith('mod+')) {
      if (currentSection !== '主要') { msg += '\n--- 主要 ---\n'; currentSection = '主要'; }
    }
    msg += `${formatKey(s.key).padEnd(18)} ${s.description}\n`;
  });
  alert(msg);
}

function formatKey(key) {
  return key
    .replace('mod', navigator.platform.includes('Mac') ? '⌘' : 'Ctrl')
    .replace('shift', '⇧')
    .replace('alt', '⌥')
    .replace('control', 'Ctrl')
    .replace('arrowup', '↑')
    .replace('arrowdown', '↓')
    .replace('arrowleft', '←')
    .replace('arrowright', '→')
    .replace('escape', 'Esc')
    .replace('enter', 'Enter')
    .replace('space', 'Space')
    .replace('backspace', 'Backspace')
    .replace('delete', 'Del')
    .replace('+', ' + ');
}

export function setupTextareaShortcuts(textarea, handlers) {
  const kb = createKeyboardManager();

  kb.register('enter', (e) => {
    if (!e.shiftKey) { e.preventDefault(); handlers.send?.(); }
    else { handlers.newline?.(); }
  }, { allowInInput: true });

  kb.register('shift+enter', () => handlers.newline?.(), { allowInInput: true });
  kb.register('escape', () => handlers.stop?.(), { allowInInput: true });
  kb.register('mod+u', () => handlers.attach?.(), { allowInInput: true });
  kb.register('mod+enter', () => handlers.send?.(), { allowInInput: true });

  return kb;
}

export default { createKeyboardManager, createGlobalShortcuts, setupTextareaShortcuts };