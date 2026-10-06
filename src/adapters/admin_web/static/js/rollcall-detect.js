/* ROLLCALL_DETECT_SHARED_V1：「偵測到點名、分組、算完成/未送出人數」的單一正本。
 *
 * 背景：這套判斷邏輯（把 /admin/api/status 回傳的帳號清單，依「同一場點名」分組，
 * 算出每組 total/done）原本被各自抄了一份、散落三個地方：
 *   1. dashboard.html 的 inline <script>（detected()/done()/recent()）
 *   2. rollcall-pending-fab.js（pendingGroups()/accountDone()/recent()，自己承認是
 *      「抄自 dashboard.html」）
 *   3. health.html 的 inline <script>（collectDetected()/doneFor()/recent()）
 * 三份雖然長得很像，但其實已經悄悄分岔：分組用的 key 公式三邊不一樣（沒有 rollcall_id
 * 時，dashboard.html／fab.js 退回用「課程名稱」當 key，health.html 退回用「課程名稱＋
 * 第一次偵測到的時間戳」當 key——前者在同一堂課、短時間內開了兩場不同的點名時會被誤併
 * 成一組，後者不會）。這正是複製貼上程式碼常見的下場：一開始一樣，改著改著就不一樣了，
 * 而且沒有人會發現，因為三邊各自測試各自的頁面。
 *
 * 這支檔案把「分組判斷」收成唯一一份正本（用 health.html 那個比較安全的 key 公式），
 * 三邊都改成呼叫 window.TronclassRollcallDetect.detect(...)，不再各自維護一份。
 * 由 pages.rs 的 inject_theme() 注入，所以跟 rollcall-pending-fab.js 一樣全站 admin
 * 頁都會載入；必須排在呼叫它的頁面／腳本「之前」（inject_theme 已經把這支排在
 * rollcall-pending-fab.js 前面）。
 */
(function () {
  'use strict';
  if (window.TronclassRollcallDetect) return;

  // 「多久以內的 last_detected_timestamp 算還有效」——三邊原本都是寫死 1800（30 分鐘），
  // 這裡維持同樣的預設值，呼叫端仍可用 opts.windowSecs 覆寫（目前三邊都沒覆寫，
  // 保留參數只是不要把「30 分鐘」這個數字也焊死在共用模組裡）。
  function recent(ts, windowSecs) {
    return !!ts && Date.now() / 1000 - Number(ts) < (windowSecs || 1800);
  }

  // 這個帳號是否已經針對「這場點名（rollcallId）」送出過（最近一次成功，或歷史紀錄裡有一筆成功）。
  function accountDone(account, rollcallId) {
    if (!account || !rollcallId) return false;
    if (String(account.last_success_rollcall_id || '') === String(rollcallId)) return true;
    return (account.rollcall_history || []).some(function (h) {
      return String(h.rollcall_id || '') === String(rollcallId) && !!h.success;
    });
  }

  /* 把帳號清單依「同一場點名」分組。
   * 回傳：[{ key, rid, course, url, ts, accounts:[account_id,...], total, done, pending }, ...]
   *   （按 ts 新到舊排序；pending = total - done，呼叫端自己決定要不要只留 pending>0 的組）
   * opts.windowSecs：偵測時間窗（預設 1800 秒／30 分鐘）。
   */
  function detect(accounts, opts) {
    opts = opts || {};
    var windowSecs = opts.windowSecs || 1800;
    var map = {};
    var order = [];
    (accounts || []).forEach(function (a) {
      if (!a || !a.last_detected_timestamp || !recent(a.last_detected_timestamp, windowSecs)) return;
      var course = a.last_detected_course || '點名';
      var rid = a.last_detected_rollcall_id || '';
      // 有 rollcall_id 就用它當 key（最準確）；沒有時退回「課程＋第一次偵測到的時間戳」，
      // 不要只用課程名稱——同一堂課在時間窗內開兩場不同點名時，只用課程名稱會被誤併成一組
      // （這是三份舊程式碼裡 health.html 寫得比較對的地方，這裡統一採用它）。
      var key = (rid ? String(rid) : course + ':' + String(a.last_detected_timestamp)) + ':' + course;
      var g = map[key];
      if (!g) {
        g = {
          key: key,
          rid: rid,
          course: course,
          url: a.last_detected_url || '',
          ts: a.last_detected_timestamp,
          accounts: [],
          total: 0,
          done: 0,
        };
        map[key] = g;
        order.push(key);
      }
      if (!g.url && a.last_detected_url) g.url = a.last_detected_url;
      if (Number(a.last_detected_timestamp) > Number(g.ts)) g.ts = a.last_detected_timestamp;
      g.accounts.push(a.account_id || a.username || '-');
      g.total++;
      if (accountDone(a, rid)) g.done++;
    });
    return order
      .map(function (key) {
        return map[key];
      })
      .map(function (g) {
        g.pending = g.total - g.done;
        return g;
      })
      .sort(function (x, y) {
        return Number(y.ts) - Number(x.ts);
      });
  }

  window.TronclassRollcallDetect = { recent: recent, accountDone: accountDone, detect: detect };
})();
