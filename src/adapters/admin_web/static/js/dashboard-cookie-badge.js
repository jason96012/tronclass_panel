(function(){
  'use strict';
  if(window.SCHOOL_COOKIE_STATUS_BADGE_V2) return;
  window.SCHOOL_COOKIE_STATUS_BADGE_V2 = true;

  var lastStatus = null;
  var applying = false;
  var scheduled = false;
  var TOKEN_KEYS = ['tronclass_admin_session','tronclass_admin_token','tronclass_dashboard_token','tronclass_session_token','adminToken','admin_token','token'];

  function q(id){ return document.getElementById(id); }
  function esc(v){ return String(v == null ? '' : v).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];}); }
  function textOf(el){ return (el && (el.textContent || '') || '').replace(/\s+/g,' ').trim(); }
  function token(){
    try{
      var u = '';
      if(u) return String(u).trim();
      for(var i=0;i<TOKEN_KEYS.length;i++){
        var v = localStorage.getItem(TOKEN_KEYS[i]) || sessionStorage.getItem(TOKEN_KEYS[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){ }
    return '';
  }
  function withToken(path){
    var t = token();
    if(!t) return path;
    var u = new URL(path, location.origin);
    // token removed: auth via HttpOnly cookie
    return u.pathname + u.search + u.hash;
  }
  async function parseResponse(res){
    var body = await res.json().catch(function(){ return {}; });
    if(!res.ok){ var e = new Error(body.error || body.message || ('HTTP '+res.status)); e.status = res.status; throw e; }
    return body;
  }
  async function loadStatus(){
    if(!token()) return null;
    if(window.status && typeof window.status === 'object' && Array.isArray(window.status.accounts)){
      lastStatus = window.status;
      return lastStatus;
    }
    try{
      lastStatus = await fetch(withToken('/admin/api/status'), {cache:'no-store'}).then(parseResponse);
      return lastStatus;
    }catch(_){ return lastStatus; }
  }
  function fmtShort(ts){
    if(!ts) return '';
    try{ return new Date(Number(ts)*1000).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit'}); }
    catch(_){ return ''; }
  }
  function stateFor(account){
    var failures = Number(account.consecutive_failures || 0);
    var userName = String(account.user_name || '').trim();
    var running = !!account.is_running;
    var checked = account.last_poll_timestamp || account.started_at || 0;
    if(!running){
      return {cls:'muted', label:'學校 Cookie：暫停中', hint:'此帳號目前暫停監控。', time:fmtShort(checked)};
    }
    if(failures > 0){
      return {cls:'warn', label:'學校 Cookie：需確認', hint:'最近監控有失敗，可能是 Cookie 過期或學校系統暫時異常。請按「重登」。', time:fmtShort(checked)};
    }
    if(userName){
      return {cls:'ok', label:'學校 Cookie：有效', hint:'監控器最近已成功登入學校系統。', time:fmtShort(checked)};
    }
    return {cls:'muted', label:'學校 Cookie：尚未檢查', hint:'目前還沒有足夠資訊判斷。', time:fmtShort(checked)};
  }
  function badgeHtml(account){
    var s = stateFor(account);
    return '<span class="school-cookie-badge '+esc(s.cls)+'" title="'+esc(s.hint)+'"><span class="dot"></span><span>'+esc(s.label)+'</span>'+(s.time?'<small>'+esc(s.time)+'</small>':'')+'</span>';
  }
  function cleanupWrongBadges(){
    document.querySelectorAll('.school-cookie-badge-wrap').forEach(function(wrap){
      if(wrap.closest('button,a,[role="button"],.actions-cell,.account-actions')){
        wrap.remove();
        return;
      }
      var td = wrap.closest('td');
      if(td && td.parentElement){
        var cells = Array.prototype.slice.call(td.parentElement.children || []);
        var idx = cells.indexOf(td);
        // 一般表格第 2 欄才是帳號。其他欄位若被舊版插入，移除。
        if(idx >= 2 || idx === 0){ wrap.remove(); }
      }
    });
  }
  function mapAccounts(status){
    var m = {};
    (status && status.accounts || []).forEach(function(a){ if(a && a.account_id) m[a.account_id] = a; });
    return m;
  }
  function candidateRows(){
    var rows = [];
    ['accountsBody','accounts','accountList','accountsTableBody'].forEach(function(id){
      var el = q(id);
      if(el) Array.prototype.push.apply(rows, Array.prototype.slice.call(el.querySelectorAll('tr,.account-row,.account-card,.item')));
    });
    document.querySelectorAll('tbody tr').forEach(function(r){ if(rows.indexOf(r) < 0) rows.push(r); });
    return rows;
  }
  function accountCell(row, account){
    var cells = Array.prototype.slice.call(row.children || []);
    if(cells.length >= 2){
      var c1 = cells[1];
      var t1 = textOf(c1);
      if(t1.indexOf(account.account_id) >= 0 || (account.user_name && t1.indexOf(account.user_name) >= 0) || (account.username && t1.indexOf(account.username) >= 0)) return c1;
    }
    for(var i=0;i<cells.length;i++){
      var t = textOf(cells[i]);
      if(t.indexOf(account.account_id) >= 0 && !cells[i].closest('.actions-cell')) return cells[i];
    }
    return null;
  }
  function applyBadges(status){
    var m = mapAccounts(status);
    var ids = Object.keys(m);
    if(!ids.length) return;
    candidateRows().forEach(function(row){
      var rowText = textOf(row);
      for(var i=0;i<ids.length;i++){
        var account = m[ids[i]];
        if(rowText.indexOf(account.account_id) < 0) continue;
        var cell = accountCell(row, account);
        if(!cell) break;
        var wrap = cell.querySelector(':scope > .school-cookie-badge-wrap');
        if(!wrap){
          wrap = document.createElement('div');
          wrap.className = 'school-cookie-badge-wrap';
          cell.appendChild(wrap);
        }
        wrap.innerHTML = badgeHtml(account);
        break;
      }
    });
  }
  async function apply(){
    if(applying) return;
    applying = true;
    try{
      cleanupWrongBadges();
      var st = await loadStatus();
      if(st) applyBadges(st);
      cleanupWrongBadges();
    }finally{
      applying = false;
    }
  }
  function schedule(){
    if(scheduled) return;
    scheduled = true;
    setTimeout(function(){ scheduled = false; apply(); }, 120);
  }
  function start(){
    apply();
    var n = 0;
    var timer = setInterval(function(){
      apply();
      n++;
      if(n >= 12) clearInterval(timer);
    }, 1200);
    try{
      new MutationObserver(schedule).observe(document.documentElement, {childList:true, subtree:true});
    }catch(_){ }
    document.addEventListener('visibilitychange', function(){ if(!document.hidden) schedule(); });
  }
  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
