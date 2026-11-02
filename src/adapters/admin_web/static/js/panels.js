/**
 * 面板管理模組 - 側邊欄、底部面板、調整大小
 */

import { el } from './markdown.js';
import { icon } from './icons.js';

import { appState } from './state.js';

export function createPanelManager(elements) {
  const {
    leftSidebar,
    rightSidebar,
    bottomPanel,
    sidebarOverlay,
    resizeLeft,
    resizeRight,
    resizeBottom,
    menuBtn,
    rightSidebarToggle,
    opencodeToggle,
    leftSidebarClose,
    rightSidebarClose,
    bottomPanelClose
  } = elements;

  let leftWidth = 280;
  let rightWidth = 320;
  let bottomHeight = 200;
  let isResizing = false;
  let resizeType = null;
  let startX = 0;
  let startY = 0;
  let startWidth = 0;
  let startHeight = 0;

  const MIN_LEFT = 200, MAX_LEFT = 400;
  const MIN_RIGHT = 240, MAX_RIGHT = 480;
  const MIN_BOTTOM = 120, MAX_BOTTOM = 600;

  function isMobile() {
    return window.innerWidth <= 900;
  }

  function updateMobileState() {
    const mobile = isMobile();
    appState.ui(state => ({ ...state, isMobile: mobile }));
    if (!mobile) {
      leftSidebar.classList.remove('open');
      rightSidebar.classList.remove('open');
      sidebarOverlay.classList.remove('open');
    }
  }

  function toggleLeftSidebar(force) {
    const mobile = isMobile();
    const open = force !== undefined ? force : !leftSidebar.classList.contains('open');
    if (mobile) {
      // 手機抽屜靠 transform 進出場；hidden 屬性若殘留會讓抽屜永遠看不到。
      leftSidebar.hidden = false;
      leftSidebar.classList.toggle('open', open);
      sidebarOverlay.classList.toggle('open', open);
      rightSidebar.classList.remove('open');
    } else {
      leftSidebar.hidden = !open;
    }
    appState.ui(state => ({ ...state, leftSidebarOpen: open }));
  }

  function toggleRightSidebar(force) {
    const mobile = isMobile();
    const open = force !== undefined ? force : !rightSidebar.classList.contains('open');
    if (mobile) {
      rightSidebar.hidden = false;
      rightSidebar.classList.toggle('open', open);
      sidebarOverlay.classList.toggle('open', open);
      leftSidebar.classList.remove('open');
    } else {
      rightSidebar.hidden = !open;
    }
    appState.ui(state => ({ ...state, rightSidebarOpen: open }));
  }

  function toggleBottomPanel(force) {
    const open = force !== undefined ? force : bottomPanel.classList.contains('hidden');
    bottomPanel.classList.toggle('hidden', !open);
    appState.ui(state => ({ ...state, bottomPanelOpen: open }));
  }

  function closeAllSidebars() {
    leftSidebar.classList.remove('open');
    rightSidebar.classList.remove('open');
    sidebarOverlay.classList.remove('open');
    appState.ui(state => ({ ...state, leftSidebarOpen: false, rightSidebarOpen: false }));
  }

  // 調整大小邏輯
  function startResize(e, type) {
    if (isMobile()) return;
    isResizing = true;
    resizeType = type;
    startX = e.clientX;
    startY = e.clientY;
    document.body.style.cursor = type === 'bottom' ? 'row-resize' : 'col-resize';
    document.body.style.userSelect = 'none';

    if (type === 'left') startWidth = leftWidth;
    else if (type === 'right') startWidth = rightWidth;
    else if (type === 'bottom') startHeight = bottomHeight;

    document.addEventListener('mousemove', onResize);
    document.addEventListener('mouseup', stopResize);
    e.preventDefault();
  }

  function onResize(e) {
    if (!isResizing) return;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;

    if (resizeType === 'left') {
      leftWidth = Math.max(MIN_LEFT, Math.min(MAX_LEFT, startWidth + dx));
      leftSidebar.style.width = leftWidth + 'px';
      leftSidebar.style.flexBasis = leftWidth + 'px';
    } else if (resizeType === 'right') {
      rightWidth = Math.max(MIN_RIGHT, Math.min(MAX_RIGHT, startWidth - dx));
      rightSidebar.style.width = rightWidth + 'px';
      rightSidebar.style.flexBasis = rightWidth + 'px';
    } else if (resizeType === 'bottom') {
      bottomHeight = Math.max(MIN_BOTTOM, Math.min(MAX_BOTTOM, startHeight + dy));
      bottomPanel.style.height = bottomHeight + 'px';
    }
  }

  function stopResize() {
    isResizing = false;
    resizeType = null;
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    document.removeEventListener('mousemove', onResize);
    document.removeEventListener('mouseup', stopResize);
  }

  // 標籤頁切換
  function setupTabs(tabContainer, panelContainer, onChange) {
    const tabs = tabContainer.querySelectorAll('.tab');
    const panels = panelContainer.querySelectorAll('.tab-panel');

    tabs.forEach(tab => {
      tab.addEventListener('click', () => {
        const target = tab.dataset.tab;
        tabs.forEach(t => {
          t.classList.toggle('active', t === tab);
          t.setAttribute('aria-selected', t === tab);
        });
        panels.forEach(p => {
          p.classList.toggle('hidden', p.dataset.panel !== target);
        });
        onChange?.(target);
      });
    });
  }

  // 綁定事件
  menuBtn?.addEventListener('click', () => toggleLeftSidebar());
  rightSidebarToggle?.addEventListener('click', () => toggleRightSidebar());
  leftSidebarClose?.addEventListener('click', () => toggleLeftSidebar(false));
  rightSidebarClose?.addEventListener('click', () => toggleRightSidebar(false));
  bottomPanelClose?.addEventListener('click', () => toggleBottomPanel(false));
  // 手機模式：點遮罩關閉抽屜（規格：Overlay 點擊關閉側邊欄）
  sidebarOverlay?.addEventListener('click', () => closeAllSidebars());
  sidebarOverlay?.addEventListener('click', closeAllSidebars);

  resizeLeft?.addEventListener('mousedown', e => startResize(e, 'left'));
  resizeRight?.addEventListener('mousedown', e => startResize(e, 'right'));
  resizeBottom?.addEventListener('mousedown', e => startResize(e, 'bottom'));

  window.addEventListener('resize', updateMobileState);

  // 初始化
  updateMobileState();

  function isAnyOpen() {
    if (isMobile()) return leftSidebar.classList.contains('open') || rightSidebar.classList.contains('open');
    return !rightSidebar.hidden;
  }
  function closeAll() {
    closeAllSidebars();
    if (!isMobile()) rightSidebar.hidden = true;
  }
  return {
    toggleLeftSidebar,
    toggleRightSidebar,
    toggleBottomPanel,
    closeAllSidebars,
    isAnyOpen,
    closeAll,
    setupTabs,
    get leftWidth() { return leftWidth; },
    get rightWidth() { return rightWidth; },
    get bottomHeight() { return bottomHeight; },
    setLeftWidth: (w) => { leftWidth = Math.max(MIN_LEFT, Math.min(MAX_LEFT, w)); leftSidebar.style.width = leftWidth + 'px'; },
    setRightWidth: (w) => { rightWidth = Math.max(MIN_RIGHT, Math.min(MAX_RIGHT, w)); rightSidebar.style.width = rightWidth + 'px'; },
    setBottomHeight: (h) => { bottomHeight = Math.max(MIN_BOTTOM, Math.min(MAX_BOTTOM, h)); bottomPanel.style.height = bottomHeight + 'px'; }
  };
}

