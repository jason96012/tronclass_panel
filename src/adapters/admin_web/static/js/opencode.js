/**
 * 主入口點 - 初始化整個 AI 對話應用
 */

import { appState } from './state.js';
import { api } from './api.js';
import { createTimeline, createEmptyState, removeEmptyState } from './timeline.js';
import { createComposer, createOpencodeComposer } from './composer.js';
import { createPanelManager, createSessionList, createModelPicker, createToolsPanel } from './panels.js';
import { createCommandPalette, createDefaultCommands } from './command-palette.js';
import { createKeyboardManager, createGlobalShortcuts, setupTextareaShortcuts } from './keyboard.js';
import { renderMarkdown, el, clear } from './markdown.js';

function q(id) { return document.getElementById(id); }
function toast(message, isError) {
  const container = q('toastContainer');
  const t = el('div', 'toast' + (isError ? ' error' : ''), message);
  container.appendChild(t);
  setTimeout(() => t.remove(), 3000);
}

// DOM 元素
const elements = {
  leftSidebar: q('leftSidebar'),
  rightSidebar: q('rightSidebar'),
  bottomPanel: q('bottomPanel'),
  sidebarOverlay: q('sidebarOverlay'),
  resizeLeft: q('resizeLeft'),
  resizeRight: q('resizeRight'),
  resizeBottom: q('resizeBottom'),
  menuBtn: q('menuBtn'),
  rightSidebarToggle: q('rightSidebarToggle'),
  opencodeToggle: q('opencodeToggle'),
  leftSidebarClose: q('leftSidebarClose'),
  rightSidebarClose: q('rightSidebarClose'),
  bottomPanelClose: q('bottomPanelClose'),
  chatTitle: q('chatTitle'),
  timeline: q('timeline'),
  emptyState: q('emptyState'),
  suggestions: q('suggestions'),
  dropzone: q('dropzone'),
  composerInput: q('composerInput'),
  sendBtn: q('sendBtn'),
  stopBtn: q('stopBtn'),
  attachBtn: q('attachBtn'),
  fileInput: q('fileInput'),
  composerFileInput: q('composerFileInput'),
  modelPickerBtn: q('modelPickerBtn'),
  modelPickerDropdown: q('modelPickerDropdown'),
  newSessionBtn: q('newSessionBtn'),
  sessionList: q('sessionList'),
  sessionsEmpty: q('sessionsEmpty'),
  toolsList: q('toolsList'),
  commandPalette: q('commandPalette'),
  cmdInput: q('cmdInput'),
  cmdResults: q('cmdResults'),
  // opencode 面板
  ocEnabled: q('ocEnabled'),
  ocBaseUrl: q('ocBaseUrl'),
  ocModel: q('ocModel'),
  ocApiKey: q('ocApiKey'),
  ocStatus: q('ocStatus'),
  ocSaveBtn: q('ocSaveBtn'),
  ocTestBtn: q('ocTestBtn'),
  ocAccountList: q('ocAccountList'),
  ocSaveAccessBtn: q('ocSaveAccessBtn')
};

// 狀態
let currentSessionId = null;
let currentModel = 'auto';
let isStreaming = false;
let streamingMessageId = null;
let sessions = [];
let models = [];
let allModels = [];
let tools = [];

// 初始化面板管理
const panelManager = createPanelManager(elements);

// 初始化會話列表
const sessionList = createSessionList(
  elements.sessionList,
  sessions,
  currentSessionId,
  (id) => loadSession(id),
  (id) => deleteSession(id),
  (id) => renameSession(id)
);

// 初始化模型選擇器
let modelPicker = null;

// 初始化工具面板
const toolsPanel = createToolsPanel(elements.toolsList, tools, (tool) => {
  console.log('Tool clicked:', tool);
});

// 初始化時間軸
const timeline = createTimeline(elements.timeline, {
  onLoadMore: loadMoreMessages,
  onScroll: (atBottom) => { }
});

// 初始化編輯器
const composer = createComposer(
  elements.attachBtn,
  elements.fileInput,
  elements.dropzone,
  elements.composerInput,
  elements.sendBtn,
  elements.stopBtn,
  async (text, attachments, attachmentNames) => {
    await sendMessage(text, attachments, attachmentNames);
  }
);

// 初始化命令面板
const commands = createDefaultCommands({
  timeline,
  composer,
  panels: panelManager,
  api,
  sessions: {
    create: newSession,
    deleteCurrent: () => currentSessionId && deleteSession(currentSessionId),
    renameCurrent: () => currentSessionId && renameSession(currentSessionId),
    clearAll: clearAllSessions
  },
  models: {
    openPicker: () => toggleModelPicker(true),
    openManager: openModelManager
  }
});
const commandPalette = createCommandPalette(
  elements.commandPalette,
  elements.cmdInput,
  elements.cmdResults,
  commands,
  () => { }
);

