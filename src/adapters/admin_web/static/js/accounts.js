/* ACCOUNTS_MODULE_PHASE4E2 */
(function(){
  'use strict';
  function start(){
    if(window.TRONCLASS_ACCOUNTS_MODULE_V1)return;
    window.TRONCLASS_ACCOUNTS_MODULE_V1=true;
    function bridge(){return window.TronclassDashboard&&window.TronclassDashboard.accounts}
    function action(name,account){var a=bridge();return a&&a.action?a.action(name,account):Promise.reject(new Error('Accounts API not ready'))}
    window.TronclassAccountsModule={refresh:function(){var a=bridge();return a&&a.render?a.render():null},list:function(){var a=bridge();return a&&a.list?a.list():[]},force:function(id){return action('force',id)},reauth:function(id){return action('reauth',id)},start:function(id){return action('start',id)},stop:function(id){return action('stop',id)},setAdmin:function(id){return action('set_admin',id)},unsetAdmin:function(id){return action('unset_admin',id)},resetPassword:function(id){return action('reset_password',id)}};
    window.addEventListener('tronclass:accounts:refresh',function(){window.TronclassAccountsModule.refresh()});
    window.addEventListener('tronclass:accounts:force',function(e){window.TronclassAccountsModule.force(e.detail&&e.detail.accountId)});
    window.addEventListener('tronclass:accounts:reauth',function(e){window.TronclassAccountsModule.reauth(e.detail&&e.detail.accountId)});
  }
  if(window.TronclassDashboard&&window.TronclassDashboard.ready)start();else window.addEventListener('tronclass:dashboard:ready',start,{once:true});
})();
