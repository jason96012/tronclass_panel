// Offline schedule regression, not executed in this change.
// With permission: node --test src/adapters/line/scanner_first_frame.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
const start = html.indexOf('    function decodeRegions() {');
const end = html.indexOf('\n    /* QR_SHARPEN_V1', start);
assert.ok(start >= 0 && end > start);
function setup() {
  const context = vm.createContext({
    nowMs: () => 1000, video: { videoWidth: 1920, videoHeight: 1080 },
    THOROUGH_EVERY_MS: 800, TILE_ORDER: [0, 1, 2, 3], LADDER: [
      { k: 4, thoroughOnly: false }, { k: 8, thoroughOnly: false },
      { k: 2, thoroughOnly: true }, { k: 1, thoroughOnly: true },
    ],
    tileRect: i => ({ sx: i + 10, sy: 0, cw: 100, ch: 100, label: 'tile' }),
    centerRegion: k => ({ sx: k, sy: 0, cw: 200 / k, ch: 200 / k, label: `centre-${k}` }),
    roiRegion: () => null, digitalZoom: () => 1, hardModeOn: () => true,
  });
  vm.runInContext('let lastThoroughAt = 0, tileCursor = 0, preferTileNext = false, hardPriorityPhase = 0, stickyTile = -1, lastSeenAt = 0;\n' + html.slice(start, end), context);
  return context;
}
test('first hard scan prioritises centre, while later rounds rotate tiles and scales', () => {
  const c = setup();
  const first = [];
  for (let i = 0; i < 6; i++) first.push(c.decodeRegions().regions[0].label);
  assert.deepEqual(first, ['centre-4', 'tile', 'centre-2', 'tile', 'centre-1', 'tile']);
});
