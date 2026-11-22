// Offline pure scheduling regression; this does not open a camera or network.
// Not executed as part of this change: node --test src/adapters/line/scanner_pacing.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
const start = html.indexOf('    function perfYield(ms) {');
const end = html.indexOf('\n    /* 這是「快速路徑」', start);
assert.ok(start >= 0 && end > start, 'shipped perfYield not found');

function harness(withRaf = true) {
  let next = 1;
  const timers = new Map();
  const frames = new Map();
  const context = {
    setTimeout(fn, delay) { const id = next++; timers.set(id, { fn, delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  if (withRaf) {
    context.requestAnimationFrame = fn => { const id = next++; frames.set(id, fn); return id; };
    context.cancelAnimationFrame = id => frames.delete(id);
  }
  vm.createContext(context);
  vm.runInContext(html.slice(start, end), context);
  return {
    yield: context.perfYield, timers, frames,
    frame() { const [id, fn] = frames.entries().next().value; frames.delete(id); fn(); },
    timer(delay) {
      const entry = [...timers.entries()].find(([, value]) => value.delay === delay);
      assert.ok(entry, `timer ${delay} missing`);
      timers.delete(entry[0]); entry[1].fn();
    },
  };
}

test('normal frame resolves after requested delay and cancels fallback timer', async () => {
  const h = harness();
  const p = h.yield(5);
  h.frame();
  assert.equal(h.timers.size, 2);
  h.timer(5);
  await p;
  assert.equal(h.timers.size, 0);
  assert.equal(h.frames.size, 0);
});

test('missing frame resolves through fallback and cancels outstanding frame', async () => {
  const h = harness();
  const p = h.yield(5);
  h.timer(125);
  await p;
  assert.equal(h.frames.size, 0);
  assert.equal(h.timers.size, 0);
});

test('without requestAnimationFrame the normal timer still resolves', async () => {
  const h = harness(false);
  const p = h.yield(0);
  h.timer(0);
  await p;
  assert.equal(h.timers.size, 0);
});
