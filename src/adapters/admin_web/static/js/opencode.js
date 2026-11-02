/**
 * AI 頁面主入口：初始化、事件綁定、協調各模組。
 *
 * 初始化順序（刻意設計）：
 *   1. 建立 UI 元件（只做 DOM 綁定，不會因後端失敗而中斷）
 *   2. 綁定所有事件
 *   3. 非同步載入後端資料（失敗只顯示 toast，頁面仍可操作）
 */

import { api } from './api.js';
import { el, clear } from './markdown.js';
import { icon } from './icons.js';
import { createTimeline } from './timeline.js';
import { createComposer } from './composer.js';
import { createPanelManager, createSessionList, createModelPicker, createToolsPanel } from './panels.js';
import { createCommandPalette, createDefaultCommands } from './command-palette.js';
import { createGlobalShortcuts } from './keyboard.js';

// ─────────────────────────────────────────────────────────────
// 靜態設定
// ─────────────────────────────────────────────────────────────

const SUGGESTIONS = [
  '幫我整理今天的重點',
  '解釋這段程式碼的邏輯',
  '把這段文字翻成英文',
  '列出學習計畫的步驟'
];

// 與後端 agent_tools() 同步（ai_chat.rs）。僅在 Agent 模式下會實際執行。
const TOOLS = [
  { name: 'workspace_list', description: '列出工作區檔案', icon: 'folder-open' },
  { name: 'workspace_read', description: '讀取工作區檔案', icon: 'book-open' },
  { name: 'workspace_write', description: '寫入工作區檔案', icon: 'pencil' },
  { name: 'workspace_delete', description: '刪除工作區檔案', icon: 'trash' },
  { name: 'calc', description: '計算算式', icon: 'calculator' },
  { name: 'now', description: '查詢現在時間', icon: 'clock' }
];

const ELEMENT_IDS = {
  leftSidebar: 'leftSidebar', rightSidebar: 'rightSidebar', bottomPanel: 'bottomPanel',
  sidebarOverlay: 'sidebarOverlay', resizeLeft: 'resizeLeft', resizeRight: 'resizeRight',
  resizeBottom: 'resizeBottom', menuBtn: 'menuBtn', rightSidebarToggle: 'rightSidebarToggle',
  opencodeToggle: 'opencodeToggle', leftSidebarClose: 'leftSidebarClose',
  rightSidebarClose: 'rightSidebarClose', bottomPanelClose: 'bottomPanelClose',
  leftTabs: 'leftTabs', leftContent: 'leftContent', rightTabs: 'rightTabs', rightContent: 'rightContent',
  chatTitle: 'chatTitle', timeline: 'timeline', emptyState: 'emptyState',
  emptyStateDesc: 'emptyStateDesc', suggestions: 'suggestions', dropzone: 'dropzone',
  composerInput: 'composerInput', sendBtn: 'sendBtn', stopBtn: 'stopBtn', attachBtn: 'attachBtn',
  fileInput: 'fileInput', modelPickerBtn: 'modelPickerBtn', modelPickerDropdown: 'modelPickerDropdown',
  currentModelName: 'currentModelName', newSessionBtn: 'newSessionBtn', sessionList: 'sessionList',
  sessionSearch: 'sessionSearch', toolsList: 'toolsList', agentMode: 'agentMode',
  terminalOutput: 'terminalOutput', commandPalette: 'commandPalette', cmdInput: 'cmdInput',
  cmdResults: 'cmdResults', ocEnabled: 'ocEnabled', ocBaseUrl: 'ocBaseUrl', ocModel: 'ocModel',
  ocApiKey: 'ocApiKey', ocKeyHint: 'ocKeyHint', ocStatus: 'ocStatus', ocSaveBtn: 'ocSaveBtn',
  ocTestBtn: 'ocTestBtn', ocAccountChips: 'ocAccountChips', ocAccountInput: 'ocAccountInput',
  ocAddAccountBtn: 'ocAddAccountBtn', ocSaveAccessBtn: 'ocSaveAccessBtn', ocAccessNote: 'ocAccessNote'
};

// ─────────────────────────────────────────────────────────────
// 狀態（模組層級，只在 init 之後使用）
// ─────────────────────────────────────────────────────────────

