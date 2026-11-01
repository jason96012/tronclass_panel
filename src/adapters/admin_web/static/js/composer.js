/**
 * 編輯器模組 - 處理輸入、附件、自動增長
 */

import { appState } from './state.js';
import { api } from './api.js';
import { el, clear } from './markdown.js';

const MAX_FILES = 6;
const MAX_IMG_SIZE = 4 * 1024 * 1024;
const MAX_TXT_SIZE = 256 * 1024;

const IMAGE_MIME = {
  'image/png': true, 'image/jpeg': true, 'image/jpg': true,
  'image/webp': true, 'image/gif': true
};

const TEXT_EXT = ['txt', 'md', 'markdown', 'csv', 'tsv', 'json', 'log', 'xml', 'yml', 'yaml', 'ini', 'conf', 'cfg',
  'sql', 'js', 'mjs', 'ts', 'jsx', 'tsx', 'py', 'rs', 'java', 'kt', 'go', 'rb', 'php', 'c', 'h', 'cpp',
  'hpp', 'cs', 'css', 'scss', 'html', 'htm', 'sh', 'bash', 'env', 'toml', 'properties'];

function extOf(name) {
  const i = String(name || '').lastIndexOf('.');
  return i < 0 ? '' : String(name).slice(i + 1).toLowerCase();
}

function humanSize(n) {
  if (n < 1024) return n + ' B';
  if (n < 1024 * 1024) return (n / 1024).toFixed(0) + ' KB';
  return (n / 1024 / 1024).toFixed(1) + ' MB';
}

function classify(file) {
  const mime = (file.type || '').toLowerCase();
  if (IMAGE_MIME[mime]) return 'image';
  if (mime.startsWith('image/')) return 'bad-image';
  if (mime.startsWith('text/')) return 'text';
  if (TEXT_EXT.includes(extOf(file.name))) return 'text';
  if (mime === 'application/json' || mime === 'application/xml') return 'text';
  return 'unknown';
}

function createAttachmentChip(attachment, onRemove) {
  const chip = el('div', 'attachment-chip');
  if (attachment.kind === 'image') {
    const img = document.createElement('img');
    img.src = attachment.preview;
    img.alt = attachment.name;
    chip.appendChild(img);
  } else {
    chip.appendChild(el('div', 'icon', '📄'));
  }
  const box = el('div');
  box.style.minWidth = '0';
  box.appendChild(el('div', 'name', attachment.name));
  box.appendChild(el('div', 'size', humanSize(attachment.size)));
  chip.appendChild(box);
  const removeBtn = el('button', 'remove', '✕');
  removeBtn.type = 'button';
  removeBtn.title = '移除';
  removeBtn.addEventListener('click', onRemove);
  chip.appendChild(removeBtn);
  return chip;
}

