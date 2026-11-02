/**
 * 虛擬化時間軸模組 - 高效能訊息渲染
 */

import { appState } from './state.js';
import { renderMarkdown, el, clear } from './markdown.js';
import { icon } from './icons.js';

function createMessageElement(message) {
  const wrap = el('div', 'message ' + (message.role === 'user' ? 'user' : 'assistant'));
  const avatar = el('div', 'message-avatar', message.role === 'user' ? '你' : 'AI');
  wrap.appendChild(avatar);

  const body = el('div', 'message-body');
  const header = el('div', 'message-header');
  header.appendChild(el('span', 'message-role', message.role === 'user' ? '你' : 'AI 助理'));

  if (message.model) {
    header.appendChild(el('span', 'message-model', message.model));
  }
  if (message.keyName) {
    header.appendChild(el('span', 'message-model', message.keyName));
  }
  body.appendChild(header);

  const content = el('div', 'message-content');
  if (message.role === 'user') {
    const p = el('p');
    p.style.whiteSpace = 'pre-wrap';
    p.textContent = message.content;
    content.appendChild(p);
  } else {
    renderMarkdown(content, message.content);
  }
  body.appendChild(content);

  if (message.attachments?.length) {
    const attRow = el('div', 'message-attachments');
    message.attachments.forEach(name => {
      const chip = el('span');
      chip.innerHTML = icon('paperclip', 12) + ' ';
      chip.appendChild(document.createTextNode(name));
      attRow.appendChild(chip);
    });
    body.appendChild(attRow);
  }

  wrap.appendChild(body);
  wrap._content = content;
  return wrap;
}

function createStreamingMessage(messageId) {
  const wrap = el('div', 'message assistant streaming');
  wrap.dataset.messageId = messageId;
  const avatar = el('div', 'message-avatar', 'AI');
  wrap.appendChild(avatar);

  const body = el('div', 'message-body');
  const header = el('div', 'message-header');
  header.appendChild(el('span', 'message-role', 'AI 助理'));
  body.appendChild(header);

  const content = el('div', 'message-content');
  const thinking = el('div', 'thinking-indicator');
  thinking.innerHTML = '<span>思考中</span><div class="thinking-dots"><span></span><span></span><span></span></div>';
  content.appendChild(thinking);
  body.appendChild(content);

  wrap.appendChild(body);
  wrap._content = content;
  wrap._thinking = thinking;
  return wrap;
}

function createToolCallElement(toolCall) {
  const box = el('div', 'tool-call');
  const header = el('div', 'tool-call-header');
  header.appendChild(el('span', 'tool-call-name', toolCall.name));
  if (toolCall.status) {
    header.appendChild(el('span', 'badge badge-' + (toolCall.status === 'success' ? 'success' : toolCall.status === 'error' ? 'error' : 'warning'), toolCall.status));
  }
  box.appendChild(header);

  if (toolCall.args) {
    const args = el('pre', 'tool-call-args');
    args.textContent = JSON.stringify(toolCall.args, null, 2);
    box.appendChild(args);
  }
  if (toolCall.result) {
    const result = el('pre', 'tool-call-result');
    result.textContent = toolCall.result;
    box.appendChild(result);
  }
  return box;
}

