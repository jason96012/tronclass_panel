/**
 * AI 頁面主入口：初始化、事件綁定、協調各模組。
 *
 * OPENCODE_AGENT_ONLY_V2（Turn 8）：所有訊息都交給 opencode agent 處理，
 * 不再有一般聊天路徑。模型清單由管理員在「模型」分頁管理。
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
import { createPanelManager, createSessionList, createModelPicker } from './panels.js';
import { createCommandPalette, createDefaultCommands } from './command-palette.js';
import { createGlobalShortcuts } from './keyboard.js';

// ─────────────────────────────────────────────────────────────
// 靜態設定
// ─────────────────────────────────────────────────────────────

const SUGGESTIONS = [
  '列出工作區的檔案結構',
  '讀取 AGENTS.md 並摘要其中的規則',
  '寫一個計算平均數的 Python 腳本',
  '解釋這個專案目前有哪些檔案'
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
  sessionSearch: 'sessionSearch', modelsPanel: 'modelsPanel', ocModelList: 'ocModelList',
  ocModelNote: 'ocModelNote', ocModelAddCard: 'ocModelAddCard', ocModelInput: 'ocModelInput',
  ocModelAddBtn: 'ocModelAddBtn',
  commandPalette: 'commandPalette', cmdInput: 'cmdInput',
  cmdResults: 'cmdResults', ocEnabled: 'ocEnabled', ocBaseUrl: 'ocBaseUrl',
  ocApiKey: 'ocApiKey', ocKeyHint: 'ocKeyHint', ocStatus: 'ocStatus', ocSaveBtn: 'ocSaveBtn',
  ocTestBtn: 'ocTestBtn', ocAccountChips: 'ocAccountChips', ocAccountInput: 'ocAccountInput',
  ocAddAccountBtn: 'ocAddAccountBtn', ocSaveAccessBtn: 'ocSaveAccessBtn', ocAccessNote: 'ocAccessNote',
  ocManageNvidia: 'ocManageNvidia', ocManageCustom: 'ocManageCustom', ocModelTitle: 'ocModelTitle',
  ocModelAddHint: 'ocModelAddHint', ocCustomName: 'ocCustomName', ocCustomBaseUrl: 'ocCustomBaseUrl',
  ocCustomApiKey: 'ocCustomApiKey', ocCustomKeyHint: 'ocCustomKeyHint', ocCustomStatus: 'ocCustomStatus',
  ocCustomSaveBtn: 'ocCustomSaveBtn'
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
let threads = [];          // 對話摘要（來自 /opencode/sessions）
let ocModels = [];         // 模型清單（來自 /opencode/state）
let defaultModel = '';     // 預設模型
let currentModel = '';
let currentProvider = 'nvidia'; // 目前聊天選用的供應商：nvidia | custom
let ocCustom = normalizeCustom({}); // 自訂供應商狀態（來自 /opencode/state 的 custom）
let manageProvider = 'nvidia';      // 模型分頁目前管理哪個供應商     // 本頁目前選用的模型（預設 = defaultModel）
let currentSessionId = null;
let canManage = false;
let isStreaming = false;
let ocState = null;        // 最近一次 /opencode/state
let accessList = [];       // 存取名單編輯中的內容

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

/** 客戶端先產生 thread_id，第一則訊息送出前就能中止。 */
function newThreadId() {
  return `t-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/** 把輸入文字與附件合成一則送給 agent 的訊息。圖片目前無法送出，會註明。 */
function buildOutgoingMessage(text, attachments) {
  const parts = [];
  if (text) parts.push(text);
  const skipped = [];
  (attachments || []).forEach((att) => {
    if (att.kind === 'text' && att.text) {
      parts.push(`[附件：${att.name}]\n\`\`\`\n${att.text}\n\`\`\``);
    } else {
      skipped.push(att.name);
    }
  });
  if (skipped.length) {
    parts.push(`（以下附件未送出，opencode 模式目前只支援文字附件：${skipped.join('、')}）`);
  }
  return parts.join('\n\n');
}

// ─────────────────────────────────────────────────────────────
// 模型、會話、空狀態
// ─────────────────────────────────────────────────────────────

function updateModelLabel() {
  elements.currentModelName.textContent = currentModel || '未設定';
}

function openModelPicker(force) {
  const dropdown = elements.modelPickerDropdown;
  const open = force !== undefined ? force : dropdown.classList.contains('hidden');
  dropdown.classList.toggle('hidden', !open);
  if (open) {
    createModelPicker(dropdown, chatOptions(), optionKey(currentProvider, currentModel), selectModel).render();
  }
}

