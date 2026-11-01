/**
 * 命令面板模組 - Cmd+K 快速搜尋與命令
 */

import { appState } from './state.js';
import { api } from './api.js';
import { el, clear } from './markdown.js';

function fuzzyMatch(query, text) {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let score = 0;
  let lastIndex = -1;
  for (const char of q) {
    const index = t.indexOf(char, lastIndex + 1);
    if (index === -1) return 0;
    score += index === lastIndex + 1 ? 2 : 1;
    lastIndex = index;
  }
  return score + (t.startsWith(q) ? 10 : 0);
}

export function createCommandPalette(palette, input, results, commands, onClose) {
  let selectedIndex = 0;
  let filteredCommands = [];
  let isOpen = false;

  function open() {
    palette.classList.add('open');
    input.value = '';
    input.focus();
    filteredCommands = commands.map(cmd => ({ ...cmd, score: 0 }));
    renderResults();
    isOpen = true;
    document.addEventListener('keydown', handleKeyDown);
  }

  function close() {
    palette.classList.remove('open');
    isOpen = false;
    document.removeEventListener('keydown', handleKeyDown);
    onClose?.();
  }

  function toggle() {
    isOpen ? close() : open();
  }

  function handleKeyDown(e) {
    if (e.key === 'Escape') { close(); return; }
    if (e.key === 'ArrowDown') { e.preventDefault(); selectNext(); }
    if (e.key === 'ArrowUp') { e.preventDefault(); selectPrev(); }
    if (e.key === 'Enter') { e.preventDefault(); executeSelected(); }
  }

  function handleInput(e) {
    const query = e.target.value.trim();
    if (!query) {
      filteredCommands = commands.map(cmd => ({ ...cmd, score: 0 }));
    } else {
      filteredCommands = commands
        .map(cmd => ({ ...cmd, score: fuzzyMatch(query, cmd.name) + fuzzyMatch(query, cmd.description || '') }))
        .filter(cmd => cmd.score > 0)
        .sort((a, b) => b.score - a.score);
    }
    selectedIndex = 0;
    renderResults();
  }

  function selectNext() {
    if (filteredCommands.length) {
      selectedIndex = (selectedIndex + 1) % filteredCommands.length;
      renderResults();
    }
  }

  function selectPrev() {
    if (filteredCommands.length) {
      selectedIndex = (selectedIndex - 1 + filteredCommands.length) % filteredCommands.length;
      renderResults();
    }
  }

  function executeSelected() {
    const cmd = filteredCommands[selectedIndex];
    if (cmd) {
      close();
      cmd.action?.();
    }
  }

  function renderResults() {
    clear(results);
    if (!filteredCommands.length) {
      results.appendChild(el('div', 'cmd-section', '無符合的命令'));
      return;
    }

    let currentSection = null;
    filteredCommands.forEach((cmd, idx) => {
      if (cmd.section !== currentSection) {
        currentSection = cmd.section;
        if (currentSection) {
          results.appendChild(el('div', 'cmd-section', currentSection));
        }
      }
      const item = el('div', 'cmd-item' + (idx === selectedIndex ? ' selected' : ''));
      item.innerHTML = `
        <div>
          <strong>${escapeHtml(cmd.name)}</strong>
          ${cmd.description ? `<span style="color:var(--text-muted);margin-left:var(--space-2);font-size:var(--font-size-sm)">${escapeHtml(cmd.description)}</span>` : ''}
        </div>
        ${cmd.keybind ? `<kbd>${escapeHtml(cmd.keybind)}</kbd>` : ''}
      `;
      item.addEventListener('click', () => { selectedIndex = idx; executeSelected(); });
      results.appendChild(item);
    });
  }

  input.addEventListener('input', handleInput);

  return { open, close, toggle, isOpen: () => isOpen };
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

export function createDefaultCommands(context) {
  const { timeline, composer, panels, api, sessions, models } = context;

  return [
    { name: '新會話', description: '建立新的對話會話', section: '會話', keybind: '⌘N', action: () => sessions.create() },
    { name: '刪除當前會話', description: '刪除正在查看的會話', section: '會話', keybind: '⌘⇧D', action: () => sessions.deleteCurrent() },
    { name: '重新命名會話', description: '修改會話標題', section: '會話', keybind: '⌘R', action: () => sessions.renameCurrent() },
    { name: '清空所有會話', description: '刪除所有對話紀錄', section: '會話', action: () => sessions.clearAll() },

    { name: '切換左側欄', description: '顯示/隱藏會話與工具面板', section: '介面', keybind: '⌘\\', action: () => panels.toggleLeftSidebar() },
    { name: '切換右側欄', description: '顯示/隱藏上下文面板', section: '介面', keybind: '⌘⌥\\', action: () => panels.toggleRightSidebar() },
    { name: '切換底部面板', description: '顯示/隱藏終端機面板', section: '介面', keybind: '⌘J', action: () => panels.toggleBottomPanel() },
    { name: '切換主題', description: '切換深色/淺色模式', section: '介面', keybind: '⌘⇧T', action: () => toggleTheme() },

    { name: '選擇模型', description: '開啟模型選擇器', section: 'AI', keybind: '⌘M', action: () => models.openPicker() },
    { name: '新增 API 金鑰', description: '設定新的 API 金鑰', section: 'AI', action: () => api.keys.add() },
    { name: '管理模型', description: '管理可用模型列表', section: 'AI', action: () => models.openManager() },

    { name: '發送訊息', description: '發送當前輸入的訊息', section: '編輯器', keybind: 'Enter', action: () => composer.send() },
    { name: '停止生成', description: '停止 AI 回覆生成', section: '編輯器', keybind: 'Esc', action: () => composer.stop() },
    { name: '附加檔案', description: '上傳圖片或文字檔案', section: '編輯器', keybind: '⌘U', action: () => composer.attach() },
    { name: '新行', description: '在編輯器中換行', section: '編輯器', keybind: 'Shift+Enter', action: () => composer.newline() },

    { name: '搜尋訊息', description: '在當前會話中搜尋', section: '搜尋', keybind: '⌘F', action: () => timeline.search() },
    { name: '跳轉到頂部', description: '捲動到對話頂部', section: '導航', keybind: '⌘↑', action: () => timeline.scrollToTop() },
    { name: '跳轉到底部', description: '捲動到對話底部', section: '導航', keybind: '⌘↓', action: () => timeline.scrollToBottom() },

    { name: '開啟設定', description: '開啟 AI 設定面板', section: '設定', keybind: '⌘,', action: () => panels.openSettings() },
    { name: '鍵盤快捷鍵', description: '顯示所有快捷鍵', section: '說明', keybind: '⌘/', action: () => showKeyboardShortcuts() },
    { name: '關於', description: '關於此應用程式', section: '說明', action: () => showAbout() }
  ];
}

function toggleTheme() {
  const html = document.documentElement;
  const current = html.dataset.theme || 'dark';
  html.dataset.theme = current === 'dark' ? 'light' : 'dark';
  localStorage.setItem('theme', html.dataset.theme);
}

function showKeyboardShortcuts() {
  alert('鍵盤快捷鍵:\n⌘K - 命令面板\n⌘N - 新會話\n⌘M - 選擇模型\n⌘F - 搜尋訊息\n⌘J - 切換底部面板\n⌘\\ - 切換左側欄\n⌘⌥\\ - 切換右側欄\n⌘⇧T - 切換主題\n⌘/ - 顯示快捷鍵');
}

function showAbout() {
  alert('AI 對話 - 基於 opencode 風格重寫\n版本: 2.0.0');
}

export default { createCommandPalette, createDefaultCommands };