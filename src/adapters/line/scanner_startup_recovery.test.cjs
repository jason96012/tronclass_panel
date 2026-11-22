// Offline pure scheduling cases; not run in this change.
// With permission: node --test src/adapters/line/scanner_startup_recovery.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, 'scanner.html'), 'utf8');
const start = html.indexOf('    function startupRecoveryReason(');
const end = html.indexOf('\n    async function recoverNoHitCamera', start);
assert.ok(start >= 0 && end > start);
const ctx = vm.createContext({});
vm.runInContext(html.slice(start, end), ctx);
const reason = ctx.startupRecoveryReason;
test('no completed round does not restart until deadline', () => {
  assert.equal(reason(4999, 5000, 12000, 0, true), '');
  assert.equal(reason(5000, 5000, 12000, 0, true), 'no-progress');
});
test('no-hit recovery needs enough completed rounds and its own deadline', () => {
  assert.equal(reason(12000, 5000, 12000, 1, true), 'no-hit');
  assert.equal(reason(11999, 5000, 12000, 3, true), '');
  assert.equal(reason(12000, 5000, 12000, 3, true), 'no-hit');
});
test('a QR hit disables both automatic recovery reasons', () => {
  assert.equal(reason(20000, 5000, 12000, 0, false), '');
  assert.equal(reason(20000, 5000, 12000, 3, false), '');
});