function optionKey(provider, id) {
  return `${provider}|${id}`;
}

/** 聊天選單的所有選項：NVIDIA 清單；自訂供應商只有在「已設定」時才列出。 */
function chatOptions() {
  const options = ocModels.map((id) => ({
    key: optionKey('nvidia', id), provider: 'nvidia', name: id, group: 'NVIDIA', isDefault: id === defaultModel
  }));
  if (ocCustom.configured) {
    ocCustom.models.forEach((id) => options.push({
      key: optionKey('custom', id), provider: 'custom', name: id, group: ocCustom.name,
      isDefault: id === ocCustom.default_model
    }));
  }
  return options;
}

function normalizeCustom(raw) {
  const c = raw || {};
  return {
    configured: !!c.configured,
    name: c.name || '自訂供應商',
    base_url: c.base_url || '',
    key_masked: c.key_masked || '',
    default_model: c.default_model || '',
    models: Array.isArray(c.models) ? c.models : []
  };
}

/** 目前選擇不在選項內時（例如模型被刪除、供應商停用）退回 NVIDIA 預設。 */
function ensureValidSelection() {
  const valid = chatOptions().some((o) => o.provider === currentProvider && o.name === currentModel);
  if (!valid) {
    currentProvider = 'nvidia';
    currentModel = defaultModel;
  }
}

function selectModel(option) {
  currentProvider = option.provider;
  currentModel = option.name;
  updateModelLabel();
  openModelPicker(false);
  updateEmptyState();
}

