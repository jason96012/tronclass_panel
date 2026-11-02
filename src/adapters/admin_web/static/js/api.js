/**
 * API 模組 - 後端通訊層（AI 頁面）
 *
 * 所有路徑都對應 src/adapters/admin_web/mod.rs 內實際註冊的路由。
 * 認證靠 same-origin cookie，不需要自己帶 token。
 *
 * 路由對照（方法 + 路徑 → 後端 handler）：
 *   GET  /admin/api/ai/state                 → ai_chat::state_handler（keys/settings/threads/models）
 *   POST /admin/api/ai/chat                  → ai_chat::chat_handler（202 + job_id）
 *   GET  /admin/api/ai/chat/status?job=      → ai_chat::chat_status_handler（完整文字 text）
 *   GET  /admin/api/ai/chat/stream?job=      → ai_chat::chat_stream_handler（SSE：meta/step/delta/done/gone）
 *   POST /admin/api/ai/chat/stop             → ai_chat::chat_stop_handler（body: { job }）
 *   POST /admin/api/ai/threads               → ai_chat::threads_handler（action: get/rename/delete/clear）
 *   GET  /admin/api/ai/opencode/state        → opencode::state_handler
 *   POST /admin/api/ai/opencode/config       → opencode::config_handler（僅管理員）
 *   POST /admin/api/ai/opencode/access       → opencode::access_handler（body: { allowed_accounts }，僅管理員）
 */

const API_BASE = '/admin/api/ai';

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
    const error = new Error((data && (data.error || data.message)) || `HTTP ${response.status}`);
    error.status = response.status;
    error.code = data && data.code;
    error.data = data;
    throw error;
  }
  return data;
}

export const api = {
  /** 初始資料：threads 摘要、模型清單、設定、權限旗標。 */
  state: () => request('/state'),

  chat: {
    /** 送出訊息。回傳 { job_id, thread_id, model_id, auto_reason, poll_ms }。 */
    send: (payload) => request('/chat', { method: 'POST', body: payload }),
    /** 輪詢完整狀態（SSE 不可用時的後備）。 */
    status: (jobId) => request(`/chat/status?job=${encodeURIComponent(jobId)}`),
    /** SSE 端點網址（給 EventSource 用，需同源 cookie）。 */
    streamUrl: (jobId) => `${API_BASE}/chat/stream?job=${encodeURIComponent(jobId)}`,
    stop: (jobId) => request('/chat/stop', { method: 'POST', body: { job: jobId } })
  },

  threads: {
    /** 讀取單一對話完整內容（含 messages）。回傳 { ok, thread }。 */
    get: (id) => request('/threads', { method: 'POST', body: { action: 'get', id } }),
    rename: (id, title) => request('/threads', { method: 'POST', body: { action: 'rename', id, title } }),
    delete: (id) => request('/threads', { method: 'POST', body: { action: 'delete', id } }),
    clear: () => request('/threads', { method: 'POST', body: { action: 'clear' } })
  },

  opencode: {
    state: () => request('/opencode/state'),
    saveConfig: (payload) => request('/opencode/config', { method: 'POST', body: payload }),
    saveAccess: (allowedAccounts) =>
      request('/opencode/access', { method: 'POST', body: { allowed_accounts: allowedAccounts } })
  }
};

export default api;
