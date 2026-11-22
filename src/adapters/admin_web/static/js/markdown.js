/**
 * Markdown 渲染模組 - 安全的 CSP 相容實作
 * 使用 createElement/textContent 避免 innerHTML 注入風險
 */

export function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = String(text);
  return node;
}

export function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

const INLINE_PATTERN = /(`[^`\n]+`)|(\*\*[^*]+\*\*)|(\*[^*\n]+\*)|(\[[^\]\n]+\]\([^)\s]+\))/;

function renderInline(parent, text) {
  let rest = String(text);
  while (rest.length) {
    const match = INLINE_PATTERN.exec(rest);
    if (!match) {
      parent.appendChild(document.createTextNode(rest));
      break;
    }
    if (match.index > 0) {
      parent.appendChild(document.createTextNode(rest.slice(0, match.index)));
    }
    const token = match[0];
    if (token.startsWith('`')) {
      parent.appendChild(el('code', 'inline', token.slice(1, -1)));
    } else if (token.startsWith('**')) {
      parent.appendChild(el('strong', null, token.slice(2, -2)));
    } else if (token.startsWith('*')) {
      parent.appendChild(el('em', null, token.slice(1, -1)));
    } else {
      const split = token.indexOf('](');
      const label = token.slice(1, split);
      const href = token.slice(split + 2, -1);
      const link = el('a', null, label);
      if (/^https?:\/\//i.test(href)) {
        link.href = href;
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
      }
      parent.appendChild(link);
    }
    rest = rest.slice(match.index + token.length);
  }
}

function createCodeBlock(lang, code) {
  const box = el('div', 'codeblk');
  const head = el('div', 'codehead');
  head.appendChild(el('span', 'lang', lang || 'code'));
  const copyBtn = el('button', 'copybtn', '複製');
  copyBtn.type = 'button';
  copyBtn.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(code);
      copyBtn.textContent = '已複製';
      setTimeout(() => copyBtn.textContent = '複製', 1400);
    } catch {
      copyBtn.textContent = '失敗';
      setTimeout(() => copyBtn.textContent = '複製', 1400);
    }
  });
  head.appendChild(copyBtn);
  box.appendChild(head);
  const pre = el('pre');
  pre.appendChild(el('code', null, code));
  box.appendChild(pre);
  return box;
}

export function renderMarkdown(target, text) {
  clear(target);
  if (!text) return;

  const lines = String(text).split('\n');
  let i = 0;
  let list = null;

  function closeList() { list = null; }

  while (i < lines.length) {
    const line = lines[i];

    const fence = /^\s*```(.*)$/.exec(line);
    if (fence) {
      closeList();
      const lang = fence[1].trim();
      const buf = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) {
        buf.push(lines[i]);
        i++;
      }
      i++; // 吃掉結尾的 ```
      target.appendChild(createCodeBlock(lang, buf.join('\n')));
      continue;
    }

    if (/^\s*$/.test(line)) { closeList(); i++; continue; }

    if (/^\s*([-*_])\s*\1\s*\1[\s\-*_]*$/.test(line)) { closeList(); target.appendChild(el('hr')); i++; continue; }

    const heading = /^(#{1,3})\s+(.*)$/.exec(line);
    if (heading) {
      closeList();
      const h = el('h' + heading[1].length);
      renderInline(h, heading[2]);
      target.appendChild(h);
      i++;
      continue;
    }

    const quote = /^>\s?(.*)$/.exec(line);
    if (quote) {
      closeList();
      const bq = el('blockquote');
      renderInline(bq, quote[1]);
      target.appendChild(bq);
      i++;
      continue;
    }

    const bullet = /^\s*[-*+]\s+(.*)$/.exec(line);
    const numbered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (bullet || numbered) {
      const want = bullet ? 'ul' : 'ol';
      if (!list || list.tagName.toLowerCase() !== want) {
        list = el(want);
        target.appendChild(list);
      }
      const li = el('li');
      renderInline(li, (bullet || numbered)[1]);
      list.appendChild(li);
      i++;
      continue;
    }

    closeList();

    // 連續的普通行併成一段
    const para = [line];
    i++;
    while (i < lines.length &&
      !/^\s*$/.test(lines[i]) &&
      !/^\s*```/.test(lines[i]) &&
      !/^(#{1,3})\s+/.test(lines[i]) &&
      !/^>\s?/.test(lines[i]) &&
      !/^\s*[-*+]\s+/.test(lines[i]) &&
      !/^\s*\d+[.)]\s+/.test(lines[i])) {
      para.push(lines[i]);
      i++;
    }
    const p = el('p');
    renderInline(p, para.join('\n'));
    target.appendChild(p);
  }
}

export function escapeHtml(text) {
  const div = document.createElement('div');
  div.textContent = text;
  return div.innerHTML;
}

export function renderMarkdownToString(text) {
  const div = document.createElement('div');
  renderMarkdown(div, text);
  return div.innerHTML;
}

export function createMarkdownRenderer() {
  const cache = new Map();
  return {
    render(target, text) {
      const key = text;
      if (cache.has(key)) {
        target.innerHTML = cache.get(key);
        return;
      }
      const temp = document.createElement('div');
      renderMarkdown(temp, text);
      const html = temp.innerHTML;
      cache.set(key, html);
      target.innerHTML = html;
    },
    clearCache() { cache.clear(); }
  };
}

export function highlightCode(code, lang) {
  // 簡單的關鍵字高亮，可擴展為 Prism/Highlight.js
  const keywords = {
    js: ['const', 'let', 'var', 'function', 'return', 'if', 'else', 'for', 'while', 'async', 'await', 'import', 'export', 'class', 'extends', 'try', 'catch', 'finally'],
    py: ['def', 'class', 'import', 'from', 'return', 'if', 'elif', 'else', 'for', 'while', 'try', 'except', 'finally', 'with', 'as', 'async', 'await'],
    rs: ['fn', 'let', 'mut', 'struct', 'enum', 'impl', 'trait', 'match', 'if', 'else', 'loop', 'for', 'while', 'return', 'use', 'mod', 'pub', 'async', 'await'],
    go: ['func', 'var', 'const', 'type', 'struct', 'interface', 'return', 'if', 'else', 'for', 'range', 'go', 'defer', 'select', 'case', 'default'],
    sh: ['if', 'then', 'else', 'fi', 'for', 'while', 'do', 'done', 'case', 'esac', 'function', 'return', 'local', 'export', 'source']
  };
  const kw = keywords[lang] || [];
  if (!kw.length) return escapeHtml(code);

  let highlighted = escapeHtml(code);
  kw.forEach(k => {
    const regex = new RegExp(`\\b${k}\\b`, 'g');
    highlighted = highlighted.replace(regex, `<span class="kw">${k}</span>`);
  });
  return highlighted;
}

export default { renderMarkdown, escapeHtml, renderMarkdownToString, createMarkdownRenderer, highlightCode };