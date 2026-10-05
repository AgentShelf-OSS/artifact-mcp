// Trusted shell action bridge. Published HTML never receives a worker URL.
(function () {
  'use strict';
  var config = document.getElementById('shell-config'), frame = document.getElementById('vframe');
  if (!config || !frame) return;
  var c = config.dataset, id = JSON.parse(c.artifactId), query = new URLSearchParams(location.search);
  var current = c.stateEnabled === '1' && c.isBundle !== '1' && !query.has('v') && !query.has('revision');
  var enabled = false, grants = [], pending = 0;
  var ready = current ? fetch('/' + encodeURIComponent(id) + '/actions').then(function (r) { return r.ok ? r.json() : {}; }).then(function (m) { enabled = m.enabled === true; grants = Array.isArray(m.actions) ? m.actions : []; }).catch(function () {}) : Promise.resolve();
  function send(fields) { frame.contentWindow.postMessage(fields, '*'); }
  window.addEventListener('message', function (e) {
    if (e.source !== frame.contentWindow || !e.data || typeof e.data !== 'object') return;
    var m = e.data;
    if (m.type === 'action:hello') { ready.then(function () { send({type:'action:ready', enabled:enabled, actions:grants}); }); return; }
    if (m.type !== 'action:start' && m.type !== 'action:status') return;
    if (typeof m.requestId !== 'string' || !/^[A-Za-z0-9._-]{1,64}$/.test(m.requestId)) return;
    ready.then(function () {
      if (!enabled || grants.indexOf(m.action) < 0 || pending >= 4) { send({type:'action:error',requestId:m.requestId,reason:'action_unavailable'}); return; }
      pending += 1;
      var controller = new AbortController(), timer = setTimeout(function () { controller.abort(); }, 8000);
      var init = {signal:controller.signal};
      if (m.type === 'action:start') init = {signal:controller.signal,method:'POST',headers:{'Content-Type':'application/json','x-artifact-mutation':'1'},body:JSON.stringify({request_id:m.requestId})};
      fetch('/' + encodeURIComponent(id) + '/actions/' + encodeURIComponent(m.action),init).then(function (r) { if (!r.ok) throw new Error(); return r.json(); })
        .then(function (run) { send({type:'action:result',requestId:m.requestId,run:run}); })
        .catch(function () { send({type:'action:error',requestId:m.requestId,reason:'action_unavailable'}); })
        .finally(function () { clearTimeout(timer);pending -= 1; });
    });
  });
})();
