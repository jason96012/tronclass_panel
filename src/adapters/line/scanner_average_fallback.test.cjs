// Offline regression for the optional average-canvas failure path; not executed here.
// Run: node --test src/adapters/line/scanner_average_fallback.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
const start = html.indexOf('    async function decodeAvg() {');
const end = html.indexOf('\n    /* 顯示「有效記憶框數」', start);
assert.ok(start >= 0 && end > start);

test('broken optional canvas is disabled and future calls do not read it again', async () => {
  let reads = 0;
  let uiUpdates = 0;
  const c = vm.createContext({
    screenAvgOn: true, screenAvgFault: false,
    avgCanvas: { width: 8, height: 8 },
    avgCtx: { getImageData() { reads++; throw new Error('synthetic canvas error'); } },
    avgFrames: 3, AVG_MIN_FRAMES: 2, avgDecodeTicks: 0, AVG_DECODE_EVERY: 1,
    AVG_ALPHA: 0.5, avgSourceMap: {}, avgStartedAt: 1000, AVG_MAX_AGE_MS: 100,
    Date: { now: () => 1001 }, window: {}, hasZxing: () => true,
    applyScreenAvgUi: () => { uiUpdates++; },
  });
  vm.runInContext(html.slice(start, end), c);
  assert.equal(await c.decodeAvg(), null);
  assert.equal(c.screenAvgFault, true);
  assert.equal(c.avgFrames, 0);
  assert.equal(c.avgSourceMap, null);
  assert.equal(await c.decodeAvg(), null);
  assert.equal(reads, 1);
  assert.equal(uiUpdates, 1);
});
