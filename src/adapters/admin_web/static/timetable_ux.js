(function(){
'use strict';
document.documentElement.classList.add('tron-auth-ok','dash-auth-ok','mobile-nav-final-auth');
if(document.body) document.body.classList.add('auth-ok','dash-auth-ok','logged-in');
if(window.TT_UX)return;
window.TT_UX=true;
var _d=false;
addEventListener("beforeunload",function(e){if(_d){e.preventDefault();e.returnValue=""}});
function m(){_d=true;rf();}
function rf(){var b=document.getElementById("saveBtn");if(!b)return;if(_d){b.textContent="儲存課表（未儲存）";b.style.background="#dc2626";b.style.borderColor="#dc2626";b.style.color="#fff"}else{b.textContent="儲存課表";b.style.background="";b.style.borderColor="";b.style.color=""}}
function tmg(m,t){var e=document.getElementById("toast");if(!e)return;e.textContent=m;var c={ok:"#15803d",bad:"#991b1b",warn:"#b45309"};e.style.background=c[t]||"#172033";e.classList.add("visible");clearTimeout(e._tid);e._tid=setTimeout(function(){e.classList.remove("visible")},3000)}
function wr(){document.querySelectorAll("#rows .row").forEach(function(r){if(r.dataset.w)return;r.dataset.w="1";var d=r.querySelector(".del");if(d){var o=d.onclick;d.onclick=function(e){if(!confirm("確定刪除這堂課？"))return;if(o)o.call(this,e);m();if(typeof renderEdit==="function")renderEdit()}};r.querySelectorAll("input,select").forEach(function(el){el.addEventListener("change",m);el.addEventListener("input",m)})})}
var tb=document.getElementById("applyTemplateBtn");if(tb){var ot=tb.onclick;tb.onclick=function(){var s=document.getElementById("templateSelect");if(!s||!s.value)return tmg("請先選擇模板","warn");if(!confirm("套用模板會覆蓋目前課表，確定？"))return;if(ot)ot.call(this)}}
var eb=document.getElementById("editBtn");if(eb){eb.style.transition="all .2s";eb.addEventListener("mouseenter",function(){this.style.transform="scale(1.03)"});eb.addEventListener("mouseleave",function(){this.style.transform=""})}
var or=window.renderEdit;if(typeof or==="function"){window.renderEdit=function(){_d=false;or();setTimeout(function(){wr();rf()},100)}}
var ab=document.getElementById("addRowBtn");if(ab){var oa=ab.onclick;ab.onclick=function(){if(oa)oa.call(this);m();setTimeout(function(){wr();rf()},300)}}
document.addEventListener("click",function(e){if(!e.target||e.target.id!=="saveBtn")return;document.querySelectorAll("#rows .row").forEach(function(r){if(typeof syncRow==="function")syncRow(r)});var b=document.getElementById("saveBtn");if(b){b.disabled=true;b.textContent="儲存中..."}setTimeout(function(){var b2=document.getElementById("saveBtn");if(b2){b2.disabled=false;_d=false;rf()}tmg("課表已儲存","ok")},700)},true);
setTimeout(function(){wr();rf()},1500);
})();
