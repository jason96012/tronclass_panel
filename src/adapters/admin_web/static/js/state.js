/**
 * 狀態管理模組 - 基於信號/存儲模式的輕量級響應式狀態
 */

export function createSignal(initialValue) {
  let value = initialValue;
  const subscribers = new Set();
  const read = () => {
    if (currentSubscriber) subscribers.add(currentSubscriber);
    return value;
  };
  const write = (newValue) => {
    value = typeof newValue === 'function' ? newValue(value) : newValue;
    subscribers.forEach(fn => fn());
  };
  return [read, write];
}

let currentSubscriber = null;
export function createEffect(fn) {
  const run = () => {
    currentSubscriber = run;
    fn();
    currentSubscriber = null;
  };
  run();
  return () => {};
}

export function createStore(initialState) {
  const state = { ...initialState };
  const subscribers = new Set();
  const proxy = new Proxy(state, {
    get(target, prop) {
      if (currentSubscriber) subscribers.add(currentSubscriber);
      return target[prop];
    },
    set(target, prop, value) {
      target[prop] = value;
      subscribers.forEach(fn => fn());
      return true;
    }
  });
  const setState = (partial) => {
    Object.assign(state, typeof partial === 'function' ? partial(state) : partial);
    subscribers.forEach(fn => fn());
  };
  return [proxy, setState];
}

export function createMemo(fn) {
  let cached;
  let dirty = true;
  const [signal, setSignal] = createSignal(undefined);
  createEffect(() => {
    dirty = true;
    setSignal(undefined);
  });
  return () => {
    if (dirty) {
      cached = fn();
      dirty = false;
    }
    signal();
    return cached;
  };
}

export function batch(fn) {
  fn();
}

export function untrack(fn) {
  const prev = currentSubscriber;
  currentSubscriber = null;
  try {
    return fn();
  } finally {
    currentSubscriber = prev;
  }
}

export function onCleanup(fn) {
  if (typeof window !== 'undefined') {
    window.addEventListener('beforeunload', fn, { once: true });
  }
}

export function createResource(fetcher) {
  const [data, setData] = createSignal(undefined);
  const [error, setError] = createSignal(null);
  const [loading, setLoading] = createSignal(true);
  const [latest, setLatest] = createSignal(0);

  const load = async (...args) => {
    const requestId = latest() + 1;
    setLatest(requestId);
    setLoading(true);
    setError(null);
    try {
      const result = await fetcher(...args);
      if (requestId === latest()) {
        setData(result);
        setLoading(false);
      }
      return result;
    } catch (e) {
      if (requestId === latest()) {
        setError(e);
        setLoading(false);
      }
      throw e;
    }
  };

  load();

  return {
    data: () => data(),
    error: () => error(),
    loading: () => loading(),
    latest: () => latest(),
    refetch: load,
    mutate: setData
  };
}

export function createDebouncedSignal(initialValue, delay = 300) {
  const [value, setValue] = createSignal(initialValue);
  let timeoutId = null;
  const setDebounced = (newValue) => {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => setValue(newValue), delay);
  };
  return [value, setValue, setDebounced];
}

export function createLocalStorageSignal(key, initialValue) {
  let stored = initialValue;
  try {
    const item = localStorage.getItem(key);
    if (item !== null) stored = JSON.parse(item);
  } catch { }
  const [value, setValue] = createSignal(stored);
  const write = (newValue) => {
    const val = typeof newValue === 'function' ? newValue(value()) : newValue;
    try {
      localStorage.setItem(key, JSON.stringify(val));
    } catch { }
    setValue(val);
  };
  return [value, write];
}

export function createSessionStorageSignal(key, initialValue) {
  let stored = initialValue;
  try {
    const item = sessionStorage.getItem(key);
    if (item !== null) stored = JSON.parse(item);
  } catch { }
  const [value, setValue] = createSignal(stored);
  const write = (newValue) => {
    const val = typeof newValue === 'function' ? newValue(value()) : newValue;
    try {
      sessionStorage.setItem(key, JSON.stringify(val));
    } catch { }
    setValue(val);
  };
  return [value, write];
}

export function onMount(fn) {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', fn, { once: true });
  } else {
    fn();
  }
}

export function onUnmount(fn) {
  window.addEventListener('beforeunload', fn, { once: true });
}

export function makeEventListener(target, event, handler, options) {
  target.addEventListener(event, handler, options);
  return () => target.removeEventListener(event, handler, options);
}

export function createComputed(fn) {
  return createMemo(fn);
}

export function mergeSignals(...signals) {
  const [value, setValue] = createSignal(signals.map(s => s()));
  signals.forEach(s => createEffect(() => setValue(signals.map(sig => sig()))));
  return value;
}

export function createEventEmitter() {
  const listeners = new Map();
  return {
    on(event, fn) {
      if (!listeners.has(event)) listeners.set(event, new Set());
      listeners.get(event).add(fn);
      return () => this.off(event, fn);
    },
    off(event, fn) {
      listeners.get(event)?.delete(fn);
    },
    emit(event, ...args) {
      listeners.get(event)?.forEach(fn => fn(...args));
    },
    once(event, fn) {
      const off = this.on(event, (...args) => {
        off();
        fn(...args);
      });
      return off;
    }
  };
}

const rawAppState = {
  sessions: createSignal([]),
  currentSessionId: createSignal(null),
  messages: createSignal([]),
  models: createSignal([]),
  currentModel: createSignal('auto'),
  providers: createSignal([]),
  keys: createSignal([]),
  settings: createSignal({}),
  opencode: createSignal({
    enabled: false,
    healthy: false,
    allowed: false,
    canManage: false,
    baseUrl: '',
    model: '',
    keyMasked: '',
    allowedAccounts: []
  }),
  ui: createSignal({
    leftSidebarOpen: false,
    rightSidebarOpen: false,
    bottomPanelOpen: false,
    commandPaletteOpen: false,
    modelPickerOpen: false,
    isMobile: window.innerWidth <= 900
  }),
  composer: createSignal({
    text: '',
    attachments: [],
    isComposing: false
  }),
  streaming: createSignal({
    active: false,
    messageId: null,
    buffer: ''
  })
};

// 每個欄位包成函式：appState.x() 讀取、appState.x(v) 或 appState.x(fn) 寫入。
// （createSignal 本身回傳 [read, write]，直接呼叫會是 TypeError。）
function asSignal([read, write]) {
  const fn = (...args) => (args.length ? write(args[0]) : read());
  fn.get = read;
  fn.set = write;
  return fn;
}

export const appState = Object.fromEntries(
  Object.entries(rawAppState).map(([key, signal]) => [key, asSignal(signal)])
)

export function getState() {
  const state = {};
  for (const [key, signal] of Object.entries(appState)) {
    state[key] = typeof signal === 'function' ? signal() : signal;
  }
  return state;
}

export function subscribe(key, fn) {
  if (appState[key]) {
    createEffect(() => fn(appState[key]()));
  }
}