export function createComposer(attachBtn, fileInput, dropzone, input, sendBtn, stopBtn, onSend) {
  let pendingAttachments = [];

  function renderAttachments() {
    clear(dropzone);
    pendingAttachments.forEach((att, idx) => {
      dropzone.appendChild(createAttachmentChip(att, () => {
        pendingAttachments.splice(idx, 1);
        renderAttachments();
      }));
    });
  }

  function autoGrow() {
    input.style.height = 'auto';
    input.style.height = Math.min(input.scrollHeight, 200) + 'px';
  }

  function addFiles(files) {
    const list = Array.from(files || []);
    if (!list.length) return;
    list.forEach(file => {
      if (pendingAttachments.length >= MAX_FILES) {
        showToast(`一次最多只能附 ${MAX_FILES} 個檔案`, true);
        return;
      }
      const kind = classify(file);
      if (kind === 'bad-image') {
        showToast(`「${file.name}」的圖片格式不支援，請用 PNG／JPG／WebP／GIF`, true);
        return;
      }
      if (kind === 'unknown') {
        showToast(`「${file.name}」不支援。目前只能附圖片和純文字類檔案`, true);
        return;
      }
      if (kind === 'image' && file.size > MAX_IMG_SIZE) {
        showToast(`「${file.name}」有 ${humanSize(file.size)}，圖片上限 4 MB`, true);
        return;
      }
      if (kind === 'text' && file.size > MAX_TXT_SIZE) {
        showToast(`「${file.name}」有 ${humanSize(file.size)}，文字檔上限 256 KB`, true);
        return;
      }

      const slot = {
        name: file.name || (kind === 'image' ? '貼上的圖片.png' : '附件.txt'),
        mime: (file.type || '').toLowerCase(),
        kind,
        size: file.size,
        data: '',
        text: '',
        preview: '',
        ready: false
      };
      pendingAttachments.push(slot);
      renderAttachments();

      const reader = new FileReader();
      reader.onload = () => {
        const out = String(reader.result || '');
        if (kind === 'image') {
          slot.preview = out;
          const cut = out.indexOf('base64,');
          slot.data = cut >= 0 ? out.slice(cut + 7) : '';
          if (!slot.mime) slot.mime = 'image/png';
        } else {
          slot.text = out;
          if (!out.trim()) {
            pendingAttachments.splice(pendingAttachments.indexOf(slot), 1);
            renderAttachments();
            showToast(`「${slot.name}」是空的`, true);
            return;
          }
        }
        slot.ready = true;
        renderAttachments();
      };
      reader.onerror = () => {
        pendingAttachments.splice(pendingAttachments.indexOf(slot), 1);
        renderAttachments();
        showToast(`「${slot.name}」讀取失敗`, true);
      };
      if (kind === 'image') reader.readAsDataURL(file);
      else reader.readAsText(file);
    });
  }

  function handlePaste(e) {
    const items = e.clipboardData?.items;
    if (!items) return;
    const files = [];
    for (const item of items) {
      if (item.kind === 'file') files.push(item.getAsFile());
    }
    if (files.length) {
      e.preventDefault();
      addFiles(files);
    }
  }

  function handleDrop(e) {
    e.preventDefault();
    e.stopPropagation();
    dropzone.classList.remove('dragging');
    const files = Array.from(e.dataTransfer?.files || []);
    if (files.length) addFiles(files);
  }

  function handleDragOver(e) {
    e.preventDefault();
    e.stopPropagation();
    dropzone.classList.add('dragging');
  }

  function handleDragLeave(e) {
    e.preventDefault();
    e.stopPropagation();
    dropzone.classList.remove('dragging');
  }

  function handleKeyDown(e) {
    if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      send();
    }
  }

  async function send() {
    const text = input.value.trim();
    if (!text && !pendingAttachments.length) return;
    if (pendingAttachments.some(a => !a.ready)) {
      showToast('檔案還在讀取，請稍候再送出', true);
      return;
    }

    const attachments = pendingAttachments.map(a => a.kind === 'image'
      ? { name: a.name, mime: a.mime, kind: 'image', data: a.data }
      : { name: a.name, mime: a.mime || 'text/plain', kind: 'text', text: a.text });
    const attachmentNames = pendingAttachments.map(a => a.name);
    pendingAttachments = [];
    renderAttachments();

    input.value = '';
    autoGrow();

    setSending(true);
    try {
      await onSend(text, attachments, attachmentNames);
    } finally {
      setSending(false);
    }
  }

  function setSending(on) {
    sendBtn.classList.toggle('hidden', on);
    stopBtn.classList.toggle('hidden', !on);
    input.disabled = on;
  }

  function stop() {
    // 觸發停止邏輯
    stopBtn.click();
  }

  function reset() {
    input.value = '';
    pendingAttachments = [];
    renderAttachments();
    autoGrow();
    setSending(false);
  }

  function getValue() {
    return input.value;
  }

  function setValue(value) {
    input.value = value;
    autoGrow();
  }

  function focus() {
    input.focus();
  }

  function getAttachments() {
    return [...pendingAttachments];
  }

  function clearAttachments() {
    pendingAttachments = [];
    renderAttachments();
  }

  // 綁定事件
  attachBtn.addEventListener('click', () => fileInput.click());
  fileInput.addEventListener('change', e => addFiles(e.target.files));
  input.addEventListener('input', autoGrow);
  input.addEventListener('keydown', handleKeyDown);
  input.addEventListener('paste', handlePaste);
  dropzone.addEventListener('dragover', handleDragOver);
  dropzone.addEventListener('dragleave', handleDragLeave);
  dropzone.addEventListener('drop', handleDrop);
  sendBtn.addEventListener('click', send);
  stopBtn.addEventListener('click', stop);

  autoGrow();

  return {
    send,
    stop,
    reset,
    getValue,
    setValue,
    focus,
    getAttachments,
    clearAttachments,
    addFiles,
    autoGrow
  };
}

function showToast(message, isError) {
  const container = document.getElementById('toastContainer');
  if (!container) return;
  const toast = el('div', 'toast' + (isError ? ' error' : ''), message);
  container.appendChild(toast);
  setTimeout(() => toast.remove(), 3000);
}

export function createOpencodeComposer(input, sendBtn, stopBtn, onSend) {
  let streaming = false;

  function autoGrow() {
    input.style.height = 'auto';
    input.style.height = Math.min(input.scrollHeight, 200) + 'px';
  }

  function handleKeyDown(e) {
    if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      send();
    }
  }

  async function send() {
    const text = input.value.trim();
    if (!text) return;

    input.value = '';
    autoGrow();
    setSending(true);

    try {
      await onSend(text);
    } finally {
      setSending(false);
    }
  }

  function setSending(on) {
    streaming = on;
    sendBtn.classList.toggle('hidden', on);
    stopBtn.classList.toggle('hidden', !on);
    input.disabled = on;
  }

  function stop() {
    // 觸發停止邏輯
  }

  function reset() {
    input.value = '';
    autoGrow();
    setSending(false);
  }

  input.addEventListener('input', autoGrow);
  input.addEventListener('keydown', handleKeyDown);
  sendBtn.addEventListener('click', send);
  stopBtn.addEventListener('click', stop);
  autoGrow();

  return { send, stop, reset, autoGrow, setSending, focus: () => input.focus() };
}

export default { createComposer, createOpencodeComposer };