function updateEmptyState() {
  const hasMessages = timeline.getMessages().length > 0;
  elements.emptyState.classList.toggle('hidden', hasMessages);
  const desc = elements.emptyStateDesc;
  if (!ocState) {
    desc.textContent = '載入中…';
  } else if (!ocState.config_enabled) {
    desc.textContent = 'opencode 尚未啟用，請管理員在 opencode 分頁開啟。';
  } else if (!ocState.healthy) {
    desc.textContent = 'opencode sidecar 無法連線，暫時無法送出訊息。';
  } else if (!ocState.allowed) {
    desc.textContent = '此帳號未被授權使用 opencode，請聯絡管理員。';
  } else if (!chatOptions().length) {
    desc.textContent = '目前沒有模型，請管理員在「模型」分頁新增。';
  } else {
    desc.textContent = `目前使用 ${currentModel}，輸入需求開始。agent 會在工作區內執行。`;
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
  // 手機：新會話後收起抽屜，才看得到輸入框
  if (isNarrow()) panels.toggleLeftSidebar(false);
  elements.composerInput.focus();
}

async function selectSession(id) {
  if (isStreaming) {
    toast('請等待目前回覆完成', true);
    return;
  }
  try {
    const data = await api.sessions.messages(id);
    currentSessionId = id;
    setUrlSession(id);
    const thread = threads.find((t) => t.id === id);
    elements.chatTitle.textContent = thread ? thread.title : '對話';
    timeline.setMessages((data.messages || []).map((m, i) => ({
      id: `h-${i}`,
      role: m.role,
      content: m.content
    })));
    renderSessions();
    updateEmptyState();
    if (isNarrow()) panels.toggleLeftSidebar(false);
  } catch (error) {
    if (error.code === 'session_not_found') {
      // 伺服器重啟後記憶體內的對話對應會消失：改開新會話
      toast('這個對話已不存在（伺服器可能已重啟），已開新會話', true);
      newSession();
      return;
    }
    toast('載入對話失敗：' + error.message, true);
  }
}

async function deleteSession(id) {
  if (!confirm('確定要刪除這個對話嗎？')) return;
  try {
    await api.sessions.delete(id);
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
  if (!next || next.trim() === session.title) return;
  try {
    const data = await api.sessions.rename(id, next.trim());
    session.title = data.title || next.trim();
    if (currentSessionId === id) elements.chatTitle.textContent = session.title;
    renderSessions();
  } catch (error) {
    toast('重新命名失敗：' + error.message, true);
  }
}

async function clearAllSessions() {
  if (!confirm('確定要刪除所有對話嗎？此操作無法復原。')) return;
  const ids = threads.map((t) => t.id);
  for (const id of ids) {
    try {
      await api.sessions.delete(id);
    } catch (error) {
      toast('刪除失敗：' + error.message, true);
    }
  }
  threads = [];
  newSession();
}

/** 重新抓取 /opencode/state（模型清單、權限、健康狀態）。失敗不影響目前畫面。 */
async function refreshState() {
  try {
    ocState = await api.state();
    ocModels = ocState.models || [];
    defaultModel = ocState.model || '';
    ocCustom = normalizeCustom(ocState.custom);
    ensureValidSelection();
    canManage = !!ocState.can_manage;
    updateModelLabel();
    updateEmptyState();
    renderModels();
  } catch (error) {
    toast('載入資料失敗：' + error.message, true);
  }
}

/** 抓取目前帳號的對話清單。未授權帳號不呼叫。 */
async function loadSessions() {
  if (!ocState || !ocState.allowed) return;
  try {
    const data = await api.sessions.list();
    threads = data.sessions || [];
    renderSessions();
  } catch (error) {
    toast('載入對話清單失敗：' + error.message, true);
  }
}

// ─────────────────────────────────────────────────────────────
// 送出與中止
// ─────────────────────────────────────────────────────────────

async function sendMessage(text, attachments, attachmentNames) {
  if (isStreaming) return;
  const message = buildOutgoingMessage(text, attachments);
  if (!message.trim()) return;

  isStreaming = true;
  if (!currentSessionId) currentSessionId = newThreadId();
  const threadId = currentSessionId;
  setUrlSession(threadId);

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
    const started = await api.chat({
      thread_id: threadId,
      message,
      model: currentModel,
      provider: currentProvider
    });
    const result = started.job_id
      ? await waitForChatJob(started.job_id, (partial) => timeline.updateStreaming(partial))
      : started;
    if (!threads.some((t) => t.id === threadId)) {
      threads.unshift({ id: threadId, title: deriveTitle(text), updated: Date.now() });
    }
    elements.chatTitle.textContent = (threads.find((t) => t.id === threadId) || {}).title || deriveTitle(text);
    renderSessions();
    const finalText = result.text || '（agent 沒有回傳文字）';
    timeline.endStreaming({
      id: messageId,
      role: 'assistant',
      content: finalText,
      model: result.model
    });
  } catch (error) {
    timeline.endStreaming({
      id: messageId,
      role: 'assistant',
      content: `錯誤：${error.message || '發送失敗'}`
    });
    toast(error.message || '發送失敗', true);
  } finally {
    isStreaming = false;
    await loadSessions();
    await refreshState();
  }
}

/** 每 0.5 秒輪詢一次背景回覆（後端約每秒更新一次進行中的文字），最多等 15 分鐘。連續 5 次網路錯誤（例如代理回 502）才放棄。 */
const CHAT_POLL_MS = 500;
const CHAT_MAX_WAIT_MS = 15 * 60 * 1000;
const CHAT_MAX_POLL_FAILURES = 5;

/** onPartial(text)：回覆還在生成時，每次拿到新的進行中文字就呼叫一次。 */
async function waitForChatJob(jobId, onPartial) {
  const deadline = Date.now() + CHAT_MAX_WAIT_MS;
  let failures = 0;
  for (;;) {
    await new Promise((resolve) => setTimeout(resolve, CHAT_POLL_MS));
    let status;
    try {
      status = await api.chatStatus(jobId);
      failures = 0;
    } catch (error) {
      // 找不到 job（已過期或伺服器重啟）直接失敗；其他錯誤（例如代理回 502）重試。
      failures += 1;
      if (error.status === 404 || failures >= CHAT_MAX_POLL_FAILURES) throw error;
      continue;
    }
    if (status.state === 'done') return status;
    if (status.state === 'failed') {
      throw Object.assign(new Error(status.error || '發送失敗'), { code: status.code });
    }
    if (onPartial && typeof status.partial === 'string' && status.partial) onPartial(status.partial);
    if (Date.now() > deadline) throw new Error('等待回覆逾時，請稍後到對話列表重新開啟');
  }
}

/** 中止目前對話的回覆（sidecar 會停止執行）。 */
async function stopGeneration() {
  if (!isStreaming || !currentSessionId) return;
  try {
    await api.abort(currentSessionId);
    toast('已要求中止');
  } catch (error) {
    toast('中止失敗：' + error.message, true);
  }
}

// ─────────────────────────────────────────────────────────────
// 模型管理（左側「模型」分頁）
// ─────────────────────────────────────────────────────────────

/** 模型分頁目前管理的清單與預設值（依 manageProvider）。 */
function manageList() {
  return manageProvider === 'custom' ? ocCustom.models : ocModels;
}

function manageDefault() {
  return manageProvider === 'custom' ? ocCustom.default_model : defaultModel;
}

function setManageProvider(provider) {
  manageProvider = provider;
  renderModels();
}

function renderManageTabs() {
  const custom = manageProvider === 'custom';
  elements.ocManageNvidia.className = 'btn btn-sm ' + (custom ? 'btn-secondary' : 'btn-primary');
  elements.ocManageCustom.className = 'btn btn-sm ' + (custom ? 'btn-primary' : 'btn-secondary');
  elements.ocManageNvidia.setAttribute('aria-pressed', String(!custom));
  elements.ocManageCustom.setAttribute('aria-pressed', String(custom));
  elements.ocModelTitle.textContent = custom ? `${ocCustom.name} 模型` : 'NVIDIA 模型';
  elements.ocModelAddHint.textContent = custom
    ? '請輸入服務商提供的模型 ID，例如 gpt-4o-mini 或 qwen2.5:7b。'
    : '請輸入 NVIDIA 的完整模型 ID（廠商/模型名），例如 nvidia/nemotron-3-super-120b-a12b';
  elements.ocModelInput.placeholder = custom ? 'gpt-4o-mini' : 'nvidia/模型名';
}

function renderModels() {
  const list = elements.ocModelList;
  if (!list) return;
  clear(list);
  renderManageTabs();
  const writable = canManage;
  const models = manageList();
  const dflt = manageDefault();
  const groupName = manageProvider === 'custom' ? ocCustom.name : 'NVIDIA';
  elements.ocModelAddCard.classList.toggle('hidden', !writable);
  if (!models.length) {
    list.appendChild(el('div', 'text-sm text-muted', '目前沒有模型。'));
  }
  models.forEach((id) => {
    const isDefault = id === dflt;
    const row = el('div', 'model-row' + (isDefault ? ' is-default' : ''));
    const main = el('div', 'model-row-main');
    main.appendChild(el('div', 'model-row-name', id));
    main.appendChild(el('div', 'field-hint', isDefault ? '預設模型' : groupName));
    row.appendChild(main);
    if (writable && !isDefault) {
      const actions = el('div', 'model-row-actions');
      const setDefault = el('button', 'btn btn-secondary btn-sm', '設為預設');
      setDefault.type = 'button';
      setDefault.addEventListener('click', () => manageModel('default', id));
      const remove = el('button', 'btn btn-danger btn-sm', '刪除');
      remove.type = 'button';
      remove.addEventListener('click', () => manageModel('remove', id));
      actions.appendChild(setDefault);
      actions.appendChild(remove);
      row.appendChild(actions);
    }
    list.appendChild(row);
  });
  let note = writable ? '' : '只有管理員可以管理模型。';
  if (writable && manageProvider === 'custom' && !ocCustom.configured) {
    note = '自訂供應商尚未設定 Base URL 或 API Key，設定完成後模型才會出現在聊天選單。';
  }
  elements.ocModelNote.textContent = note;
}

/** 把 /models 的回應寫回對應供應商的狀態。 */
function applyModelsResponse(provider, data) {
  if (provider === 'custom') {
    ocCustom.models = data.models || ocCustom.models;
    ocCustom.default_model = data.model || ocCustom.default_model;
  } else {
    ocModels = data.models || ocModels;
    defaultModel = data.model || defaultModel;
  }
}

/** action: 'add' | 'remove' | 'default'。每次操作都會寫入設定並要求 sidecar 重啟。 */
async function manageModel(action, model) {
  if (!canManage) return;
  if (action === 'remove' && !confirm(`確定要從清單移除 ${model} 嗎？`)) return;
  const provider = manageProvider;
  try {
    const data = await api.models.manage(action, model, provider);
    applyModelsResponse(provider, data);
    if (action === 'default' && provider === currentProvider) {
      currentModel = provider === 'custom' ? ocCustom.default_model : defaultModel;
    }
    ensureValidSelection();
    updateModelLabel();
    renderModels();
    updateEmptyState();
    elements.ocModelNote.textContent = data.message || '';
    toast(data.message || '模型清單已儲存');
    await refreshState();
  } catch (error) {
    toast('模型操作失敗：' + error.message, true);
  }
}

async function addModelFromInput() {
  const value = elements.ocModelInput.value.trim();
  if (!value) return;
  const provider = manageProvider;
  try {
    const data = await api.models.manage('add', value, provider);
    applyModelsResponse(provider, data);
    elements.ocModelInput.value = '';
    renderModels();
    updateEmptyState();
    elements.ocModelNote.textContent = data.message || '';
    toast(data.message || '已新增模型');
    await refreshState();
  } catch (error) {
    toast('新增模型失敗：' + error.message, true);
  }
}

// ─────────────────────────────────────────────────────────────
// opencode 面板（左側欄第三個分頁）
// ─────────────────────────────────────────────────────────────

const OPENCODE_FIELDS = ['ocEnabled', 'ocBaseUrl', 'ocApiKey', 'ocSaveBtn',
  'ocAccountInput', 'ocAddAccountBtn', 'ocSaveAccessBtn',
  'ocCustomName', 'ocCustomBaseUrl', 'ocCustomApiKey', 'ocCustomSaveBtn'];

async function loadOpencode() {
  try {
    await refreshState();
    renderOpencode();
    if (ocState && ocState.allowed && !threads.length) await loadSessions();
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
  elements.ocApiKey.value = '';
  elements.ocKeyHint.textContent = s.key_masked
    ? `目前 NVIDIA 金鑰：${s.key_masked}（留空不更動）`
    : '尚未設定 NVIDIA 金鑰';

  const custom = normalizeCustom(s.custom);
  elements.ocCustomName.value = (s.custom && s.custom.name) || '';
  elements.ocCustomBaseUrl.value = custom.base_url;
  elements.ocCustomApiKey.value = '';
  elements.ocCustomKeyHint.textContent = custom.key_masked
    ? `目前金鑰：${custom.key_masked}（留空不更動）`
    : '尚未設定金鑰';
  elements.ocCustomStatus.textContent = custom.configured
    ? '已設定，模型可在聊天選單使用'
    : '未設定（需要 Base URL 與 API Key）';

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
  elements.ocApiKey.disabled = !writable || !on;
  const customLocked = !writable || !on;
  elements.ocCustomName.disabled = customLocked;
  elements.ocCustomBaseUrl.disabled = customLocked;
  elements.ocCustomApiKey.disabled = customLocked;
  elements.ocCustomSaveBtn.disabled = customLocked;
}

async function saveOpencodeConfig() {
  const payload = {
    enabled: elements.ocEnabled.value === '1',
    base_url: elements.ocBaseUrl.value.trim()
  };
  const key = elements.ocApiKey.value.trim();
  if (key) payload.api_key = key;
  try {
    await api.config.save(payload);
    toast('opencode 設定已儲存，sidecar 將自動重啟');
    await loadOpencode();
  } catch (error) {
    toast('儲存失敗：' + error.message, true);
  }
}

/** 自訂供應商：名稱、Base URL、API Key（留空不更動）。 */
async function saveCustomProvider() {
  const payload = {
    name: elements.ocCustomName.value.trim(),
    base_url: elements.ocCustomBaseUrl.value.trim()
  };
  const key = elements.ocCustomApiKey.value.trim();
  if (key) payload.api_key = key;
  try {
    await api.custom.save(payload);
    toast('自訂供應商已儲存，sidecar 將自動重啟');
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
    ocState = await api.state();
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
    await api.config.saveAccess(accessList);
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
// 建議按鈕
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

  elements.ocModelAddBtn.addEventListener('click', addModelFromInput);
  elements.ocManageNvidia.addEventListener('click', () => setManageProvider('nvidia'));
  elements.ocManageCustom.addEventListener('click', () => setManageProvider('custom'));
  elements.ocCustomSaveBtn.addEventListener('click', saveCustomProvider);
  elements.ocModelInput.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      event.preventDefault();
      addModelFromInput();
    }
  });

  panels.setupTabs(elements.leftTabs, elements.leftContent, (target) => {
    if (target === 'opencode') loadOpencode();
    if (target === 'models') refreshState();
  });
  panels.setupTabs(elements.rightTabs, elements.rightContent);
}

async function init() {
  try {
    const savedTheme = localStorage.getItem('theme');
    if (savedTheme) document.documentElement.dataset.theme = savedTheme;
  } catch {
    // 無法讀取主題時使用預設值
  }

  collectElements();
  // 觸控裝置沒有 Enter 發送的概念（而且狹窄），佔位文字改短
  if (window.matchMedia('(hover: none)').matches && elements.composerInput) {
    elements.composerInput.placeholder = '輸入訊息…';
  }
  buildComponents();
  renderSuggestions();
  bindEvents();
  updateModelLabel();
  updateEmptyState();

  await refreshState();
  await loadSessions();

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
