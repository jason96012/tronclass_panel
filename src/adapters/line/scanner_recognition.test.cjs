// Offline, pure-function regression cases. Not executed in this change.
// Run only with permission: node --test src/adapters/line/scanner_recognition.test.cjs
// Extracts the shipped functions, rather than maintaining independent copies.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
function extract(name) {
  const start = html.indexOf(`    function ${name}(`);
  assert.notEqual(start, -1, `missing ${name}`);
  const brace = html.indexOf('{', start);
  let depth = 0;
  for (let i = brace; i < html.length; i++) {
    if (html[i] === '{') depth++;
    else if (html[i] === '}' && --depth === 0) return html.slice(start, i + 1);
  }
  throw new Error(`unterminated ${name}`);
}
function createContext() {
  const c = vm.createContext({
    video: { videoWidth: 7680, videoHeight: 4320 },
    digitalZoom: () => 1,
    AVG_MAX_SIDE: 1280, AVG_PATCH: 64, AVG_SEARCH: 4,
  });
  vm.runInContext(extract('avgRegion') + '\n' + extract('estimateShift') + '\n' + extract('mapLocToVideo'), c);
  return c;
}
function pattern() {
  const a = new Uint8Array(64 * 64);
  let seed = 123456;
  for (let i = 0; i < a.length; i++) {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    a[i] = seed >>> 24;
  }
  a.srcPerPatch = 1;
  return a;
}
function shift(ref, dx, dy) {
  const out = new Uint8Array(ref.length);
  for (let y = 0; y < 64; y++) for (let x = 0; x < 64; x++) {
    const xx = x + dx, yy = y + dy;
    if (xx >= 0 && xx < 64 && yy >= 0 && yy < 64) out[yy * 64 + xx] = ref[y * 64 + x];
  }
  return out;
}
test('average crop retains source area and caps only output', () => {
  const r = createContext().avgRegion();
  assert.equal(r.cw, 1920);
  assert.equal(r.ch, 1080);
  assert.equal(r.dw, 1280);
  assert.equal(r.dh, 720);
  assert.equal(r.sx, 2880);
  assert.equal(r.sy, 1620);
});
test('SAD detects both displacement directions and search edge', () => {
  const c = createContext(), ref = pattern();
  for (const [dx, dy] of [[0, 0], [2, -3], [-3, 1], [4, 0]]) {
    const found = c.estimateShift(ref, shift(ref, dx, dy));
    assert.equal(found.x, dx);
    assert.equal(found.y, dy);
    assert.equal(found.atSearchEdge, Math.abs(dx) === 4 || Math.abs(dy) === 4);
  }
});
test('flat image ties select zero movement', () => {
  const flat = new Uint8Array(64 * 64).fill(80);
  flat.srcPerPatch = 1;
  const found = createContext().estimateShift(flat, flat);
  assert.equal(found.x, 0);
  assert.equal(found.y, 0);
  assert.equal(found.atSearchEdge, false);
});

test('decoder coordinates map resized crop back to source frame', () => {
  const c = createContext();
  const out = c.mapLocToVideo({pts: [{x: 25, y: 50}], w: 100, h: 200},
    {sx: 1000, sy: 500, cw: 400, ch: 800, dw: 100, dh: 200});
  assert.equal(out[0].x, 1100);
  assert.equal(out[0].y, 700);
});
