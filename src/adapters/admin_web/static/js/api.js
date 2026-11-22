/**
 * API 模組 - 後端通訊層（AI 頁面）
 *
 * AI 頁面只使用 opencode（OPENCODE_AGENT_ONLY_V2）。所有路徑都對應
 * src/adapters/admin_web/mod.rs 內實際註冊的路由，實作在 opencode.rs。
 * 認證靠 same-origin cookie，不需要自己帶 token。
 *
 * 路由對照（方法 + 路徑 → 後端 handler）：
 *   GET  /admin/api/ai/opencode/state      → opencode::state_handler（設定、模型清單、權限）
 *   POST /admin/api/ai/opencode/config     → opencode::config_handler（僅管理員）
 *   POST /admin/api/ai/opencode/access     → opencode::access_handler（僅管理員）
 *   POST /admin/api/ai/opencode/models     → opencode::models_handler（add/remove/default，僅管理員）
 *   POST /admin/api/ai/opencode/custom     → opencode::custom_provider_handler（自訂供應商，僅管理員）
 *   GET  /admin/api/ai/opencode/sessions   → opencode::sessions_list_handler
 *   POST /admin/api/ai/opencode/sessions   → opencode::sessions_action_handler（messages/rename/delete）
 *   POST /admin/api/ai/opencode/chat       → opencode::chat_handler（回 202 與 { job_id, thread_id }，結果用 chat/status 輪詢）
 *   GET  /admin/api/ai/opencode/chat/status?job=… → opencode::chat_status_handler（{ state: running|done|failed, text, model, error, code }）
 *   POST /admin/api/ai/opencode/abort      → opencode::abort_handler
 */

const API_BASE = '/admin/api/ai/opencode';

async function request(path, { method = 'GET', body } = {}) {
  const init = { method, credentials: 'same-origin', headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const response = await fetch(`${API_BASE}${path}`, init);
  const text = await response.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!response.ok) {
    const error = new Error((data && (data.message || data.error)) || `HTTP ${response.status}`);
    error.status = response.status;
    error.code = data && data.error;
    error.data = data;
    throw error;
  }
  return data;
}

export const api = {
  /** 設定、模型清單、權限旗標。 */
  state: () => request('/state'),

  config: {
    save: (payload) => request('/config', { method: 'POST', body: payload }),
    saveAccess: (allowedAccounts) =>
      request('/access', { method: 'POST', body: { allowed_accounts: allowedAccounts } })
  },

  models: {
    /** action: 'add' | 'remove' | 'default'；provider: 'nvidia'（預設）| 'custom' */
    manage: (action, model, provider = 'nvidia') =>
      request('/models', { method: 'POST', body: { action, model, provider } })
  },

  /** 自訂 OpenAI 相容供應商：name、base_url、api_key（留空不更動）。停用請送空的 base_url。 */
  custom: {
    save: (payload) => request('/custom', { method: 'POST', body: payload })
  },

  sessions: {
    list: () => request('/sessions'),
    messages: (threadId) => request('/sessions', { method: 'POST', body: { action: 'messages', thread_id: threadId } }),
    rename: (threadId, title) => request('/sessions', { method: 'POST', body: { action: 'rename', thread_id: threadId, title } }),
    delete: (threadId) => request('/sessions', { method: 'POST', body: { action: 'delete', thread_id: threadId } })
  },

  /** 送出一則訊息：只回 202 與 job_id（模型回覆可能需要好幾分鐘，不能掛在同一個 HTTP 請求上）。 */
  chat: (payload) => request('/chat', { method: 'POST', body: payload }),
  /** 查詢背景回覆的狀態。 */
  chatStatus: (jobId) => request(`/chat/status?job=${encodeURIComponent(jobId)}`),
  abort: (threadId) => request('/abort', { method: 'POST', body: { thread_id: threadId } })
};

export default api;