// 初始化鍵盤快捷鍵
const keyboard = createGlobalShortcuts({
  composer: {
    send: () => composer.send(),
    stop: () => composer.stop(),
    attach: () => elements.fileInput.click(),
    newline: () => { },
    isStreaming: () => isStreaming
  },
  timeline: {
    search: () => { },
    scrollToTop: () => elements.timeline.scrollTop = 0,
    scrollToBottom: () => elements.timeline.scrollTop = elements.timeline.scrollHeight
  },
  panels: panelManager,
  commandPalette,
  sessions: {
    create: newSession,
    deleteCurrent: () => currentSessionId && deleteSession(currentSessionId),
    renameCurrent: () => currentSessionId && renameSession(currentSessionId)
  },
  models: {
    openPicker: () => toggleModelPicker(true),
    openManager: openModelManager
  }
});

// 模型選擇器下拉選單
function toggleModelPicker(force) {
  const open = force !== undefined ? force : elements.modelPickerDropdown.classList.contains('hidden');
  elements.modelPickerDropdown.classList.toggle('hidden', !open);
  if (open) renderModelPicker();
}

function renderModelPicker() {
  clear(elements.modelPickerDropdown);
  const autoOpt = el('div', 'model-option' + (currentModel === 'auto' ? ' selected' : ''));
  autoOpt.innerHTML = '<span>⚡</span><div class="model-option-info"><div class="model-option-name">自動</div><div class="model-option-provider">依問題自動選擇最適合模型</div></div>';
  autoOpt.addEventListener('click', () => selectModel('auto'));
  elements.modelPickerDropdown.appendChild(autoOpt);

  models.filter(m => m.enabled && m.available).forEach(model => {
    const opt = el('div', 'model-option' + (model.id === currentModel ? ' selected' : ''));
    opt.innerHTML = `
      <div class="model-option-info">
        <div class="model-option-name">${model.name}</div>
        <div class="model-option-provider">${model.provider} · ${model.id}</div>
      </div>
    `;
    opt.addEventListener('click', () => selectModel(model.id));
    elements.modelPickerDropdown.appendChild(opt);
  });
}

function selectModel(modelId) {
  currentModel = modelId;
  appState.currentModel(modelId);
  elements.currentModelName.textContent = modelId === 'auto' ? '自動' : models.find(m => m.id === modelId)?.name || modelId;
  toggleModelPicker(false);
  if (currentSessionId) {
    // 更新會話模型
    api.threads.rename(currentSessionId, { model_id: modelId }).catch(console.error);
  }
}

// 點擊外部關閉下拉選單
document.addEventListener('click', (e) => {
  if (!e.target.closest('#modelPickerContainer')) {
    elements.modelPickerDropdown.classList.add('hidden');
  }
});

// 主初始化
async function init() {
  // 載入主題
  const savedTheme = localStorage.getItem('theme');
  if (savedTheme) document.documentElement.dataset.theme = savedTheme;

  // 載入初始資料
  await Promise.all([
    loadSessions(),
    loadModels(),
    loadSettings(),
    loadOpencodeState()
  ]);

  // 渲染初始狀態
  sessionList.render();
  updateEmptyState();

  // 綁定事件
  bindEvents();

  // 檢查 URL 是否有會話 ID
  const urlParams = new URLSearchParams(window.location.search);
  const sessionId = urlParams.get('session');
  if (sessionId) {
    await loadSession(sessionId);
  }
}

function bindEvents() {
  // 新會話
  elements.newSessionBtn.addEventListener('click', newSession);

  // 模型選擇器按鈕
  elements.modelPickerBtn.addEventListener('click', () => toggleModelPicker());

  // opencode 面板
  elements.ocSaveBtn.addEventListener('click', saveOpencodeConfig);
  elements.ocTestBtn.addEventListener('click', testOpencodeConnection);
  elements.ocSaveAccessBtn.addEventListener('click', saveOpencodeAccess);
  elements.ocEnabled.addEventListener('change', () => {
    const enabled = elements.ocEnabled.value === '1';
    elements.ocBaseUrl.disabled = !enabled;
    elements.ocModel.disabled = !enabled;
    elements.ocApiKey.disabled = !enabled;
  });
}

async function loadSessions() {
  try {
    sessions = await api.threads.list();
    sessionList.render();
  } catch (e) {
    console.error('載入會話失敗:', e);
    toast('載入會話失敗', true);
  }
}

