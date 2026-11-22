/* TT_WALLPAPER_V1 — iPhone 直式課表桌布 */
(function (global) {
  'use strict';
  var W = 1290;
  var H = 2796;

  function wallpaperName() {
    var el = document.getElementById('brandSub');
    var n = el && String(el.textContent || '').trim();
    if (n && n !== '載入中' && n !== '已登入') return n;
    var sel = document.getElementById('accountSelect') || document.getElementById('ttStableAccount');
    if (sel && sel.options && sel.selectedIndex >= 0) {
      n = String(sel.options[sel.selectedIndex].text || '').trim();
      if (n) return n;
    }
    return '我的課表';
  }

  function isDark() {
    var t = document.documentElement.getAttribute('data-theme');
    if (t === 'dark') return true;
    if (t === 'light') return false;
    return !!(window.matchMedia && window.matchMedia('(prefers-color-scheme:dark)').matches);
  }

  function rr(ctx, x, y, w, h, r) {
    var rad = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + rad, y);
    ctx.arcTo(x + w, y, x + w, y + h, rad);
    ctx.arcTo(x + w, y + h, x, y + h, rad);
    ctx.arcTo(x, y + h, x, y, rad);
    ctx.arcTo(x, y, x + w, y, rad);
    ctx.closePath();
  }

  function wrapText(ctx, text, maxW, maxLines) {
    var chars = Array.from(String(text || ''));
    var lines = [];
    var cur = '';
    for (var i = 0; i < chars.length; i++) {
      var t = cur + chars[i];
      if (cur && ctx.measureText(t).width > maxW) {
        lines.push(cur);
        cur = chars[i];
        if (lines.length >= maxLines) return lines;
      } else cur = t;
    }
    if (cur && lines.length < maxLines) lines.push(cur);
    return lines;
  }

  function collectBlocks(list) {
    var occupied = Object.create(null);
    var blocks = [];
    (list || []).forEach(function (e) {
      var name = String(e.course_name || '').trim();
      if (!name) return;
      var ps = Math.max(1, Math.min(10, Number(e.period_start || 1) || 1));
      var pe = Math.max(ps, Math.min(10, Number(e.period_end || ps) || ps));
      var clash = false;
      for (var p = ps; p <= pe; p++) if (occupied[e.weekday + ':' + p]) clash = true;
      if (clash) return;
      for (var q = ps; q <= pe; q++) occupied[e.weekday + ':' + q] = true;
      blocks.push({ day: e.weekday, ps: ps, pe: pe, name: name, room: String(e.classroom || e.teacher || '').trim() });
    });
    return { occupied: occupied, blocks: blocks };
  }

  function drawWallpaper(entries, periodMap) {
    var PERIOD = periodMap || {};
    var dark = isDark();
    var bg = dark ? '#0b1220' : '#eef3fb';
    var card = dark ? '#182232' : '#ffffff';
    var soft = dark ? '#121a28' : '#f8fafc';
    var text = dark ? '#f1f5f9' : '#172033';
    var muted = dark ? '#94a3b8' : '#64748b';
    var line = dark ? '#2a3a52' : '#dbe3ef';
    var periodBg = dark ? '#334155' : '#64748b';
    var accent = '#3b82f6';
    var courseBg = dark ? '#243656' : '#e8eef9';

    var data = collectBlocks(entries);
    var days = [['mon', '一'], ['tue', '二'], ['wed', '三'], ['thu', '四'], ['fri', '五']];
    if (data.blocks.some(function (b) { return b.day === 'sat'; })) days.push(['sat', '六']);
    if (data.blocks.some(function (b) { return b.day === 'sun'; })) days.push(['sun', '日']);

    var canvas = document.createElement('canvas');
    canvas.width = W;
    canvas.height = H;
    var ctx = canvas.getContext('2d');
    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, W, H);
    var g = ctx.createLinearGradient(0, 0, W, H);
    if (dark) {
      g.addColorStop(0, 'rgba(37,99,235,.18)');
      g.addColorStop(1, 'rgba(15,23,42,0)');
    } else {
      g.addColorStop(0, 'rgba(37,99,235,.08)');
      g.addColorStop(1, 'rgba(255,255,255,0)');
    }
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);

    var padX = 72;
    var topSafe = 260;
    var bottomSafe = 480;
    ctx.fillStyle = text;
    ctx.font = '800 42px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.textAlign = 'center';
    ctx.fillText('WEEKLY', W / 2, topSafe - 70);
    ctx.font = '900 86px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText('課表', W / 2, topSafe + 20);
    ctx.fillStyle = muted;
    ctx.font = '700 36px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText(wallpaperName(), W / 2, topSafe + 78);

    var gridX = padX;
    var gridY = topSafe + 130;
    var gridW = W - padX * 2;
    var gridH = H - gridY - bottomSafe;
    rr(ctx, gridX - 16, gridY - 24, gridW + 32, gridH + 48, 48);
    ctx.fillStyle = soft;
    ctx.fill();

    var leftW = 150;
    var colW = (gridW - leftW) / days.length;
    var rowH = gridH / 11;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    days.forEach(function (d, i) {
      var x = gridX + leftW + i * colW;
      ctx.fillStyle = muted;
      ctx.font = '800 32px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillText(d[1], x + colW / 2, gridY + rowH / 2);
    });

    for (var p = 1; p <= 10; p++) {
      var y = gridY + p * rowH;
      rr(ctx, gridX + 8, y + 8, leftW - 20, rowH - 16, 22);
      ctx.fillStyle = periodBg;
      ctx.fill();
      ctx.fillStyle = '#fff';
      ctx.font = '900 34px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillText(String(p), gridX + leftW / 2 - 2, y + rowH / 2 - 14);
      var tm = PERIOD[p] || ['', ''];
      ctx.font = '700 18px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
      ctx.fillStyle = 'rgba(255,255,255,.88)';
      ctx.fillText(String(tm[0] || ''), gridX + leftW / 2 - 2, y + rowH / 2 + 16);
    }

    days.forEach(function (d, i) {
      for (var p = 1; p <= 10; p++) {
        var x = gridX + leftW + i * colW;
        var y = gridY + p * rowH;
        var start = null;
        for (var b = 0; b < data.blocks.length; b++) {
          if (data.blocks[b].day === d[0] && data.blocks[b].ps === p) { start = data.blocks[b]; break; }
        }
        if (start) {
          var span = start.pe - start.ps + 1;
          rr(ctx, x + 8, y + 8, colW - 16, rowH * span - 16, 24);
          ctx.fillStyle = courseBg;
          ctx.fill();
          ctx.strokeStyle = line;
          ctx.lineWidth = 2;
          ctx.stroke();
          ctx.fillStyle = text;
          ctx.font = '800 28px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
          var lines = wrapText(ctx, start.name, colW - 36, span >= 2 ? 3 : 2);
          var startY = y + (rowH * span) / 2 - ((lines.length + (start.room ? 1 : 0) - 1) * 18);
          lines.forEach(function (ln, li) {
            ctx.fillText(ln, x + colW / 2, startY + li * 36);
          });
          if (start.room) {
            ctx.fillStyle = muted;
            ctx.font = '700 22px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
            ctx.fillText(start.room, x + colW / 2, startY + lines.length * 36);
          }
        } else if (!data.occupied[d[0] + ':' + p]) {
          rr(ctx, x + 8, y + 8, colW - 16, rowH - 16, 22);
          ctx.fillStyle = card;
          ctx.fill();
        }
      }
    });

    ctx.fillStyle = muted;
    ctx.font = '600 24px -apple-system,BlinkMacSystemFont,system-ui,sans-serif';
    ctx.fillText('長按圖片 → 加入照片 → 設為桌布', W / 2, H - 200);
    return canvas;
  }

  function exportTimetableWallpaper(entries, periodMap) {
    var real = (entries || []).filter(function (e) { return String(e.course_name || '').trim(); });
    if (!real.length) {
      alert('目前沒有課表可以匯出');
      return;
    }
    var canvas = drawWallpaper(real, periodMap);
    var name = '課表桌布-' + wallpaperName() + '.png';
    canvas.toBlob(function (blob) {
      if (!blob) return;
      var file = new File([blob], name, { type: 'image/png' });
      if (navigator.share && navigator.canShare && navigator.canShare({ files: [file] })) {
        navigator.share({ files: [file], title: '課表桌布' }).catch(function () { saveBlob(blob, name); });
        return;
      }
      saveBlob(blob, name);
    }, 'image/png');
  }

  function saveBlob(blob, name) {
    var url = URL.createObjectURL(blob);
    var a = document.createElement('a');
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(function () { URL.revokeObjectURL(url); }, 4000);
  }

  global.exportTimetableWallpaper = exportTimetableWallpaper;
})(window);
