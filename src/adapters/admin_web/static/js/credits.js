/* CREDITS_PAGE_V1 — 學分狀態獨立頁（/admin/credits）
 *
 * 資料來源：
 * 1) GET  /admin/api/credits?account_id=…          → 學分報告快照（report + fetched_at）
 * 2) GET  /admin/api/timetable?account_id=…        → 本學期課表（course_code 對照「修課中」）
 * 3) POST /admin/api/credits/sync/run              → 立即從 emis 同步（手動）
 * 統計（必修/選修/通識、GPA、學期平均）都在前端計算，方便日後調整顯示。
 * 未登入（401/403）時導回登入頁。
 */
(function () {
  'use strict';
  if (window.TronclassCreditsPage) return;
  window.TronclassCreditsPage = true;

  function q(id) { return document.getElementById(id); }
  function esc(v) {
    return String(v == null ? '' : v).replace(/[&<>"']/g, function (ch) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch];
    });
  }
  async function api(path, opts) {
    opts = opts || {};
    opts.credentials = 'same-origin';
    opts.cache = 'no-store';
    var res = await fetch(path, opts);
    var body = await res.json().catch(function () { return {}; });
    if (!res.ok) {
      var e = new Error(body.error || body.message || ('HTTP ' + res.status));
      e.status = res.status;
      throw e;
    }
    return body;
  }
  function fmtTime(epoch) {
    if (!epoch) return '';
    try {
      var d = new Date(epoch * 1000);
      var pad = function (n) { return (n < 10 ? '0' : '') + n; };
      return d.getFullYear() + '/' + pad(d.getMonth() + 1) + '/' + pad(d.getDate()) + ' ' + pad(d.getHours()) + ':' + pad(d.getMinutes());
    } catch (_) { return ''; }
  }
  /* 淡江 GPA 級距（學長姐整理版：A 80~100=4、B 70~79=3、C 65~69=2、D 60~64=1、F<60=0）。
   * 如與學則不同，改這裡即可（不影響百分制平均）。 */
  function tkuGpaPoint(score) {
    if (score >= 80) return 4;
    if (score >= 70) return 3;
    if (score >= 65) return 2;
    if (score >= 60) return 1;
    return 0;
  }

  /* 通識教育參數是「各系不同」的（總學分與核心/代選課承認表都在各系新生手冊）。
   * 這裡維護已建檔的系所；未建檔的系所自動退回通用模式：
   *   - 通識判定只用成績單的學門碼（group_code）
   *   - 通識格不顯示分母（總學分請洽貴系手冊／通核中心）
   * 其餘功能（畢業總覽／必修／本系選修／自由選修／GPA／趨勢／學門清單）全系通用，
   * 因為 emis 三個頁面（TMWS080/120/100）的結構全校一致。
   * key＝系名核心（系級去年級綴字，例：「資工英語二」→「資工英語」）。 */
  var DEPT_GE_PROFILES = {
    '資工英語': {
      // 114 學年度資工全英語學士班：必修 81 ＝ 系必修 55 ＋ 通識教育 26。
      // 通識 26 ＝ 基本知能 12（中國語文 A1376 2＋外語 Q 8＋大學學習 N 1＋課外活動 K 1）
      //   ＋ 通識核心 12（探索永續 1＋三領域各至少 2 學分）
      //   ＋ 服務學習 2（T9607，計入畢業學分；體育 4 與國防不計）。
      geTotal: 26,
      // 全英語學士班：畢業學分只採計「全英語授課」（emis ◎ 前綴）課程。
      allEnglishOnly: true,
      // 無學門碼、計入通識 26 的固定必修／承認課；值＝承認的學門碼（無對應學門為 ''）。
      // 依 114 手冊：社會議題 T3154 → 社會分析(W)；國際學習 T3155 → 全球視野(T)；
      // 團隊發展 T3156 → 課外活動與團隊發展(K)；英文一 T0466 → 外國語文(Q)。
      coreCodes: { A1376: '', T0466: 'Q', T3154: 'W', T3155: 'T', T3156: 'K', T3174: '', T9607: '' }
    }
  };
  /* 淡江 114 學年度起通識架構（全校統一）：學門 → 領域。
   * 人文／社會／科學三大領域各至少 2 學分（GE_AREA_MIN），每學門至多 2 科 4 學分；
   * Q／N／K 屬基本知能（固定必修或學校代選，不適用領域最低門檻）。 */
  var GE_DOMAIN_AREAS = {
    L: '人文', P: '人文', V: '人文', M: '人文',
    T: '社會', R: '社會', W: '社會', S: '社會',
    O: '科學', Z: '科學', U: '科學',
    Q: '基本知能', N: '基本知能', K: '基本知能'
  };
  var GE_AREA_MIN = { '人文': 2, '社會': 2, '科學': 2 };
  /* 畢業審核學年度 ≥114 才用「領域」架構顯示（113 前為舊制，學門平面顯示）。 */
  function geUseAreas(report) {
    var y = parseInt((report && report.graduation && report.graduation.audit_year) || '', 10);
    return y >= 114;
  }
  /* 校定必修但不列入畢業總學分（128）的課：體育（T98xx/T99xx 系列）與
   * 全民國防教育（T97xx 系列）。只計入當學期選課學分數，用於滿足學期學分下限。
   * 淡江全校規則（114 學年度架構：體育 4、國防 1 學分）。 */
  var NON_DEGREE_PREFIXES = ['T98', 'T99', 'T97'];
  function isNonDegreeCourse(c) {
    var code = (c.course_code || '').toUpperCase();
    return NON_DEGREE_PREFIXES.some(function (p) { return code.indexOf(p) === 0; });
  }
  /* 必修列學期排序：全年（上下）→ 上 → 下 → 其他；必修清單與缺口卡共用。 */
  var TERM_W = { '上下': 0, '上': 1, '下': 2 };
  function termSort(a, b) {
    var wa = TERM_W[a.term]; if (wa === undefined) wa = 3;
    var wb = TERM_W[b.term]; if (wb === undefined) wb = 3;
    return wa - wb;
  }
  var currentGeProfile = null;
  function setGeProfile(report) {
    var dept = ((report && report.student && report.student.dept) || '').trim();
    var core = dept.length >= 2 ? dept.slice(0, -1) : dept; // 去掉末位年級綴字
    currentGeProfile = null;
    for (var key in DEPT_GE_PROFILES) {
      if (core === key || core.indexOf(key) === 0) {
        currentGeProfile = DEPT_GE_PROFILES[key];
        break;
      }
    }
  }
  function isGeCourse(c) {
    if (c.group_code) return true;
    var map = currentGeProfile && currentGeProfile.coreCodes;
    return !!map && Object.prototype.hasOwnProperty.call(map, c.course_code);
  }
  /* 這門課承認的通識學門碼（無則空字串；用於學門「已修」判定與標籤）。
   * （資工英語的無碼核心/代選課對照在 DEPT_GE_PROFILES.coreCodes：T0466→Q、
   * T3174、T3154→S、T9607、T3155→T；有 group 碼的課不需要對照。） */
  function coreGeGroup(c) {
    if (c.group_code) return c.group_code;
    var map = currentGeProfile && currentGeProfile.coreCodes;
    return (map && map[c.course_code]) || '';
  }
  /* 本系開課判定：成績單「開課單位簡稱」與學生系級去掉年級字後比對
   * （例：資工英語一 vs 資工英語二 → 同為「資工英語」）。 */
  function isDeptCourse(c, dept) {
    if (!c.dept_name || !dept || dept.length < 2) return false;
    return c.dept_name.slice(0, -1) === dept.slice(0, -1);
  }

  var statusCache = null;
  async function loadStatus() {
    if (statusCache) return statusCache;
    statusCache = await api('/admin/api/status?include_history=false&history_limit=0');
    return statusCache;
  }
  async function fillAccountSelect(sel) {
    try {
      var st = await loadStatus();
      var old = sel.value;
      sel.innerHTML = '';
      sel.disabled = false;
      if (st.is_admin) {
        (st.accounts || []).forEach(function (a) {
          var id = a.account_id || a.id || a.username || '';
          if (!id) return;
          var name = a.user_name || a.name || a.display_name || a.username || '';
          sel.appendChild(new Option((id + (name ? '  ' + name : '')).trim(), id));
        });
        var preferred = (st.current_account_id && st.current_account_id !== '<token-admin>') ? st.current_account_id : '';
        if (old) sel.value = old;
        if (preferred && Array.prototype.some.call(sel.options, function (o) { return o.value === preferred; })) sel.value = preferred;
        if (!sel.value && sel.options.length) sel.selectedIndex = 0;
      } else {
        var me = st.current_account_id || st.account_id || '目前帳號';
        sel.appendChild(new Option(me, me));
        sel.disabled = true;
      }
    } catch (e) {
      sel.innerHTML = '';
      sel.appendChild(new Option('載入失敗', ''));
      sel.disabled = true;
    }
  }

  // ── 統計計算 ────────────────────────────────────────────────────────────
  function summarize(report, timetableEntries) {
    var g = report.graduation || {};
    var courses = report.courses || [];
    var dept = (report.student || {}).dept || '';
    var passed = courses.filter(function (c) { return c.passed; });
    var sumCredits = function (list) {
      return list.reduce(function (acc, c) { return acc + (c.credits || 0); }, 0);
    };
    // 畢業學分口徑：排除體育／全民國防（校定必修但不含在 128 門檻內）。
    var degreePassed = passed.filter(function (c) { return !isNonDegreeCourse(c); });
    var peCredits = sumCredits(passed.filter(function (c) { return /^(T98|T99)/.test((c.course_code || '').toUpperCase()); }));
    var defenseCredits = sumCredits(passed.filter(function (c) { return /^T97/.test((c.course_code || '').toUpperCase()); }));
    var requiredPassed = degreePassed.filter(function (c) { return c.required; });
    // 通識：學門碼（group_code）或核心/代選課常數；通識是必修 81 的一部分。
    var gePassedList = degreePassed.filter(isGeCourse);
    // 選修課細分：本系開課 → 本系選修；其他（外系、共同科目）→ 自由選修。
    var electivePassed = degreePassed.filter(function (c) { return !c.required && !isGeCourse(c); });
    var deptElectiveList = electivePassed.filter(function (c) { return isDeptCourse(c, dept); });
    var freeElectiveList = electivePassed.filter(function (c) { return !isDeptCourse(c, dept); });

    var total = g.total_credits || 0;
    var requiredNeed = g.required_credits || 0;
    var electiveNeed = g.dept_elective_min || 0;
    var otherNeed = Math.max(0, total - requiredNeed - electiveNeed); // 自由選修（其他選修）

    // 本學期修課中的必修（用課表 course_code 對照必修清單）。
    var doingCodes = {};
    (timetableEntries || []).forEach(function (e) {
      var code = (e.course_code || '').trim().toUpperCase();
      if (code) doingCodes[code] = true;
    });
    var requiredDoing = (report.required_courses || []).filter(function (r) {
      return doingCodes[(r.course_code || '').trim().toUpperCase()];
    });
    var requiredDoneCodes = {};
    passed.forEach(function (c) { if (c.course_code) requiredDoneCodes[c.course_code.toUpperCase()] = true; });
    // 修課中必修（畢業口徑）：排除已及格與體育/國防（後者不列入 128）。
    var requiredDoingList = requiredDoing.filter(function (r) {
      return !requiredDoneCodes[(r.course_code || '').trim().toUpperCase()] && !isNonDegreeCourse(r);
    });
    var requiredDoingCredits = sumCredits(requiredDoingList);
    // 已完成學期數（有及格課的學年-學期去重）：達標速度估算用。
    var semSeen = {};
    passed.forEach(function (c) { if (c.yr && c.sem) semSeen[c.yr + '-' + c.sem] = true; });
    var semCount = Object.keys(semSeen).length;

    // 已修通識學門（有學門碼的及格課＋手冊承認學門的核心/代選課）。
    var geDoneGroups = [];
    // 各領域（人文/社會/科學）通識及格學分（114 制架構；每領域至少 2 學分）。
    var geAreas = { '人文': 0, '社會': 0, '科學': 0 };
    passed.forEach(function (c) {
      if (!isGeCourse(c)) return;
      var grp = coreGeGroup(c);
      if (grp && geDoneGroups.indexOf(grp) < 0) geDoneGroups.push(grp);
      var area = grp && GE_DOMAIN_AREAS[grp];
      if (area && GE_AREA_MIN[area]) geAreas[area] = (geAreas[area] || 0) + (c.credits || 0);
    });

    return {
      earnedReported: report.earned_total,
      earnedSum: sumCredits(passed),
      degreeEarned: sumCredits(degreePassed),
      peCredits: peCredits,
      defenseCredits: defenseCredits,
      requiredPassed: sumCredits(requiredPassed),
      requiredNeed: requiredNeed,
      requiredDoingCredits: requiredDoingCredits,
      requiredDoingList: requiredDoingList,
      requiredDoneCodes: requiredDoneCodes,
      semCount: semCount,
      gePassed: sumCredits(gePassedList),
      geNeed: currentGeProfile ? currentGeProfile.geTotal : null,
      geDoneGroups: geDoneGroups,
      geAreas: geAreas,
      deptElectivePassed: sumCredits(deptElectiveList),
      freeElectivePassed: sumCredits(freeElectiveList),
      electiveNeed: electiveNeed,
      otherNeed: otherNeed,
      total: total,
      passedCount: passed.length,
      failedCount: courses.length - passed.length,
      doingCodes: doingCodes,
      isGeCourse: isGeCourse,
      isDeptCourse: function (c) { return isDeptCourse(c, dept); }
    };
  }

  function semesters(report) {
    var map = {};
    (report.courses || []).forEach(function (c) {
      var key = (c.yr || '') + '-' + (c.sem || '');
      if (!map[key]) map[key] = { key: key, yr: c.yr, sem: c.sem, rows: [] };
      map[key].rows.push(c);
    });
    return Object.keys(map).sort().map(function (k) { return map[k]; });
  }

  function semStats(rows) {
    var sumScore = 0, sumCredits = 0, sumPts = 0, gpaCredits = 0, passed = 0, failed = 0;
    rows.forEach(function (c) {
      if (c.passed) passed++; else failed++;
      if (typeof c.grade_value === 'number') {
        sumScore += c.grade_value * (c.credits || 0);
        sumCredits += c.credits || 0;
        sumPts += tkuGpaPoint(c.grade_value) * (c.credits || 0);
        gpaCredits += c.credits || 0;
      }
    });
    return {
      avg: sumCredits ? (sumScore / sumCredits) : null,
      gpa: gpaCredits ? (sumPts / gpaCredits) : null,
      avgCredits: sumCredits,
      passed: passed,
      failed: failed
    };
  }

  /* 「還缺什麼」的數字計算（純邏輯、無 DOM）。 */
  function computeGap(report, s) {
    var degGot = s.degreeEarned;
    var reqGot = s.requiredPassed;
    var geGot = s.gePassed;
    var reqGap = s.requiredNeed ? Math.max(0, s.requiredNeed - reqGot) : null;
    var elGap = s.electiveNeed ? Math.max(0, s.electiveNeed - s.deptElectivePassed) : null;
    var oGap = s.otherNeed ? Math.max(0, s.otherNeed - s.freeElectivePassed) : null;
    var geGap = s.geNeed != null ? Math.max(0, s.geNeed - geGot) : null;
    var totalGap = s.total ? Math.max(0, s.total - degGot) : null;
    var areas = s.geAreas;
    var areaGaps = null;
    if (s.geNeed != null && geUseAreas(report)) {
      areaGaps = {};
      ['人文', '社會', '科學'].forEach(function (a) {
        areaGaps[a] = Math.max(0, (GE_AREA_MIN[a] || 0) - (areas[a] || 0));
      });
    }
    // 未完成必修（畢業口徑：排除體育/國防與已及格；預估視角再排除修課中）。
    var pend = (report.required_courses || []).filter(function (r) {
      var code = (r.course_code || '').trim().toUpperCase();
      if (!code || isNonDegreeCourse(r)) return false;
      if (s.requiredDoneCodes[code]) return false;
      return true;
    });
    var groups = {}, order = [];
    pend.forEach(function (r) {
      var k = r.grade_level || '其他';
      if (!groups[k]) { groups[k] = []; order.push(k); }
      groups[k].push(r);
    });
    order.forEach(function (k) { groups[k].sort(termSort); });
    var pendCredits = pend.reduce(function (acc, r) { return acc + (r.credits || 0); }, 0);
    // 達標速度：已得學分 ÷ 已完成學期數。
    var pace = s.semCount > 0 ? degGot / s.semCount : 0;
    return {
      degGot: degGot, reqGot: reqGot, geGot: geGot,
      reqGap: reqGap, elGap: elGap, oGap: oGap, geGap: geGap, totalGap: totalGap,
      areaGaps: areaGaps,
      groups: groups, order: order, pendCount: pend.length, pendCredits: pendCredits,
      doingCount: s.requiredDoingList.length, doingCredits: s.requiredDoingCredits,
      semCount: s.semCount, pace: pace,
      needSems: totalGap && pace > 0 ? Math.ceil(totalGap / pace) : 0
    };
  }

  // ── 渲染 ────────────────────────────────────────────────────────────────
  var currentReport = null;
  var currentEntries = null;

  function render(report, fetchedAt, timetableEntries) {
    currentReport = report;
    currentEntries = timetableEntries || [];
    setGeProfile(report); // 依系級載入通識參數（未建檔系所 → 通用模式）
    var s = summarize(report, currentEntries);
    var g = report.graduation || {};

    q('content').classList.remove('hidden-init');
    q('emptyCard').classList.add('hidden-init');

    // 總覽
    var st = report.student || {};
    q('studentLine').textContent = (st.dept ? st.dept + '　' : '') + (st.name ? st.name + '　' : '') + (st.student_no ? '學號 ' + st.student_no : '');
    q('overviewDesc').textContent = g.audit_year
      ? '畢業審核條件比照 ' + g.audit_year + ' 學年度入學新生' + (g.min_semesters ? '；修業學期 ' + (g.completed_semesters || 0) + '/' + g.min_semesters : '')
      : '';
    q('fetchedBadge').textContent = fetchedAt ? ('同步於 ' + fmtTime(fetchedAt)) : '—';

    q('earnedBig').innerHTML = esc(s.degreeEarned) + '<small> / ' + esc(s.total || '—') + ' 學分</small>';
    var note = '畢業學分＝及格學分 ' + s.degreeEarned + '（不含體育 ' + s.peCredits + '、全民國防 ' + s.defenseCredits + ' 學分，二者不列入畢業門檻）';
    note += s.earnedReported != null && s.earnedReported !== s.earnedSum
      ? '；校方累計實得 ' + s.earnedReported + ' 學分與全部及格加總 ' + s.earnedSum + ' 不同，請以校方為準'
      : '；校方累計實得 ' + s.earnedSum + ' 學分＝全部及格加總（含不計門檻課程）';
    q('earnedNote').textContent = note;
    q('earnedBar').style.width = s.total ? Math.min(100, Math.round(s.degreeEarned * 100 / s.total)) + '%' : '0%';

    var statGrid = q('statGrid');
    statGrid.innerHTML = '';
    function stat(cls, k, v, sub) {
      var d = document.createElement('div');
      d.className = 'stat' + (cls ? ' ' + cls : '');
      d.innerHTML = '<div class="k">' + esc(k) + '</div><div class="v">' + v + '</div>' + (sub ? '<div class="sub">' + esc(sub) + '</div>' : '');
      statGrid.appendChild(d);
    }
    function frac(a, b) { return esc(a) + '<small> / ' + esc(b || '—') + '</small>'; }
    stat('', '必修學分', frac(s.requiredPassed, s.requiredNeed),
      (s.geNeed ? '含通識教育 ' + s.geNeed + ' 學分' : '含通識教育學分（依貴系規定）') +
      (s.requiredDoingCredits ? '；修課中再+ ' + s.requiredDoingCredits + ' 學分' : ''));
    var geValue = s.geNeed ? frac(s.gePassed, s.geNeed) : esc(s.gePassed) + '<small> 學分</small>';
    var geSub = s.geNeed
      ? '基本知能＋通識核心；含於必修 ' + (s.requiredNeed || '—') + ' 內'
      : '以學門碼統計；總學分依貴系手冊';
    if (geUseAreas(report)) {
      geSub += '；領域 ' + ['人文', '社會', '科學'].map(function (a) {
        var got = s.geAreas[a] || 0;
        return a + (got >= GE_AREA_MIN[a] ? '✓' : '✗');
      }).join(' ') + '（各至少 2 學分）';
    }
    if (s.geDoneGroups.length) geSub += '；已修學門 ' + s.geDoneGroups.join(' ');
    stat(s.geNeed && s.gePassed >= s.geNeed ? 'ok' : '', '通識教育', geValue, geSub);
    stat(s.deptElectivePassed >= s.electiveNeed && s.electiveNeed ? 'ok' : '', '本系選修', frac(s.deptElectivePassed, s.electiveNeed),
      '本系（' + ((report.student || {}).dept || '').slice(0, -1) + '）開設的選修課');
    stat('', '自由選修', frac(s.freeElectivePassed, s.otherNeed),
      '其他選修（外系／共同科目等）');
    stat('', '修業學期', frac(g.completed_semesters || 0, g.min_semesters || 0), g.max_semesters ? '上限 ' + g.max_semesters + ' 學期' : '');

    renderGap(report, s);

    renderGe(report, s);
    renderSemesters(report);
    renderRequired(report, s);
    renderCourses(report, s);
  }

  function renderGe(report, s) {
    var domains = report.ge_domains || [];
    var passedGe = {};
    (report.courses || []).forEach(function (c) {
      if (c.passed && isGeCourse(c)) {
        var grp = coreGeGroup(c);
        if (grp) {
          if (!passedGe[grp]) passedGe[grp] = [];
          passedGe[grp].push(c);
        }
      }
    });
    var openChips = q('geOpenChips');
    var grid = q('geGrid');
    openChips.innerHTML = '';
    grid.innerHTML = '';
    var openCount = 0;

    function domainItem(d) {
      var done = passedGe[d.code];
      var item = document.createElement('div');
      item.className = 'ge-item' + (done ? ' done' : '');
      if (done) {
        var enLine = done.map(function (c) {
          var line = esc(c.name) + '（' + esc(c.grade) + '，' + c.credits + ' 學分）';
          if (c.is_english === true) {
            line += ' <span class="badge ok">全英</span>';
          } else if (c.is_english === false && currentGeProfile && currentGeProfile.allEnglishOnly) {
            // 全英語學士班：中文授課之通識核心科目不列入畢業學分（114 手冊）。
            line += ' <span class="badge warn">中文授課⚠不計畢業學分</span>';
          }
          return line;
        }).join('<br>');
        item.innerHTML = '<div class="t"><span class="code">' + esc(d.code) + '</span>' + esc(d.name) +
          '<span class="badge ok">已修</span></div><div class="d">' + enLine + '</div>';
      } else {
        openCount++;
        item.innerHTML = '<div class="t"><span class="code">' + esc(d.code) + '</span>' + esc(d.name) +
          '<span class="badge todo">尚未修</span></div><div class="d">還可以選的學門' + (d.grade_level ? '（排定 ' + esc(d.grade_level) + '年級）' : '') + '</div>';
        var chip = document.createElement('span');
        chip.className = 'chip';
        chip.innerHTML = '<span class="code">' + esc(d.code) + '</span>' + esc(d.name);
        openChips.appendChild(chip);
      }
      return item;
    }

    if (geUseAreas(report)) {
      // 114 學年度起：學門 → 領域分組顯示（人文/社會/科學各至少 2 學分）。
      var AREA_ORDER = ['人文', '社會', '科學', '基本知能', '其他'];
      var buckets = {};
      AREA_ORDER.forEach(function (a) { buckets[a] = []; });
      domains.forEach(function (d) {
        var area = GE_DOMAIN_AREAS[d.code] || '其他';
        (buckets[area] || buckets['其他']).push(d);
      });
      AREA_ORDER.forEach(function (area) {
        if (!buckets[area].length) return;
        var got = (s.geAreas && s.geAreas[area]) || 0;
        var min = GE_AREA_MIN[area];
        var head;
        if (min) {
          head = area + '領域（至少 ' + min + ' 學分）— 已修 ' + got + ' 學分 ' + (got >= min ? '✓' : '⬜');
        } else if (area === '基本知能') {
          head = '基本知能（語文表達／學習與發展／課外活動；多為固定必修或學校代選）';
        } else {
          head = area;
        }
        var wrap = document.createElement('div');
        wrap.className = 'req-group';
        var h = document.createElement('h3');
        h.textContent = head;
        if (min && got >= min) h.className = 'area-ok';
        wrap.appendChild(h);
        var g = document.createElement('div');
        g.className = 'ge-grid';
        buckets[area].forEach(function (d) { g.appendChild(domainItem(d)); });
        wrap.appendChild(g);
        grid.appendChild(wrap);
      });
    } else {
      // 舊學年度（113 前）：學門平面顯示。
      domains.forEach(function (d) { grid.appendChild(domainItem(d)); });
    }

    if (!domains.length) {
      grid.innerHTML = '<div class="small">必修科目表未列出通識學門（可能資料未同步）。</div>';
    }
    if (!openCount && domains.length) {
      openChips.innerHTML = '<span class="badge ok">所有列出學門都已修得及格課程 🎉</span>';
    }
  }

  function renderSemesters(report) {
    var sems = semesters(report);
    var semGrid = q('semGrid');
    semGrid.innerHTML = '';
    var svg = q('trendSvg');
    var points = [];
    sems.forEach(function (sem) {
      var t = semStats(sem.rows);
      var card = document.createElement('div');
      card.className = 'sem-card';
      var label = (sem.yr || '') + ' 學年度 第 ' + (sem.sem || '?') + ' 學期';
      card.innerHTML = '<div class="t">' + esc(label) + '</div>' +
        '<div class="n">' + (t.avg == null ? '—' : t.avg.toFixed(2)) + '<small> 加權平均</small></div>' +
        '<div class="d">GPA ' + (t.gpa == null ? '—' : t.gpa.toFixed(2)) + '　及格 ' + t.passed + ' 科' + (t.failed ? '　<span class="bad-text">不及格 ' + t.failed + ' 科</span>' : '') + '</div>';
      semGrid.appendChild(card);
      if (t.avg != null) points.push({ label: sem.yr + '-' + sem.sem, avg: t.avg });
    });
    // 趨勢折線（有 2 點以上才畫；用 createElementNS 純 DOM API，
    // 不經 innerHTML——避開 Trusted Types / DOMPurify 的 SVG namespace 邊角）。
    if (points.length >= 2) {
      svg.classList.remove('hidden-init');
      var W = 640, H = 110, PAD = 26, BOTTOM = 18;
      var SVGNS = 'http://www.w3.org/2000/svg';
      var lo = Math.min.apply(null, points.map(function (p) { return p.avg; }));
      var hi = Math.max.apply(null, points.map(function (p) { return p.avg; }));
      if (hi - lo < 8) { var mid = (hi + lo) / 2; lo = mid - 4; hi = mid + 4; }
      lo = Math.floor(lo); hi = Math.ceil(hi);
      var x = function (i) { return PAD + (W - 2 * PAD) * (points.length === 1 ? 0.5 : i / (points.length - 1)); };
      var y = function (v) { return BOTTOM + (H - 2 * BOTTOM) * (1 - (v - lo) / (hi - lo)); };
      function el(tag, attrs) {
        var e = document.createElementNS(SVGNS, tag);
        for (var k in attrs) e.setAttribute(k, String(attrs[k]));
        return e;
      }
      while (svg.firstChild) svg.removeChild(svg.firstChild);
      for (var gv = lo; gv <= hi; gv += Math.max(1, Math.round((hi - lo) / 3))) {
        svg.appendChild(el('line', { class: 'grid', x1: PAD, x2: W - PAD, y1: y(gv), y2: y(gv) }));
        var gt = el('text', { x: PAD - 6, y: y(gv) + 3, 'text-anchor': 'end' });
        gt.textContent = String(gv);
        svg.appendChild(gt);
      }
      var poly = el('polyline', {
        class: 'ln',
        points: points.map(function (p, i) { return x(i) + ',' + y(p.avg); }).join(' ')
      });
      svg.appendChild(poly);
      points.forEach(function (p, i) {
        svg.appendChild(el('circle', { class: 'pt', cx: x(i), cy: y(p.avg), r: 4 }));
        var vt = el('text', { x: x(i), y: y(p.avg) - 9, 'text-anchor': 'middle' });
        vt.textContent = p.avg.toFixed(1);
        svg.appendChild(vt);
        var lt = el('text', { x: x(i), y: H - 4, 'text-anchor': 'middle' });
        lt.textContent = p.label;
        svg.appendChild(lt);
      });
    } else {
      svg.classList.add('hidden-init');
    }
  }

  function renderRequired(report, s) {
    var list = q('reqList');
    list.innerHTML = '';
    var groups = {};
    var order = [];
    (report.required_courses || []).forEach(function (r) {
      var k = r.grade_level || '其他';
      if (!groups[k]) { groups[k] = []; order.push(k); }
      groups[k].push(r);
    });
    // 學門（無科號）不改為逐科教學，只在說明提示。
    // 同年級內依開課學期排序（termSort），避免 emis 原始列序上下交替。
    order.forEach(function (k) { groups[k].sort(termSort); });
    var html = '';
    order.forEach(function (k) {
      html += '<div class="req-group"><h3>' + esc(k) + '年級</h3><div class="req-rows">';
      groups[k].forEach(function (r) {
        var done = false, doing = false, gradeTxt = '';
        for (var i = 0; i < (report.courses || []).length; i++) {
          var c = report.courses[i];
          if ((c.course_code || '').toUpperCase() === (r.course_code || '').toUpperCase()) {
            if (c.passed) { done = true; gradeTxt = c.grade; }
          }
        }
        if (!done && s.doingCodes[(r.course_code || '').toUpperCase()]) doing = true;
        var mark = done ? '✅' : (doing ? '📖' : '⬜');
        var cls = done ? ' done' : (doing ? ' doing' : '');
        var meta = (r.term || '') + (r.credits ? '　' + r.credits + ' 學分' : '') + (gradeTxt ? '　成績 ' + esc(gradeTxt) : '');
        html += '<div class="req-row' + cls + '"><span class="mark">' + mark + '</span>' +
          '<span class="name">' + esc(r.name) + (r.course_code ? ' <span class="meta">' + esc(r.course_code) + '</span>' : '') + '</span>' +
          '<span class="meta">' + meta + '</span></div>';
      });
      html += '</div></div>';
    });
    if (!order.length) {
      html = '<div class="small">必修科目清單未同步。</div>';
    }
    list.innerHTML = html;
  }

  /* 「還缺什麼」卡：computeGap 算數字（純邏輯，可離線驗證），這裡只組 HTML。 */
  function renderGap(report, s) {
    if (!s.total) {
      q('gapBody').innerHTML = '<div class="small">畢業門檻資料未同步，無法計算缺口。</div>';
      q('gapDesc').textContent = '各項畢業缺口與達標預估。';
      return;
    }
    var c = computeGap(report, s);
    function right(gap) {
      return gap == null ? '—' : (gap === 0 ? '✓ 完成' : '缺 ' + gap + ' 學分');
    }
    function item(ok, name, sub, need) {
      return '<div class="gap-item' + (ok ? ' ok' : '') + '"><div class="g-name">' + esc(name) +
        (sub ? '<span class="small">' + esc(sub) + '</span>' : '') + '</div>' +
        '<div class="g-need">' + esc(need) + '</div></div>';
    }
    var h = '';
    var reqSub = '已修 ' + c.reqGot + ' / ' + (s.requiredNeed || '—');
    if (c.doingCount) reqSub += '；修課中 ' + c.doingCount + ' 門 ' + c.doingCredits + ' 學分';
    if (c.pendCount) reqSub += '；未修 ' + c.pendCount + ' 門 ' + c.pendCredits + ' 學分';
    h += item(c.reqGap === 0, '必修學分', reqSub, right(c.reqGap));
    h += item(c.elGap === 0, '本系選修', '已修 ' + s.deptElectivePassed + ' / ' + (s.electiveNeed || '—') + '；修課中選修之學分待成績公布後計入', right(c.elGap));
    h += item(c.oGap === 0, '自由選修', '已修 ' + s.freeElectivePassed + ' / ' + (s.otherNeed || '—'), right(c.oGap));
    if (s.geNeed != null) {
      var geSub = '已修 ' + c.geGot + ' / ' + s.geNeed + '；含於必修';
      if (c.areaGaps) {
        geSub += '；' + ['人文', '社會', '科學'].map(function (a) {
          return a + (c.areaGaps[a] > 0 ? '缺 ' + c.areaGaps[a] : '✓');
        }).join('　') + '（各至少 2 學分）';
      }
      h += item(c.geGap === 0, '通識教育', geSub, right(c.geGap));
    }
    if (c.pendCount) {
      h += '<details class="gap-more"><summary>未完成必修清單（' + c.pendCount + ' 門 ' + c.pendCredits + ' 學分）</summary>';
      c.order.forEach(function (k) {
        h += '<div class="small mb6">' + esc(k) + '年級</div><div class="req-rows">';
        c.groups[k].forEach(function (r) {
          h += '<div class="req-row"><span class="mark">⬜</span><span class="name">' + esc(r.name) +
            ' <span class="meta">' + esc(r.course_code) + '</span></span><span class="meta">' +
            esc(r.term || '') + (r.credits ? '　' + r.credits + ' 學分' : '') + '</span></div>';
        });
        h += '</div>';
      });
      h += '</details>';
    }
    h += '<div class="gap-total">總缺口 <b>' + c.totalGap + '</b> 學分（畢業 ' + s.total + ' − 已得 ' + c.degGot + '）';
    if (c.needSems) h += '；目前速度 ' + (Math.round(c.pace * 10) / 10) + ' 學分/學期（' + c.semCount + ' 學期），約需 <b>' + c.needSems + '</b> 個學期';
    h += '。</div>';
    q('gapBody').innerHTML = h;
    q('gapDesc').textContent = '以畢業學分 ' + s.total + ' 口徑列出各項缺口與達標預估。';
  }

  function renderCourses(report, s) {
    var courses = report.courses || [];
    var tbody = q('courseTbody');
    tbody.innerHTML = '';
    courses.forEach(function (c) {
      var tr = document.createElement('tr');
      if (!c.passed) tr.className = 'failed';
      var kind;
      var enBadge = c.is_english ? '<span class="pill ge">全英</span>' : '';
      if (isNonDegreeCourse(c)) {
        kind = '<span class="pill">體育／國防（不計門檻）</span>';
      } else if (s.isGeCourse(c)) {
        kind = '<span class="pill ge">通識' + esc(coreGeGroup(c) || '核心') + '</span>' + enBadge;
      } else if (c.required) {
        kind = '<span class="pill req">必修</span>' + enBadge;
      } else if (s.isDeptCourse(c)) {
        kind = '<span class="pill">本系選修</span>' + enBadge;
      } else {
        kind = '<span class="pill">自由選修</span>' + enBadge;
      }
      tr.innerHTML = '<td class="credit-num">' + esc(c.yr) + '</td><td class="credit-num">' + esc(c.sem) + '</td>' +
        '<td class="wrap">' + esc(c.name) + (c.course_code ? ' <span class="code-inline">' + esc(c.course_code) + '</span>' : '') + '</td>' +
        '<td>' + kind + '</td><td class="credit-num">' + c.credits + '</td><td class="credit-num">' + esc(c.grade) + '</td>';
      tbody.appendChild(tr);
    });
    q('coursesDesc').textContent = '共 ' + courses.length + ' 筆（含不及格重修紀錄；同科上/下學期各列一筆）。類別：必修／本系選修／自由選修／通識（英文字母＝學門碼，「通識核心」＝計入通識 26 學分的無學門碼核心課）。';
    if (!courses.length) tbody.innerHTML = '<tr><td colspan="6" class="small">尚無歷年成績資料。</td></tr>';
  }

  function showEmpty(msg) {
    q('content').classList.add('hidden-init');
    q('emptyCard').classList.remove('hidden-init');
    if (msg) q('emptyText').textContent = msg;
  }

  // ── 資料載入 ────────────────────────────────────────────────────────────
  async function loadReport(accountId, force) {
    try {
      var data = await api('/admin/api/credits' + (accountId ? '?account_id=' + encodeURIComponent(accountId) : ''));
      if (!data.report) {
        showEmpty();
        return;
      }
      // 本學期課表（course_code 對照修課中；失敗不影響主顯示）。
      var entries = [];
      try {
        var tt = await api('/admin/api/timetable' + (accountId ? '?account_id=' + encodeURIComponent(accountId) : ''));
        entries = tt.entries || [];
      } catch (_) { /* 課表 optional */ }
      render(data.report, data.fetched_at, entries);
    } catch (e) {
      if (e.status === 401 || e.status === 403) {
        location.replace('/admin?next=' + encodeURIComponent('/admin/credits'));
        return;
      }
      showEmpty('讀取學分資料失敗：' + (e.message || e));
    }
  }

  async function runSync() {
    var btn = q('syncBtn');
    var accountId = q('accountSel').value;
    btn.disabled = true;
    btn.textContent = '同步中…';
    q('syncMsg').className = 'msg';
    q('syncMsg').textContent = '正在連線校務系統（emis）抓取修業狀況、必修清單與歷年成績，約需數秒～數十秒…';
    try {
      var res = await api('/admin/api/credits/sync/run', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(accountId ? { account_id: accountId } : {})
      });
      q('syncMsg').className = 'msg ok';
      q('syncMsg').textContent = '同步完成（' + (res.courses || 0) + ' 科' +
        (res.earned_total != null ? '，累計 ' + res.earned_total + ' 學分' : '') + '）';
      statusCache = null; /* 帳號狀態可能變動 */
      await loadReport(accountId, true);
    } catch (e) {
      if (e.status === 401 || e.status === 403) {
        location.replace('/admin?next=' + encodeURIComponent('/admin/credits'));
        return;
      }
      q('syncMsg').className = 'msg bad';
      q('syncMsg').textContent = e.message || ('同步失敗（HTTP ' + e.status + '）');
      /* 快照可能仍存在：顯示舊資料。 */
      loadReport(accountId);
    } finally {
      btn.disabled = false;
      btn.textContent = '立即同步';
    }
  }

  // ── 啟動 ────────────────────────────────────────────────────────────────
  function verifySession() {
    if (document.documentElement.getAttribute('data-auth') === 'ok') return;
    fetch('/admin/api/status', { credentials: 'same-origin', cache: 'no-store' })
      .then(function (res) {
        if (res.status === 401 || res.status === 403) {
          location.replace('/admin?next=' + encodeURIComponent('/admin/credits'));
        }
      }).catch(function () { });
  }

  async function init() {
    verifySession();
    var sel = q('accountSel');
    await fillAccountSelect(sel);
    sel.onchange = function () { loadReport(sel.value); };
    q('syncBtn').onclick = runSync;
    if (sel.value) loadReport(sel.value);
    else showEmpty();
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init);
  else init();
})();