export function createSessionList(panel, sessions, currentId, onSelect, onDelete, onRename) {
  function render(list = sessions, current = currentId) {
    clear(panel);
    if (!list.length) {
      panel.innerHTML = `<div class="empty-state empty-state-sm"><div class="empty-state-icon">${icon('message-square', 26)}</div><div class="empty-state-title">還沒有對話</div><div class="empty-state-desc">點擊「新會話」開始</div></div>`;
      return;
    }
    list.forEach(session => {
      const item = el('div', 'session-item' + (session.id === current ? ' active' : ''));
      item.innerHTML = `
        <span class="session-item-icon">${session.id === current ? '●' : '○'}</span>
        <span class="session-item-title" title="${escapeHtml(session.title)}">${escapeHtml(session.title)}</span>
        <span class="session-item-time">${formatTime(session.updated ?? session.updatedAt)}</span>
      `;
      const openMenu = (x, y) => showContextMenu(x, y, [
        { label: '重新命名', action: () => onRename(session.id) },
        { label: '刪除', action: () => onDelete(session.id), danger: true }
      ]);
      let pressTimer = null;
      let longPressed = false;
      const cancelPress = () => {
        if (pressTimer) {
          clearTimeout(pressTimer);
          pressTimer = null;
        }
      };
      item.addEventListener('click', (e) => {
        if (longPressed) {
          // 長按剛開過選單：這次點擊不選取會話
          longPressed = false;
          e.preventDefault();
          return;
        }
        onSelect(session.id);
      });
      item.addEventListener('contextmenu', e => {
        e.preventDefault();
        openMenu(e.clientX, e.clientY);
      });
      // 觸控長按（手機沒有右鍵）：約 500ms 開選單；移動即取消
      item.addEventListener('touchstart', e => {
        const t = e.touches[0];
        longPressed = false;
        cancelPress();
        pressTimer = setTimeout(() => {
          pressTimer = null;
          longPressed = true;
          openMenu(t.clientX, t.clientY);
        }, 500);
      }, { passive: true });
      item.addEventListener('touchmove', cancelPress, { passive: true });
      item.addEventListener('touchcancel', cancelPress, { passive: true });
      item.addEventListener('touchend', e => {
        cancelPress();
        // 長按結束時吃掉合成的 click，避免選單剛開就被關掉
        if (longPressed) e.preventDefault();
      }, { passive: false });
      panel.appendChild(item);
    });
  }

  function formatTime(ts) {
    if (!ts) return '';
    const date = new Date(ts);
    const now = new Date();
    const diff = now - date;
    if (diff < 60000) return '剛剛';
    if (diff < 3600000) return Math.floor(diff / 60000) + '分鐘前';
    if (diff < 86400000) return Math.floor(diff / 3600000) + '小時前';
    return date.toLocaleDateString('zh-TW', { month: 'short', day: 'numeric' });
  }

  function showContextMenu(x, y, items) {
    const existing = document.querySelector('.context-menu');
    existing?.remove();

    const menu = el('div', 'context-menu');
    menu.style.cssText = `position:fixed;left:${Math.max(8, Math.min(x, window.innerWidth - 170))}px;top:${Math.max(8, Math.min(y, window.innerHeight - 120))}px;background:var(--bg-elevated);border:1px solid var(--border-base);border-radius:var(--radius-md);padding:var(--space-1);z-index:var(--z-tooltip);min-width:160px;box-shadow:var(--shadow-lg);`;
    items.forEach(item => {
      const btn = el('div', 'btn btn-secondary btn-sm w-full text-left' + (item.danger ? ' text-error' : ''), item.label);
      btn.style.cssText = 'width:100%;justify-content:flex-start;padding:var(--space-2) var(--space-3);';
      btn.addEventListener('click', () => { item.action(); menu.remove(); });
      menu.appendChild(btn);
    });
    document.body.appendChild(menu);
    const close = () => { menu.remove(); document.removeEventListener('click', close); };
    setTimeout(() => document.addEventListener('click', close), 0);
  }

  return { render };
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

export function createModelPicker(container, models, currentModel, onSelect) {
  function render() {
    clear(container);
    const autoOpt = el('div', 'model-option' + (currentModel === 'auto' ? ' selected' : ''));
    autoOpt.innerHTML = `<span class="model-option-glyph">${icon('zap', 16)}</span><div class="model-option-info"><div class="model-option-name">自動</div><div class="model-option-provider">依問題自動選擇最適合模型</div></div>`;
    autoOpt.addEventListener('click', () => onSelect('auto'));
    container.appendChild(autoOpt);

    models.filter(m => m.enabled && m.available).forEach(model => {
      const opt = el('div', 'model-option' + (model.id === currentModel ? ' selected' : ''));
      opt.innerHTML = `
        <div class="model-option-info">
          <div class="model-option-name">${escapeHtml(model.name)}</div>
          <div class="model-option-provider">${escapeHtml(model.provider)} · ${model.id}</div>
        </div>
      `;
      opt.addEventListener('click', () => onSelect(model.id));
      container.appendChild(opt);
    });
  }
  return { render };
}

export function createToolsPanel(panel, tools, onToolClick) {
  function render() {
    clear(panel);
    if (!tools.length) {
      panel.innerHTML = `<div class="empty-state" style="padding:var(--space-4);"><div class="empty-state-icon">${icon('wrench', 26)}</div><div class="empty-state-title">無可用工具</div><div class="empty-state-desc">安裝 opencode 插件以獲得更多工具</div></div>`;
      return;
    }
    tools.forEach(tool => {
      const item = el('div', 'tool-item');
      item.innerHTML = `
        <div class="tool-item-icon">${icon(tool.icon || 'settings', 18)}</div>
        <div class="tool-item-info">
          <div class="tool-item-name">${escapeHtml(tool.name)}</div>
          <div class="tool-item-desc">${escapeHtml(tool.description || '')}</div>
        </div>
      `;
      item.addEventListener('click', () => onToolClick(tool));
      panel.appendChild(item);
    });
  }
  return { render };
}

export default { createPanelManager, createSessionList, createModelPicker, createToolsPanel };