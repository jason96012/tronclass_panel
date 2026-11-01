/**
 * API 模組 - 後端通訊層
 */

const API_BASE = '/admin/api';

async function request(path, options = {}) {
  const url = `${API_BASE}${path}`;
  const config = {
    credentials: 'same-origin',
    headers: {
      'Content-Type': 'application/json',
      ...options.headers
    },
    ...options
  };

  if (config.body && typeof config.body === 'object') {
    config.body = JSON.stringify(config.body);
  }

  const response = await fetch(url, config);
  const text = await response.text();

  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }

  if (!response.ok) {
    const error = new Error(data?.error || `HTTP ${response.status}`);
    error.status = response.status;
    error.code = data?.code;
    error.data = data;
    throw error;
  }

  return data;
}

export const api = {
  // 聊天相關
  chat: {
    send: (payload) => request('/ai/chat', { method: 'POST', body: payload }),
    poll: (jobId) => request(`/ai/chat/poll/${jobId}`),
    stop: (jobId) => request(`/ai/chat/stop/${jobId}`, { method: 'POST' }),
    stream: (payload) => {
      return new Promise((resolve, reject) => {
        const controller = new AbortController();
        fetch(`${API_BASE}/ai/chat/stream`, {
          method: 'POST',
          credentials: 'same-origin',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(payload),
          signal: controller.signal
        }).then(response => {
          if (!response.ok) {
            response.text().then(text => {
              try { reject(JSON.parse(text)); } catch { reject(new Error(text || `HTTP ${response.status}`)); }
            });
            return;
          }
          const reader = response.body.getReader();
          const decoder = new TextDecoder();
          let buffer = '';
          const read = () => reader.read().then(({ done, value }) => {
            if (done) { resolve(); return; }
            buffer += decoder.decode(value, { stream: true });
            const lines = buffer.split('\n');
            buffer = lines.pop();
            lines.forEach(line => {
              if (line.startsWith('data: ')) {
                try {
                  const data = JSON.parse(line.slice(6));
                  resolve(data);
                } catch { }
              }
            });
            read();
          });
          read();
        }).catch(reject);
        return { abort: () => controller.abort() };
      });
    }
  },

  // 會話/線程管理
  threads: {
    list: () => request('/ai/threads'),
    get: (id) => request(`/ai/threads/${id}`),
    create: (title) => request('/ai/threads', { method: 'POST', body: { action: 'create', title } }),
    delete: (id) => request('/ai/threads', { method: 'POST', body: { action: 'delete', id } }),
    rename: (id, title) => request('/ai/threads', { method: 'POST', body: { action: 'rename', id, title } }),
    clearAll: () => request('/ai/threads', { method: 'POST', body: { action: 'clear_all' } })
  },

  // 模型管理
  models: {
    list: () => request('/ai/models'),
    add: (payload) => request('/ai/models', { method: 'POST', body: { action: 'add', ...payload } }),
    remove: (id) => request('/ai/models', { method: 'POST', body: { action: 'remove', id } }),
    enable: (id, enabled) => request('/ai/models', { method: 'POST', body: { action: 'enable', id, enabled } }),
    bulkAdd: (payload) => request('/ai/models', { method: 'POST', body: { action: 'bulk_add', ...payload } }),
    clean: () => request('/ai/models', { method: 'POST', body: { action: 'clean' } }),
    removeOrphans: () => request('/ai/models', { method: 'POST', body: { action: 'remove_orphans' } }),
    reset: () => request('/ai/models', { method: 'POST', body: { action: 'reset' } })
  },

  // API 金鑰管理
  keys: {
    list: () => request('/ai/keys'),
    add: (payload) => request('/ai/keys', { method: 'POST', body: { action: 'add', ...payload } }),
    update: (id, payload) => request('/ai/keys', { method: 'POST', body: { action: 'update', id, ...payload } }),
    delete: (id) => request('/ai/keys', { method: 'POST', body: { action: 'delete', id } }),
    reorder: (ids) => request('/ai/keys', { method: 'POST', body: { action: 'reorder', ids } }),
    test: (id) => request('/ai/keys', { method: 'POST', body: { action: 'test', id } })
  },

  // 設定
  settings: {
    get: () => request('/ai/settings'),
    save: (payload) => request('/ai/settings', { method: 'POST', body: payload }),
    saveTokens: (payload) => request('/ai/settings/tokens', { method: 'POST', body: payload })
  },

  // 供應商
  providers: {
    list: () => request('/ai/providers')
  },

  // opencode 專用
  opencode: {
    state: () => request('/opencode/state'),
    config: () => request('/opencode/config'),
    saveConfig: (payload) => request('/opencode/config', { method: 'POST', body: payload }),
    access: () => request('/opencode/access'),
    saveAccess: (accounts) => request('/opencode/access', { method: 'POST', body: { accounts } }),
    chat: (payload) => request('/opencode/chat', { method: 'POST', body: payload })
  },

  // 通用
  health: () => request('/health'),
  version: () => request('/version')
};

export async function uploadFile(file) {
  const formData = new FormData();
  formData.append('file', file);
  const response = await fetch(`${API_BASE}/ai/upload`, {
    method: 'POST',
    credentials: 'same-origin',
    body: formData
  });
  if (!response.ok) throw new Error('上傳失敗');
  return response.json();
}

export function createApiClient(baseUrl = API_BASE) {
  return {
    request: (path, options) => request(path, options),
    chat: api.chat,
    threads: api.threads,
    models: api.models,
    keys: api.keys,
    settings: api.settings,
    providers: api.providers,
    opencode: api.opencode
  };
}

export default api;