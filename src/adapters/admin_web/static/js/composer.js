/**
 * 編輯器模組 - 處理輸入、附件、自動增長
 */

import { appState } from './state.js';
import { api } from './api.js';
import { el, clear } from './markdown.js';
import { icon } from './icons.js';

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
    const fileIcon = el('div', 'icon');
    fileIcon.innerHTML = icon('file', 20);
    chip.appendChild(fileIcon);
  }
  const box = el('div');
  box.style.minWidth = '0';
  box.appendChild(el('div', 'name', attachment.name));
  box.appendChild(el('div', 'size', humanSize(attachment.size)));
  chip.appendChild(box);
  const removeBtn = el('button', 'remove');
  removeBtn.innerHTML = icon('x', 14);
  removeBtn.type = 'button';
  removeBtn.title = '移除';
  removeBtn.addEventListener('click', onRemove);
  chip.appendChild(removeBtn);
  return chip;
}

export function createComposer(attachBtn, fileInput, dropzone, input, sendBtn, stopBtn, onSend, onStop) {
  let sending = false;
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
    // 輸入法組字中（e.isComposing）按 Enter 是選字，不能送出。
    if (e.key === 'Enter' && !e.isComposing && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      send();
    }
  }

  async function send() {
    // 生成中不接受新送出；先擋住，避免輸入框被清空而文字遺失。
    if (sending) return;
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
    sending = on;
    sendBtn.classList.toggle('hidden', on);
    stopBtn.classList.toggle('hidden', !on);
    input.disabled = on;
  }

  // 停止：交給外部（opencode.js）處理。舊版在這裡呼叫 stopBtn.click()，
  // 而 stopBtn 的 click 又綁到 stop()，會無限遞迴。
  function stop() {
    if (onStop) onStop();
  }
  function isStreaming() {
    return sending;
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
  fileInput.addEventListener('change', e => {
    // 先複製出清單再清空 input：否則選同一個檔案（移除晶片後再附加）瀏覽器不會再觸發 change。
    const picked = Array.from(e.target.files || []);
    e.target.value = '';
    addFiles(picked);
  });
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
    isStreaming,
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

export default { createComposer };