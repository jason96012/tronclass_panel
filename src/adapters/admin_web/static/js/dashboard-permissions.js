(function(){
  'use strict';
  if(window.NON_ADMIN_MOBILE_PERMISSIONS_FIX_V1) return;
  window.NON_ADMIN_MOBILE_PERMISSIONS_FIX_V1 = true;

  var roleState = null;
  var applying = false;
  /* SETTINGS_FOR_ALL_V1: 「設定」已改為全民獨立頁 /admin/settings，自 admin 專屬文字移除 */
  var adminOnlyText = /^(系統\s*Log|系統\s*Logs|Logs?|維護)$/i;
  var adminOnlyViews = {maintenance:1, logs:1, health:1};

  function q(id){ return document.getElementById(id); }
  function token(){
    try{
      var urlToken = '';
      if(urlToken) return String(urlToken).trim();
      var keys = ['tronclass_admin_session','tronclass_admin_token','adminToken','token'];
      for(var i=0;i<keys.length;i++){
        var v = localStorage.getItem(keys[i]) || sessionStorage.getItem(keys[i]);
        if(v && String(v).trim()) return String(v).trim();
      }
    }catch(_){ }
    return '';
  }
  function withToken(path){
    var t = token();
    if(!t) return path;
    return path;
  }
  async function readJson(res){
    var body = await res.json().catch(function(){return {};});
    if(!res.ok){ var e = new Error(body.error || body.message || ('HTTP ' + res.status)); e.status=res.status; throw e; }
    return body;
  }
  async function loadStatus(){
    // Dashboard authentication is cookie-only.  Do not require a legacy token
    // from Web Storage here: regular users have no such token, which used to
    // prevent their role from loading and left admin-only navigation visible.
    if(window.status && typeof window.status === 'object' && 'is_admin' in window.status) return window.status;
    if(window.__tronAuthStatus && typeof window.__tronAuthStatus === 'object' && 'is_admin' in window.__tronAuthStatus) return window.__tronAuthStatus;
    try{ return await fetch(withToken('/admin/api/status?include_history=false'), {cache:'no-store',credentials:'same-origin'}).then(readJson); }
    catch(_){ return null; }
  }

  function showRoleToast(text){
    var el = q('roleGuardToast');
    if(!el){
      el = document.createElement('div');
      el.id = 'roleGuardToast';
      el.className = 'role-guard-toast';
      document.body.appendChild(el);
    }
    el.textContent = text || '此功能僅管理員可用';
    el.classList.add('visible');
    clearTimeout(el._timer);
    el._timer = setTimeout(function(){ el.classList.remove('visible'); }, 2200);
  }

  function isLoggedInBody(){
    return document.body.classList.contains('logged-in') || !!q('mobileNav') || !!document.querySelector('.protected');
  }

  function textOf(el){ return (el && (el.textContent || '') || '').replace(/\s+/g,' ').trim(); }

  function markAndHideByText(root){
    if(!root) root = document;
    var scope = root.querySelectorAll ? root : document;
    var candidates = scope.querySelectorAll('a,button,[role="button"],.tab');
    Array.prototype.forEach.call(candidates, function(el){
      var t = textOf(el);
      if(adminOnlyText.test(t)){
        // 只在 Dashboard 的 mobile sheet / tabs / toolbar 內套用文字判斷，避免誤傷其他頁面文字。
        if(el.closest('.mobile-sheet,.sheet,.sheet-grid,.tabs,.mobile-nav') || el.classList.contains('tab')){
          el.dataset.roleAdminOnly = '1';
          if(roleState && roleState.is_admin === false) el.style.display = 'none';
        }
      }
    });
  }

  function hideAdminOnlyLinks(){
    // SETTINGS_FOR_ALL_V1: settings 相關選擇器已移除（設定頁全民可用）
    var selectors = [
      '.tab[data-view="maintenance"]',
      '[data-view="maintenance"]',
      '[data-switch="maintenance"]',
      '#view-maintenance',
      'a[href*="/admin/logs"]', 'a[href*="/admin/health"]', 'a[href*="/admin/activity"]',
      '#moreBlacklistFinalV2', '#mActivity', '#moreActivityFinalV2',
      '[data-admin-only="1"]', '.admin-only', '[data-role-admin-only]'
    ];
    selectors.forEach(function(sel){
      document.querySelectorAll(sel).forEach(function(el){
        el.dataset.roleAdminOnly = '1';
        el.style.display = 'none';
        el.style.visibility = 'hidden';
        el.style.pointerEvents = 'none';
      });
    });
    markAndHideByText(document);
  }

  function showAdminOnlyLinks(){
    document.querySelectorAll('[data-role-admin-only="1"]').forEach(function(el){
      el.style.display = '';
      el.style.visibility = '';
      el.style.pointerEvents = '';
    });
  }

  function currentView(){
    var h = (location.hash || '').replace(/^#/, '').trim();
    if(h) return h;
    var active = document.querySelector('.view.active');
    if(active && active.id && active.id.indexOf('view-') === 0) return active.id.slice(5);
    return '';
  }

  function switchSafeView(){
    var v = currentView();
    if(!adminOnlyViews[v]) return;
    var safe = 'overview';
    document.querySelectorAll('.view').forEach(function(el){ el.classList.toggle('active', el.id === 'view-' + safe); });
    document.querySelectorAll('.tab').forEach(function(el){ el.classList.toggle('active', el.dataset.view === safe); });
    document.querySelectorAll('.mobile-nav button[data-view]').forEach(function(el){ el.classList.toggle('active', el.dataset.view === safe); });
    try{ history.replaceState(null, '', '#overview'); }catch(_){ }
  }

  function cleanUnauthorizedArtifacts(){
    if(!roleState || roleState.is_admin !== false || !isLoggedInBody()) return;
    var nodes = document.querySelectorAll('.toast,.bad,.error,.alert,.notice,.panel,.card,pre,div');
    Array.prototype.forEach.call(nodes, function(el){
      if(!el || el.closest('#loginCard,.login-card')) return;
      var t = textOf(el).toLowerCase();
      if(t === 'unauthorized' || t === '讀取失敗：unauthorized' || t === '讀取失敗: unauthorized'){
        el.style.display = 'none';
        el.style.visibility = 'hidden';
      }
    });
  }

  function closeMobileSheet(){
    document.querySelectorAll('#mobileSheet,.mobile-sheet,.mobile-dashboard-sheet-backdrop,.mobile-dashboard-sheet-force').forEach(function(el){
      el.classList.remove('visible','open','show','active');
      if(el.id === 'mobileSheet' || el.classList.contains('mobile-sheet')){
        // 不強制 display none，避免破壞原本開關；只移除 visible。
      }
    });
  }

  function isAdminOnlyTarget(el){
    if(!el) return false;
    var node = el.closest('a,button,[role="button"],.tab,[data-view],[data-switch]');
    if(!node) return false;
    var view = node.dataset && (node.dataset.view || node.dataset.switch || node.dataset.sheetView || '');
    if(adminOnlyViews[view]) return true;
    var href = node.getAttribute && (node.getAttribute('href') || '');
    if(/\/admin\/(logs|health)(\?|#|$)/.test(href)) return true;
    if(node.dataset && (node.dataset.adminOnly === '1' || node.dataset.roleAdminOnly === '1')) return true;
    var t = textOf(node);
    if((node.closest('.mobile-sheet,.sheet,.sheet-grid,.tabs,.mobile-nav') || node.classList.contains('tab')) && adminOnlyText.test(t)) return true;
    return false;
  }

  function applyRollcallAccountFilter(st){
    var filter = q('historyAccountFilter');
    if(!filter) return;
    /* VIEWER_ROLE_V1：檢視者與管理員一樣看得到全部帳號 */
    if(st.is_admin || st.is_viewer){
      filter.disabled = false;
      if(!filter.querySelector('option[value=""]')){
        var all = document.createElement('option');
        all.value = ''; all.textContent = '全部帳號';
        filter.insertBefore(all, filter.firstChild);
      }
      return;
    }
    // The status API already limits regular users to their own account; make
    // that scope explicit in the UI instead of showing a misleading “全部帳號”.
    var accountId = String(st.current_account_id || st.account_id || '');
    var account = (st.accounts || []).find(function(item){ return String(item.account_id || '') === accountId; });
    var label = accountId;
    if(account && (account.user_name || account.username)) label += ' ' + (account.user_name || account.username);
    filter.innerHTML = '';
    var own = document.createElement('option');
    own.value = accountId; own.textContent = label || '我的帳號'; own.selected = true;
    filter.appendChild(own);
    filter.disabled = true;
  }

  /* VIEWER_ROLE_V1：檢視者（唯讀）。後端 middleware 已擋掉所有寫入；這裡只做 UX：
     顯示徽章、把會改變狀態的按鈕停用並加上提示，避免使用者按了才看到 403。 */
  var viewerMutating = /^(檢查|重登|暫停|恢復|啟動監控|設為管理員|取消管理員|設為檢視者|取消檢視者|重製密碼|儲存|新增帳號|立即檢查|全部檢查|全部重登|測試|送出|清除|套用|匯入|同步)/;
  function applyViewerUi(){
    if(!roleState || !roleState.is_viewer) return;
    if(!document.getElementById('viewerRoleBadge')){
      var host = document.querySelector('.hero-copy') || document.querySelector('.hero') || document.querySelector('main');
      if(host){
        var badge = document.createElement('span');
        badge.id = 'viewerRoleBadge';
        badge.setAttribute('role','status');
        badge.textContent = '檢視者（唯讀）：可以看所有帳號狀態，但不能執行任何操作';
        badge.style.cssText = 'display:inline-flex;align-items:center;gap:6px;margin-top:8px;padding:6px 12px;border-radius:999px;font-size:12px;font-weight:800;background:rgba(245,158,11,.16);color:#b45309;border:1px solid rgba(245,158,11,.35)';
        host.appendChild(badge);
      }
    }
    var buttons = document.querySelectorAll('button[data-action],#view-accounts button,#view-overview button,#view-rollcall button,#view-timetable button,.actions-cell button,.quick-actions button');
    Array.prototype.forEach.call(buttons, function(btn){
      if(!btn || btn.dataset.viewerLocked === '1') return;
      var t = textOf(btn);
      var act = (btn.dataset && btn.dataset.action) || '';
      if(act || viewerMutating.test(t)){
        if(/^(重新整理|關閉|取消|返回|登出|重新讀取|重試|上月|下月|下載|匯出)/.test(t)) return;
        btn.dataset.viewerLocked = '1';
        btn.disabled = true;
        btn.title = '檢視者（唯讀）不能執行此操作';
        btn.style.opacity = '.45';
        btn.style.cursor = 'not-allowed';
      }
    });
  }

  function applyRole(st){
    if(!st || typeof st.is_admin === 'undefined') return;
    roleState = {is_admin: !!st.is_admin, is_viewer: !!st.is_viewer && !st.is_admin, account_id: st.current_account_id || st.account_id || ''};
    applyRollcallAccountFilter(st);
    document.documentElement.classList.toggle('dash-role-admin', roleState.is_admin);
    document.documentElement.classList.toggle('dash-role-user', !roleState.is_admin);
    document.documentElement.classList.toggle('dash-role-viewer', roleState.is_viewer);
    document.documentElement.dataset.admin = roleState.is_admin ? '1' : '0';
    document.documentElement.dataset.viewer = roleState.is_viewer ? '1' : '0';
    applyViewerUi();

    if(roleState.is_admin){
      showAdminOnlyLinks();
    }else{
      hideAdminOnlyLinks();
      switchSafeView();
      cleanUnauthorizedArtifacts();
    }
  }

  async function bootRole(){
    if(applying) return;
    applying = true;
    try{
      var st = await loadStatus();
      if(st) applyRole(st);
    }finally{
      applying = false;
    }
  }

  document.addEventListener('click', function(ev){
    if(!roleState || roleState.is_admin !== false) return;
    if(isAdminOnlyTarget(ev.target)){
      ev.preventDefault();
      ev.stopPropagation();
      closeMobileSheet();
      switchSafeView();
      cleanUnauthorizedArtifacts();
      showRoleToast('此功能僅管理員可用');
    }
  }, true);

  // 若原本程式把 unauthorized 顯示成 toast/card，非管理員狀態下自動清掉。
  var mo = new MutationObserver(function(muts){
    if(roleState && roleState.is_viewer) applyViewerUi();
    if(roleState && roleState.is_admin === false){
      muts.forEach(function(m){
        Array.prototype.forEach.call(m.addedNodes || [], function(n){
          if(n.nodeType === 1) markAndHideByText(n);
        });
      });
      hideAdminOnlyLinks();
      cleanUnauthorizedArtifacts();
    }
  });

  function start(){
    bootRole();
    try{ mo.observe(document.documentElement, {childList:true, subtree:true}); }catch(_){ }
    var n = 0;
    var timer = setInterval(function(){
      bootRole();
      if(roleState && roleState.is_admin === false){ hideAdminOnlyLinks(); cleanUnauthorizedArtifacts(); }
      n++;
      if(n >= 40) clearInterval(timer);
    }, 750);
    document.addEventListener('visibilitychange', function(){ if(!document.hidden) bootRole(); });
  }

  if(document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
  else start();
})();
