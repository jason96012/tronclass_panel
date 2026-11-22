// Offline mock regression cases; not executed in this change.
// With permission: node --test src/adapters/line/wechat_cleanup.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, 'wechat/index.mjs'), 'utf8');
const start = source.indexOf('async function scan(input, options = {}) {');
const end = source.indexOf('\nasync function loadModels', start);
assert.ok(start >= 0 && end > start);
function fixture(throwAt, corners = [1, 2, 11, 2, 11, 12, 1, 12], noPoints = false) {
  const deleted = [];
  const resource = (name, props = {}) => Object.assign({ delete() { deleted.push(name); } }, props);
  const points = resource('points', { floatAt(i) { return corners[i]; } });
  const vector = resource('vector', { size() { return noPoints ? 0 : 1; }, get() { return points; } });
  const result = resource('result', { get() { if (throwAt === 'result') throw new Error('mock'); return 'qr'; } });
  const image = resource('image');
  const cv = { IMREAD_GRAYSCALE: 0, imread() { return image; }, MatVector: function () { return vector; } };
  const qrcode_detector = { detectAndDecode() { if (throwAt === 'decode') throw new Error('mock'); return result; } };
  const context = vm.createContext({ getOpenCV: async () => ({ cv, qrcode_detector }) });
  vm.runInContext(source.slice(start, end), context);
  return { scan: context.scan, deleted };
}
test('successful scan releases every created OpenCV allocation', async () => {
  const f = fixture();
  const result = await f.scan({});
  assert.equal(result.text, 'qr');
  assert.deepEqual(f.deleted, ['points', 'result', 'vector', 'image']);
});
test('decode failure releases resources allocated before the exception', async () => {
  const f = fixture('decode');
  await assert.rejects(f.scan({}), /mock/);
  assert.deepEqual(f.deleted, ['vector', 'image']);
});
test('result extraction failure releases all resources', async () => {
  const f = fixture('result');
  await assert.rejects(f.scan({}), /mock/);
  assert.deepEqual(f.deleted, ['points', 'result', 'vector', 'image']);
});

test('rotated QR bounds use all four corners', async () => {
  const f = fixture(null, [10, 0, 20, 10, 10, 20, 0, 10]);
  const { rect, text } = await f.scan({});
  assert.equal(text, 'qr');
  assert.equal(rect.x, 0);
  assert.equal(rect.y, 0);
  assert.equal(rect.width, 20);
  assert.equal(rect.height, 20);
});
test('decoded text is retained when points are absent', async () => {
  const f = fixture(null, [], true);
  const { rect, text } = await f.scan({});
  assert.equal(text, 'qr');
  assert.equal(rect, undefined);
  assert.deepEqual(f.deleted, ['result', 'vector', 'image']);
});