export function createTimeline(container, options = {}) {
  const {
    onScroll,
    onLoadMore,
    loadMoreThreshold = 100,
    maxMessages = 100
  } = options;

  let messages = [];
  let messageElements = new Map();
  let streamingMessage = null;
  let isAtBottom = true;
  let observer = null;

  function isNearBottom() {
    const { scrollTop, scrollHeight, clientHeight } = container;
    return scrollHeight - scrollTop - clientHeight < loadMoreThreshold;
  }

  function checkScroll() {
    const wasAtBottom = isAtBottom;
    isAtBottom = isNearBottom();
    if (onScroll) onScroll(isAtBottom);
    if (!wasAtBottom && isAtBottom && onLoadMore) {
      onLoadMore();
    }
  }

  function scrollToBottom(behavior = 'smooth') {
    container.scrollTop = container.scrollHeight;
  }

  function addMessage(message) {
    messages.push(message);
    if (messages.length > maxMessages) {
      const removed = messages.shift();
      const el = messageElements.get(removed.id);
      if (el) { el.remove(); messageElements.delete(removed.id); }
    }
    renderMessage(message);
    if (isAtBottom) scrollToBottom();
  }

  function renderMessage(message) {
    const element = createMessageElement(message);
    messageElements.set(message.id, element);
    container.appendChild(element);
    return element;
  }

  function startStreaming(messageId, initialContent = '') {
    streamingMessage = createStreamingMessage(messageId);
    container.appendChild(streamingMessage);
    if (isAtBottom) scrollToBottom();
    return streamingMessage;
  }

  function updateStreaming(content) {
    if (!streamingMessage) return;
    if (streamingMessage._thinking) {
      streamingMessage._thinking.remove();
      streamingMessage._thinking = null;
    }
    clear(streamingMessage._content);
    renderMarkdown(streamingMessage._content, content);
    if (isAtBottom) scrollToBottom();
  }

  function endStreaming(finalMessage) {
    if (streamingMessage) {
      streamingMessage.remove();
      streamingMessage = null;
    }
    if (finalMessage) {
      addMessage(finalMessage);
    }
  }

  function appendStreamingChunk(chunk) {
    if (!streamingMessage) return;
    if (streamingMessage._thinking) {
      streamingMessage._thinking.remove();
      streamingMessage._thinking = null;
    }
    streamingMessage._content.textContent += chunk;
    if (isAtBottom) scrollToBottom();
  }

  function showError(messageId, error) {
    // 串流中的泡泡不在 messageElements 內，所以也要接受 streamingMessage。
    const element = messageElements.get(messageId) || streamingMessage;
    if (!element || !element._content) return;
    const errorBox = el('div', 'errbox');
    const errHead = el('div', 'errbox-h');
    errHead.innerHTML = icon('alert-triangle', 14) + ' ';
    errHead.appendChild(document.createTextNode('生成失敗'));
    errorBox.appendChild(errHead);
    errorBox.appendChild(el('div', 'errbox-m', (error && error.message) || '未知錯誤'));
    element._content.appendChild(errorBox);
  }
  function replaceMessage(messageId, newMessage) {
    const oldElement = messageElements.get(messageId);
    if (oldElement) {
      oldElement.remove();
      messageElements.delete(messageId);
    }
    messages = messages.map(m => m.id === messageId ? newMessage : m);
    renderMessage(newMessage);
  }

  function clearAll() {
    messages = [];
    messageElements.forEach(el => el.remove());
    messageElements.clear();
    if (streamingMessage) { streamingMessage.remove(); streamingMessage = null; }
  }

  function getMessages() {
    return [...messages];
  }

  function getMessage(messageId) {
    return messages.find(m => m.id === messageId);
  }

  function setMessages(newMessages) {
    clearAll();
    messages = newMessages.slice(-maxMessages);
    messages.forEach(m => renderMessage(m));
    if (isAtBottom) scrollToBottom();
  }

  // 設定滾動觀察
  container.addEventListener('scroll', checkScroll, { passive: true });

  // IntersectionObserver for load more
  if (onLoadMore) {
    const sentinel = el('div', 'load-more-sentinel');
    sentinel.style.height = '1px';
    container.insertBefore(sentinel, container.firstChild);
    observer = new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting) onLoadMore();
    }, { root: container, rootMargin: '100px' });
    observer.observe(sentinel);
  }

  function destroy() {
    container.removeEventListener('scroll', checkScroll);
    if (observer) observer.disconnect();
    clearAll();
  }

  return {
    addMessage,
    startStreaming,
    updateStreaming,
    appendStreamingChunk,
    endStreaming,
    showError,
    replaceMessage,
    clearAll,
    setMessages,
    getMessages,
    getMessage,
    scrollToBottom,
    isAtBottom: () => isAtBottom,
    destroy
  };
}

function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

export function createEmptyState(container, options = {}) {
  const {
    title = '歡迎使用 AI 對話',
    description = '選擇模型並開始輸入訊息',
    suggestions = [],
    onSuggestionClick
  } = options;

  const emptyState = el('div', 'empty-state flex-1');
  const emptyIcon = el('div', 'empty-state-icon');
  emptyIcon.innerHTML = icon('message-square', 26);
  emptyState.appendChild(emptyIcon);
  emptyState.appendChild(el('div', 'empty-state-title', title));
  emptyState.appendChild(el('div', 'empty-state-desc', description));

  if (suggestions.length) {
    const suggContainer = el('div', 'flex gap-2 mt-4 flex-wrap justify-center');
    suggestions.forEach(text => {
      const btn = el('button', 'btn btn-secondary btn-sm', text);
      btn.type = 'button';
      btn.addEventListener('click', () => onSuggestionClick?.(text));
      suggContainer.appendChild(btn);
    });
    emptyState.appendChild(suggContainer);
  }

  container.appendChild(emptyState);
  return emptyState;
}

export function removeEmptyState(element) {
  element?.remove();
}

export default { createTimeline, createEmptyState, removeEmptyState };