// Offline decoder-stall regression; no camera, service or network. Not run in this change.
// node --test src/adapters/line/scanner_decoder_recovery.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
function functionSource(startText, endText) {
  const start = html.indexOf(startText), end = html.indexOf(endText, start);
  assert.ok(start >= 0 && end > start);
  return html.slice(start, end);
}
const zx = functionSource('    async function zxingReadImageData(', '\n    /* ─────────── QR_WECHAT_V1');
const wx = functionSource('    async function wechatScanRaw(', '\n    async function wechatReadImageData(');
function setup() {
  let next = 0, callsZ = 0, callsW = 0;
  const timers = new Map();
  let lateZ;
  const pendingZ = new Promise(resolve => { lateZ = resolve; });
  const c = vm.createContext({
    window: { ZXingWASM: { readBarcodes: () => { callsZ++; return pendingZ; } }, jsQR: () => null },
    zxingReady: true, zxingState: 'ready', barcodeDetector: null, nativeDecodeDisabled: false,
    wechatState: 'ready', wechatEngine: { scan: () => { callsW++; return new Promise(() => {}); } },
    setScanHint() {}, zxingPositionToPts: () => null,
    setTimeout(fn, ms) { const id = ++next; timers.set(id, {fn, ms}); return id; },
    clearTimeout(id) { timers.delete(id); },
  });
  vm.runInContext('function hasZxing() { return zxingReady; } function wechatReady() { return wechatState === "ready"; }\n' + zx + '\n' + wx, c);
  return { c, timers, callsZ: () => callsZ, callsW: () => callsW, lateZ,
    fire() { const [id, t] = timers.entries().next().value; assert.equal(t.ms, 4000); timers.delete(id); t.fn(); } };
}
test('ZXing timeout releases waiter, retires engine and ignores late result', async () => {
  const h = setup();
  const pending = h.c.zxingReadImageData({width: 1, height: 1}, {});
  h.fire();
  assert.equal(await pending, null);
  assert.equal(h.c.zxingReady, false);
  h.lateZ([{text: 'synthetic'}]);
  assert.equal(await h.c.zxingReadImageData({width: 1, height: 1}, {}), null);
  assert.equal(h.callsZ(), 1);
  assert.equal(h.timers.size, 0);
});
test('WeChat timeout releases waiter and never schedules another call', async () => {
  const h = setup();
  const pending = h.c.wechatScanRaw({});
  h.fire();
  assert.equal(await pending, null);
  assert.equal(h.c.wechatState, 'unavailable');
  assert.equal(await h.c.wechatScanRaw({}), null);
  assert.equal(h.callsW(), 1);
  assert.equal(h.timers.size, 0);
});