async function loadModels() {
  try {
    const data = await api.models.list();
    allModels = data.models || [];
    models = allModels.filter(m => m.enabled);
    updateModelPickerButton();
  } catch (e) {
    console.error('載入模型失敗:', e);
  }
}

async function loadSettings() {
  try {
    const data = await api.settings.get();
    currentModel = data.model_id || 'auto';
    appState.currentModel(currentModel);
    updateModelPickerButton();
  } catch (e) {
    console.error('載入設定失敗:', e);
  }
}

async function loadOpencodeState() {
  try {
    const data = await api.opencode.state();
    updateOpencodePanel(data);
  } catch (e) {
    console.error('載入 opencode 狀態失敗:', e);
    elements.ocStatus.textContent = '載入失敗';
  }
}

function updateModelPickerButton() {
  const model = models.find(m => m.id === currentModel);
  elements.currentModelName.textContent = currentModel === 'auto' ? '自動' : model?.name || currentModel;
}

function updateEmptyState() {
  const hasMessages = timeline.getMessages().length > 0;
  elements.emptyState.style.display = hasMessages ? 'none' : 'flex';
  if (!hasMessages && models.length) {
    const model = models.find(m => m.id === currentModel);
    elements.emptyStateDesc.textContent = currentModel === 'auto'
      ? `目前是「自動」模式：系統會依你的問題挑最合適的模型（共 ${models.length} 個可用）`
      : `目前使用 ${model?.name || currentModel}，可在右上角切換`;
  }
}

async function newSession() {
  try {
    const data = await api.threads.create({ title: '新對話' });
    currentSessionId = data.id;
    history.replaceState(null, '', `?session=${currentSessionId}`);
    await loadSession(currentSessionId);
  } catch (e) {
    toast('建立會話失敗', true);
  }
}

async function loadSession(id) {
  try {
    elements.timeline.scrollTop = 0;
    timeline.clearAll();
    removeEmptyState(elements.emptyState);

    const data = await api.threads.get(id);
    currentSessionId = id;
    history.replaceState(null, '', `?session=${id}`);
    elements.chatTitle.textContent = data.title || '對話';

    if (data.model_id) {
      currentModel = data.model_id;
      updateModelPickerButton();
    }

    if (data.messages?.length) {
      timeline.setMessages(data.messages.map((m, i) => ({
        id: m.id || `msg-${i}`,
        role: m.role,
        content: m.content,
        model: m.model,
        keyName: m.key_name,
        attachments: m.attachments
      })));
    }

    sessionList.render();
    updateEmptyState();
    panelManager.toggleLeftSidebar(false);
    elements.composerInput.focus();
  } catch (e) {
    toast('載入會話失敗', true);
  }
}

async function deleteSession(id) {
  if (!confirm('確定要刪除這個對話嗎？')) return;
  try {
    await api.threads.delete(id);
    if (currentSessionId === id) {
      currentSessionId = null;
      history.replaceState(null, '', '/admin/ai');
      timeline.clearAll();
      elements.chatTitle.textContent = '新對話';
      updateEmptyState();
    }
    await loadSessions();
  } catch (e) {
    toast('刪除失敗', true);
  }
}

async function renameSession(id) {
  const session = sessions.find(s => s.id === id);
  if (!session) return;
  const newTitle = prompt('新標題:', session.title);
  if (!newTitle || newTitle === session.title) return;
  try {
    await api.threads.rename(id, { title: newTitle });
    await loadSessions();
    if (currentSessionId === id) elements.chatTitle.textContent = newTitle;
  } catch (e) {
    toast('重新命名失敗', true);
  }
}

async function clearAllSessions() {
  if (!confirm('確定要刪除所有對話嗎？此操作無法復原。')) return;
  try {
    await api.threads.clearAll();
    currentSessionId = null;
    history.replaceState(null, '', '/admin/ai');
    timeline.clearAll();
    elements.chatTitle.textContent = '新對話';
    updateEmptyState();
    await loadSessions();
  } catch (e) {
    toast('清空失敗', true);
  }
}