const elements = {};
let panels;
let timeline;
let composer;
let sessionList;
let commandPalette;
let threads = [];          // 對話摘要（來自 /state 與本地更新）
let models = [];           // 全部模型（含停用），選擇器會再過濾
let currentSessionId = null;
let currentModel = 'auto';
let canManage = false;
let isStreaming = false;
let activeJob = null;      // { jobId, stop() }
let ocState = null;        // 最近一次 /opencode/state
let accessList = [];       // 存取名單編輯中的內容
let agentMode = false;

// ─────────────────────────────────────────────────────────────
// 小工具
// ─────────────────────────────────────────────────────────────

function toast(message, isError = false) {
  const container = elements.toastContainer;
  if (!container) return;
  const node = el('div', 'toast' + (isError ? ' error' : ''), message);
  container.appendChild(node);
  setTimeout(() => node.remove(), 3000);
}

function isNarrow() {
  return window.innerWidth <= 900;
}

function deriveTitle(text) {
  const clean = String(text || '').trim().replace(/\s+/g, ' ');
  if (!clean) return '新對話';
  return clean.length > 30 ? clean.slice(0, 30) + '…' : clean;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function modelName(id) {
  if (id === 'auto') return '自動';
  return models.find((m) => m.id === id)?.name || id;
}

// ─────────────────────────────────────────────────────────────
// 模型、會話、空狀態
// ─────────────────────────────────────────────────────────────

function availableModels() {
  return models.filter((m) => m.enabled && m.available);
}

function updateModelLabel() {
  elements.currentModelName.textContent = modelName(currentModel);
}

function openModelPicker(force) {
  const dropdown = elements.modelPickerDropdown;
  const open = force !== undefined ? force : dropdown.classList.contains('hidden');
  dropdown.classList.toggle('hidden', !open);
  if (open) {
    createModelPicker(dropdown, models, currentModel, selectModel).render();
  }
}

function selectModel(id) {
  currentModel = id;
  updateModelLabel();
  openModelPicker(false);
  updateEmptyState();
}

function updateEmptyState() {
  const hasMessages = timeline.getMessages().length > 0;
  elements.emptyState.classList.toggle('hidden', hasMessages);
  const count = availableModels().length;
  if (!count) {
    elements.emptyStateDesc.textContent = '目前沒有可用模型，請聯絡管理員。';
  } else if (currentModel === 'auto') {
    elements.emptyStateDesc.textContent = `目前是「自動」模式：系統會依你的問題挑最合適的模型（共 ${count} 個可用）`;
  } else {
    elements.emptyStateDesc.textContent = `目前使用 ${modelName(currentModel)}，可在右上角切換`;
  }
}

function renderSessions() {
  const query = (elements.sessionSearch.value || '').trim().toLowerCase();
  const list = threads.filter((t) => !query || String(t.title || '').toLowerCase().includes(query));
  sessionList.render(list, currentSessionId);
}

function setUrlSession(id) {
  const url = id ? `/admin/ai?session=${encodeURIComponent(id)}` : '/admin/ai';
  history.replaceState(null, '', url);
}

function newSession() {
  if (isStreaming) {
    toast('請等待目前回覆完成', true);
    return;
  }
  currentSessionId = null;
  setUrlSession(null);
  timeline.clearAll();
  elements.chatTitle.textContent = '新對話';
  renderSessions();
  updateEmptyState();
  elements.composerInput.focus();
}

async function selectSession(id) {
  if (isStreaming) {
    toast('請等待目前回覆完成', true);
    return;
  }
  try {
    const data = await api.threads.get(id);
    const thread = data.thread;
    currentSessionId = thread.id;
    setUrlSession(thread.id);
    elements.chatTitle.textContent = thread.title || '對話';
    if (thread.model_id) currentModel = thread.model_id;
    updateModelLabel();
    timeline.setMessages((thread.messages || []).map((m, i) => ({
      id: `h-${i}`,
      role: m.role,
      content: m.content,
      model: m.model,
      keyName: m.key_name,
      // 後端歷史附件可能是字串或物件；時間軸只顯示名稱
      attachments: (m.attachments || []).map((a) => (typeof a === 'string' ? a : (a && a.name) || '附件'))
    })));
    renderSessions();
    updateEmptyState();
    if (isNarrow()) panels.toggleLeftSidebar(false);
  } catch (error) {
    toast('載入對話失敗：' + error.message, true);
  }
}

async function deleteSession(id) {
  if (!confirm('確定要刪除這個對話嗎？')) return;
  try {
    await api.threads.delete(id);
    threads = threads.filter((t) => t.id !== id);
    if (currentSessionId === id) {
      newSession();
    } else {
      renderSessions();
    }
  } catch (error) {
    toast('刪除失敗：' + error.message, true);
  }
}

async function renameSession(id) {
  const session = threads.find((t) => t.id === id);
  if (!session) return;
  const next = prompt('新標題：', session.title || '');
  if (!next || next === session.title) return;
  try {
    const data = await api.threads.rename(id, next);
    session.title = data.title || next;
    if (currentSessionId === id) elements.chatTitle.textContent = session.title;
    renderSessions();
  } catch (error) {
    toast('重新命名失敗：' + error.message, true);
  }
}

async function clearAllSessions() {
  if (!confirm('確定要刪除所有對話嗎？此操作無法復原。')) return;
  try {
    await api.threads.clear();
    threads = [];
    newSession();
  } catch (error) {
    toast('清空失敗：' + error.message, true);
  }
}

/** 重新抓取 /state（模型、對話摘要）。失敗不影響目前畫面。 */
async function refreshState() {
  try {
    const data = await api.state();
    threads = data.threads || [];
    models = data.models || [];
    canManage = !!data.can_manage;
    renderSessions();
    updateModelLabel();
    updateEmptyState();
  } catch (error) {
    toast('載入資料失敗：' + error.message, true);
  }
}

// ─────────────────────────────────────────────────────────────
// 送出與串流
// ─────────────────────────────────────────────────────────────

async function sendMessage(text, attachments, attachmentNames) {
  if (isStreaming) return;
  isStreaming = true;
  elements.terminalOutput.replaceChildren();

  timeline.addMessage({
    id: `u-${Date.now()}`,
    role: 'user',
    content: text || '（請看附件）',
    attachments: attachmentNames
  });
  updateEmptyState();

  const messageId = `a-${Date.now()}`;
  timeline.startStreaming(messageId);

  try {
    const started = await api.chat.send({
      thread_id: currentSessionId || '',
      message: text,
      model_id: currentModel === 'auto' ? '' : currentModel,
      attachments,
      agent: agentMode
    });

    if (!currentSessionId) {
      threads.unshift({
        id: started.thread_id,
        title: deriveTitle(text),
        model_id: started.model_id,
        updated: Date.now(),
        count: 0
      });
      elements.chatTitle.textContent = deriveTitle(text);
    }
    currentSessionId = started.thread_id;
    setUrlSession(currentSessionId);
    renderSessions();

    const job = await runJob(started.job_id, messageId);
    finishMessage(messageId, job, started);
  } catch (error) {
    timeline.endStreaming({
      id: messageId,
      role: 'assistant',
      content: `錯誤：${error.message || '發送失敗'}`
    });
    toast(error.message || '發送失敗', true);
  } finally {
    isStreaming = false;
    await refreshState();
  }
}

/**
 * 串流一個 job，直到拿到最終結果。
 * 主要走 SSE（delta 逐段推送）；SSE 斷線或被代理吃掉時退回輪詢 /chat/status。
 * 回傳最終 job 快照（含 text / ok / error / model / key_name）。
 */
function runJob(jobId, messageId) {
  return new Promise((resolve) => {
    let text = '';
    let settled = false;
    let es = null;
    let paintTimer = null;
    let lastPaint = 0;

    const paint = () => {
      paintTimer = null;
      lastPaint = Date.now();
      timeline.updateStreaming(text);
    };
    const schedulePaint = () => {
      if (paintTimer) return;
      const wait = Math.max(0, 80 - (Date.now() - lastPaint));
      paintTimer = setTimeout(paint, wait);
    };

    const settle = (job) => {
      if (settled) return;
      settled = true;
      if (paintTimer) clearTimeout(paintTimer);
      if (es) es.close();
      activeJob = null;
      resolve(job);
    };

    const pollUntilDone = async () => {
      for (;;) {
        let snapshot;
        try {
          snapshot = await api.chat.status(jobId);
        } catch (error) {
          return { ok: false, error: error.message || '無法取得回覆狀態' };
        }
        if (snapshot.text && snapshot.text.length > text.length) {
          text = snapshot.text;
          schedulePaint();
        }
        if (snapshot.done) return snapshot;
        await sleep(350);
      }
    };

    activeJob = {
      jobId,
      // 停止：通知後端（保留已生成內容），然後輪詢到結束。
      stop: async () => {
        try {
          await api.chat.stop(jobId);
        } catch {
          // 工作可能已結束，忽略
        }
        if (es) es.close();
        settle(await pollUntilDone());
      }
    };

    try {
      es = new EventSource(api.chat.streamUrl(jobId));
    } catch {
      es = null;
    }
    if (!es) {
      pollUntilDone().then(settle);
      return;
    }

    es.addEventListener('meta', () => {});
    es.addEventListener('step', (event) => appendTerminal(parseJson(event.data)));
    es.addEventListener('delta', (event) => {
      text += event.data;
      schedulePaint();
    });
    es.addEventListener('done', (event) => settle(parseJson(event.data) || { ok: false, error: '回覆資料格式錯誤' }));
    es.addEventListener('gone', () => {
      if (es) es.close();
      pollUntilDone().then(settle);
    });
    es.onerror = () => {
      if (settled) return;
      es.close();
      pollUntilDone().then(settle);
    };
  });
}

function parseJson(raw) {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

function finishMessage(messageId, job, started) {
  const ok = !!job.ok && !job.error;
  const body = job.text || '';
  const content = ok
    ? body
    : `${body ? body + '\n\n' : ''}錯誤：${job.error || '生成失敗'}`;
  timeline.endStreaming({
    id: messageId,
    role: 'assistant',
    content,
    model: job.model || started.model_id,
    keyName: job.key_name || ''
  });
  if (!ok) toast(job.error || '生成失敗', true);
}

function stopGeneration() {
  if (activeJob) activeJob.stop();
}

/** 工具執行步驟 → 底部終端機面板。 */
function appendTerminal(step) {
  if (step === null || step === undefined) return;
  const text = typeof step === 'string' ? step : JSON.stringify(step);
  if (elements.terminalOutput.querySelector('.term-empty')) {
    elements.terminalOutput.replaceChildren();
  }
  elements.terminalOutput.appendChild(el('div', 'term-line', '› ' + text.slice(0, 500)));
  panels.toggleBottomPanel(true);
}

// ─────────────────────────────────────────────────────────────
// opencode 面板（左側欄第三個分頁）
// ─────────────────────────────────────────────────────────────

const OPENCODE_FIELDS = ['ocEnabled', 'ocBaseUrl', 'ocModel', 'ocApiKey', 'ocSaveBtn',
  'ocAccountInput', 'ocAddAccountBtn', 'ocSaveAccessBtn'];

async function loadOpencode() {
  try {
    ocState = await api.opencode.state();
    renderOpencode();
  } catch (error) {
    elements.ocStatus.textContent = '載入失敗：' + error.message;
    elements.ocStatus.className = 'text-sm text-error';
  }
}

function renderOpencode() {
  const s = ocState || {};
  const writable = !!s.can_manage;

  elements.ocEnabled.value = s.config_enabled ? '1' : '0';
  elements.ocBaseUrl.value = s.base_url || '';
  elements.ocModel.value = s.model || '';
  elements.ocApiKey.value = '';
  elements.ocKeyHint.textContent = s.key_masked
    ? `目前金鑰：${s.key_masked}（留空不更動）`
    : '尚未設定金鑰';

  if (s.config_enabled && !s.healthy) {
    elements.ocStatus.textContent = '已啟用，但 sidecar 無法連線';
    elements.ocStatus.className = 'text-sm text-warning status-dot';
  } else {
    elements.ocStatus.textContent = s.healthy ? '連線正常' : '已停用';
    elements.ocStatus.className = 'text-sm status-dot ' + (s.healthy ? 'text-success' : 'text-muted');
  }

  for (const id of OPENCODE_FIELDS) {
    const node = elements[id];
    if (node) node.disabled = !writable;
  }
  elements.ocAccessNote.textContent = writable ? '' : '只有管理員可以修改設定與存取名單。';
  setAccessList(s.allowed_accounts || []);
  updateOpencodeFieldState();
}

/** 停用時仍可改「開關」本身，其餘欄位一律鎖住。 */
function updateOpencodeFieldState() {
  const writable = !!(ocState && ocState.can_manage);
  const on = elements.ocEnabled.value === '1';
  elements.ocBaseUrl.disabled = !writable || !on;
  elements.ocModel.disabled = !writable || !on;
  elements.ocApiKey.disabled = !writable || !on;
}

async function saveOpencodeConfig() {
  const payload = {
    enabled: elements.ocEnabled.value === '1',
    base_url: elements.ocBaseUrl.value.trim(),
    model: elements.ocModel.value.trim()
  };
  const key = elements.ocApiKey.value.trim();
  if (key) payload.api_key = key;
  try {
    await api.opencode.saveConfig(payload);
    toast('opencode 設定已儲存');
    await loadOpencode();
  } catch (error) {
    toast('儲存失敗：' + error.message, true);
  }
}

/** 測試只讀取「已儲存」的設定與健康狀態，不會改動任何設定。 */
async function testOpencode() {
  elements.ocTestBtn.disabled = true;
  const label = elements.ocTestBtn.textContent;
  elements.ocTestBtn.textContent = '測試中…';
  try {
    ocState = await api.opencode.state();
    renderOpencode();
    toast(ocState.healthy ? '連線測試成功' : '連線測試失敗', !ocState.healthy);
  } catch (error) {
    toast('測試失敗：' + error.message, true);
  } finally {
    elements.ocTestBtn.disabled = false;
    elements.ocTestBtn.textContent = label;
  }
}

function setAccessList(list) {
  accessList = [...list];
  drawAccessChips();
}

function drawAccessChips() {
  const container = elements.ocAccountChips;
  clear(container);
  if (!accessList.length) {
    container.appendChild(el('div', 'text-sm text-muted', '目前沒有授權帳號。'));
    return;
  }
  accessList.forEach((id) => {
    const chip = el('span', 'account-chip');
    chip.appendChild(el('span', null, id));
    if (ocState && ocState.can_manage) {
      const remove = el('button', 'chip-remove');
      remove.innerHTML = icon('x', 12);
      remove.type = 'button';
      remove.setAttribute('aria-label', `移除 ${id}`);
      remove.addEventListener('click', () => {
        accessList = accessList.filter((x) => x !== id);
        drawAccessChips();
      });
      chip.appendChild(remove);
    }
    container.appendChild(chip);
  });
}

function addAccessAccount() {
  const value = elements.ocAccountInput.value.trim();
  if (!value) return;
  if (value.length > 64 || /[\s,]/.test(value)) {
    toast('帳號格式不正確（不可含空白或逗號，最多 64 字元）', true);
    return;
  }
  if (!accessList.includes(value)) {
    accessList.push(value);
    drawAccessChips();
  }
  elements.ocAccountInput.value = '';
}

async function saveOpencodeAccess() {
  try {
    await api.opencode.saveAccess(accessList);
    toast('授權名單已儲存');
    await loadOpencode();
  } catch (error) {
    toast('儲存失敗：' + error.message, true);
  }
}

function openOpencodePanel() {
  panels.toggleLeftSidebar(true);
  elements.leftTabs.querySelector('[data-tab="opencode"]')?.click();
}

// ─────────────────────────────────────────────────────────────
// 工具分頁、建議按鈕
// ─────────────────────────────────────────────────────────────

function renderSuggestions() {
  clear(elements.suggestions);
  SUGGESTIONS.forEach((text) => {
    const chip = el('button', 'suggestion-chip', text);
    chip.type = 'button';
    chip.addEventListener('click', () => {
      composer.setValue(text);
      composer.focus();
    });
    elements.suggestions.appendChild(chip);
  });
}

function renderTools() {
  const tools = createToolsPanel(elements.toolsList, TOOLS, (tool) => {
    composer.setValue(`請使用「${tool.name}」工具協助我：`);
    composer.focus();
    if (isNarrow()) panels.toggleLeftSidebar(false);
  });
  tools.render();
}

// ─────────────────────────────────────────────────────────────
// 初始化
// ─────────────────────────────────────────────────────────────

function collectElements() {
  for (const [key, id] of Object.entries(ELEMENT_IDS)) {
    elements[key] = document.getElementById(id);
  }
  elements.toastContainer = document.getElementById('toastContainer');
  const missing = Object.entries(elements).filter(([, node]) => !node).map(([key]) => key);
  if (missing.length) {
    console.error('AI 頁面缺少元素：', missing.join(', '));
  }
}

function buildComponents() {
  panels = createPanelManager(elements);
  timeline = createTimeline(elements.timeline, {});
  composer = createComposer(
    elements.attachBtn,
    elements.fileInput,
    elements.dropzone,
    elements.composerInput,
    elements.sendBtn,
    elements.stopBtn,
    (text, attachments, names) => sendMessage(text, attachments, names),
    () => stopGeneration()
  );
  sessionList = createSessionList(
    elements.sessionList,
    [],
    null,
    (id) => selectSession(id),
    (id) => deleteSession(id),
    (id) => renameSession(id)
  );

  const commands = createDefaultCommands({
    sessions: {
      create: newSession,
      deleteCurrent: () => currentSessionId && deleteSession(currentSessionId),
      renameCurrent: () => currentSessionId && renameSession(currentSessionId),
      clearAll: clearAllSessions
    },
    composer: {
      send: () => composer.send(),
      stop: () => composer.stop(),
      attach: () => elements.fileInput.click(),
      focus: () => composer.focus()
    },
    timeline: {
      search: () => focusSessionSearch(),
      scrollToTop: () => { elements.timeline.scrollTop = 0; },
      scrollToBottom: () => { elements.timeline.scrollTop = elements.timeline.scrollHeight; }
    },
    panels,
    models: { openPicker: () => openModelPicker(true) },
    opencode: { open: openOpencodePanel }
  });
  commandPalette = createCommandPalette(
    elements.commandPalette,
    elements.cmdInput,
    elements.cmdResults,
    commands,
    () => {}
  );

  createGlobalShortcuts({
    composer: {
      send: () => composer.send(),
      stop: () => composer.stop(),
      attach: () => elements.fileInput.click(),
      newline: () => {},
      isStreaming: () => composer.isStreaming()
    },
    timeline: {
      search: () => focusSessionSearch(),
      scrollToTop: () => { elements.timeline.scrollTop = 0; },
      scrollToBottom: () => { elements.timeline.scrollTop = elements.timeline.scrollHeight; }
    },
    panels,
    commandPalette,
    sessions: {
      create: newSession,
      deleteCurrent: () => currentSessionId && deleteSession(currentSessionId),
      renameCurrent: () => currentSessionId && renameSession(currentSessionId)
    },
    models: { openPicker: () => openModelPicker(true) }
  });
}

function focusSessionSearch() {
  panels.toggleLeftSidebar(true);
  elements.leftTabs.querySelector('[data-tab="sessions"]')?.click();
  elements.sessionSearch.focus();
}

function bindEvents() {
  elements.newSessionBtn.addEventListener('click', newSession);
  elements.sessionSearch.addEventListener('input', renderSessions);

  elements.modelPickerBtn.addEventListener('click', () => openModelPicker());
  document.addEventListener('click', (event) => {
    if (!event.target.closest('#modelPickerContainer')) {
      elements.modelPickerDropdown.classList.add('hidden');
    }
  });

  elements.opencodeToggle.addEventListener('click', openOpencodePanel);

  elements.ocEnabled.addEventListener('change', updateOpencodeFieldState);
  elements.ocSaveBtn.addEventListener('click', saveOpencodeConfig);
  elements.ocTestBtn.addEventListener('click', testOpencode);
  elements.ocAddAccountBtn.addEventListener('click', addAccessAccount);
  elements.ocAccountInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      event.preventDefault();
      addAccessAccount();
    }
  });
  elements.ocSaveAccessBtn.addEventListener('click', saveOpencodeAccess);

  elements.agentMode.checked = agentMode;
  elements.agentMode.addEventListener('change', () => {
    agentMode = elements.agentMode.checked;
    try {
      localStorage.setItem('ai-agent-mode', agentMode ? '1' : '0');
    } catch {
      // 無法寫入 localStorage 時只影響記憶，不影響功能
    }
  });

  panels.setupTabs(elements.leftTabs, elements.leftContent, (target) => {
    if (target === 'opencode') loadOpencode();
  });
  panels.setupTabs(elements.rightTabs, elements.rightContent);
}

async function init() {
  try {
    const savedTheme = localStorage.getItem('theme');
    if (savedTheme) document.documentElement.dataset.theme = savedTheme;
    agentMode = localStorage.getItem('ai-agent-mode') === '1';
  } catch {
    agentMode = false;
  }

  collectElements();
  // 觸控裝置沒有 Enter 發送的概念（而且狹窄），佔位文字改短
  if (window.matchMedia('(hover: none)').matches && elements.composerInput) {
    elements.composerInput.placeholder = '輸入訊息…';
  }
  buildComponents();
  renderSuggestions();
  renderTools();
  bindEvents();
  updateModelLabel();
  updateEmptyState();

  await refreshState();

  const sessionId = new URLSearchParams(window.location.search).get('session');
  if (sessionId) await selectSession(sessionId);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', () => {
    init().catch((error) => console.error('AI 頁面初始化失敗', error));
  });
} else {
  init().catch((error) => console.error('AI 頁面初始化失敗', error));
}
