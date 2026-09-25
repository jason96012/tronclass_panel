/* MAINTENANCE_MODULE_PHASE4A2B — facade only; existing inline button handlers remain active. */
(function(){
  'use strict';
  if(window.TRONCLASS_MAINTENANCE_MODULE_V1)return;
  window.TRONCLASS_MAINTENANCE_MODULE_V1=true;
  function api(){return window.TronclassDashboard&&window.TronclassDashboard.maintenance}
  function call(name){var m=api(); if(!m||typeof m[name]!=='function'){console.warn('Maintenance API not ready:',name);return Promise.resolve(null)} return Promise.resolve(m[name]())}
  window.TronclassMaintenanceModule={refresh:function(){return call('refreshAll')},check:function(){return call('loadHealthCheck')},copyDiagnostic:function(){return call('copyDiagnostic')},stopAll:function(){return call('stopAll')}};
  window.addEventListener('tronclass:maintenance:refresh',function(){window.TronclassMaintenanceModule.refresh()});
  window.addEventListener('tronclass:maintenance:check',function(){window.TronclassMaintenanceModule.check()});
  window.addEventListener('tronclass:maintenance:copy',function(){window.TronclassMaintenanceModule.copyDiagnostic()});
  window.addEventListener('tronclass:maintenance:stop',function(){window.TronclassMaintenanceModule.stopAll()});
})();