async function sendMessage(text, attachments, attachmentNames) {
  isStreaming = true;
  composer.setSending(true);

  const userMessage = {
    id: `msg-${Date.now()}`,
    role: 'user',
    content: text || (attachments.some(a => a.kind === 'image') ? '（請看這張圖片）' : '（請看附件）'),
    attachments: attachmentNames
  };
  timeline.addMessage(userMessage);
  updateEmptyState();

  const aiMessageId = `msg-${Date.now() + 1}`;
  const aiElement = timeline.startStreaming(aiMessageId);

  try {
    const response = await api.chat.send({
      thread_id: currentSessionId,
      message: text,
      model_id: currentModel === 'auto' ? undefined : currentModel,
      attachments
    });

    if (response.job_id) {
      await pollJob(response.job_id, aiMessageId);
    } else if (response.message) {
      timeline.endStreaming({
        id: aiMessageId,
        role: 'assistant',
        content: response.message.content,
        model: response.model_id,
        keyName: response.key_name
      });
    }
  } catch (e) {
    timeline.showError(aiMessageId, e);
    toast(e.message || '發送失敗', true);
  } finally {
    isStreaming = false;
    composer.setSending(false);
  }
}

async function pollJob(jobId, messageId) {
  let buffer = '';
  while (isStreaming) {
    try {
      const data = await api.chat.poll(jobId);
      if (data.done) {
        if (data.message) {
          timeline.endStreaming({
            id: messageId,
            role: 'assistant',
            content: data.message.content,
            model: data.model_id,
            keyName: data.key_name
          });
        }
        break;
      }
      if (data.delta) {
        buffer += data.delta;
        timeline.appendStreamingChunk(data.delta);
      }
      await new Promise(r => setTimeout(r, 350));
    } catch (e) {
      if (e.status !== 404) {
        timeline.showError(messageId, e);
      }
      break;
    }
  }
}

async function loadMoreMessages() {
  if (!currentSessionId) return;
  // TODO: 實作分頁載入
}

function updateOpencodePanel(data) {
  elements.ocEnabled.value = data.enabled ? '1' : '0';
  elements.ocBaseUrl.value = data.base_url || '';
  elements.ocModel.value = data.model || '';
  elements.ocApiKey.value = '';
  elements.ocStatus.textContent = data.healthy ? '🟢 連線正常' : '🔴 連線失敗';
  elements.ocStatus.style.color = data.healthy ? 'var(--success)' : 'var(--error)';

  const enabled = data.enabled;
  elements.ocBaseUrl.disabled = !enabled;
  elements.ocModel.disabled = !enabled;
  elements.ocApiKey.disabled = !enabled;

  if (data.allowed_accounts) {
    elements.ocAccountList.innerHTML = '';
    data.allowed_accounts.forEach(acc => {
      const label = el('label', 'flex items-center gap-2');
      label.innerHTML = `<input type="checkbox" value="${acc.id}" ${acc.allowed ? 'checked' : ''}> ${escapeHtml(acc.name)}`;
      elements.ocAccountList.appendChild(label);
    });
  }
}

async function saveOpencodeConfig() {
  try {
    await api.opencode.saveConfig({
      enabled: elements.ocEnabled.value === '1',
      base_url: elements.ocBaseUrl.value,
      model: elements.ocModel.value,
      api_key: elements.ocApiKey.value || undefined
    });
    toast('設定已儲存');
    await loadOpencodeState();
  } catch (e) {
    toast('儲存失敗', true);
  }
}

async function testOpencodeConnection() {
  elements.ocTestBtn.disabled = true;
  elements.ocTestBtn.textContent = '測試中...';
  try {
    await api.opencode.saveConfig({
      enabled: true,
      base_url: elements.ocBaseUrl.value,
      model: elements.ocModel.value,
      api_key: elements.ocApiKey.value || undefined
    });
    const data = await api.opencode.state();
    elements.ocStatus.textContent = data.healthy ? '🟢 連線正常' : '🔴 連線失敗';
    elements.ocStatus.style.color = data.healthy ? 'var(--success)' : 'var(--error)';
    toast(data.healthy ? '連線測試成功' : '連線測試失敗', !data.healthy);
  } catch (e) {
    toast('測試失敗: ' + e.message, true);
  } finally {
    elements.ocTestBtn.disabled = false;
    elements.ocTestBtn.textContent = '測試連線';
  }
}

async function saveOpencodeAccess() {
  const checkboxes = elements.ocAccountList.querySelectorAll('input[type="checkbox"]');
  const accounts = Array.from(checkboxes).map(cb => ({ id: cb.value, allowed: cb.checked }));
  try {
    await api.opencode.saveAccess(accounts);
    toast('授權已儲存');
  } catch (e) {
    toast('儲存失敗', true);
  }
}

function openModelManager() {
  panelManager.toggleRightSidebar(true);
  // 切換到模型管理標籤
  document.querySelector('[data-tab="models"]')?.click();
}

// 啟動
document.addEventListener('DOMContentLoaded', init);

// 熱鍵：Cmd+K 開啟命令面板
document.addEventListener('keydown', (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
    e.preventDefault();
    commandPalette.toggle();
  }
});

export { init, toast };