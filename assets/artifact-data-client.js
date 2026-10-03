// Trusted viewer-shell bridge for named artifact data sources.
(function (root) {
  'use strict';
  var parentWindow = root.parent;
  var enabled = false, bindings = {}, nextId = 0, settled = false;
  var pending = new Map(), listeners = new Map(), activeSubscriptions = new Map(), readyResolve;
  var ready = new Promise(function (resolve) { readyResolve = resolve; });
  function id() { nextId += 1; return 'data-' + nextId; }
  function post(message) { try { parentWindow.postMessage(message, '*'); } catch (_) {} }
  function validName(value) { return typeof value === 'string' && /^[A-Za-z0-9._-]{1,64}$/.test(value); }
  function finishReady(value) { if (settled) return; settled = true; enabled = value.enabled === true; bindings = value.bindings && typeof value.bindings === 'object' ? value.bindings : {}; readyResolve({ enabled: enabled, bindings: bindings }); }
  function failPending(requestId, reason) { var item = pending.get(requestId); if (!item) return; pending.delete(requestId); clearTimeout(item.timer); if (item.signal && item.abort) item.signal.removeEventListener('abort', item.abort); var error = new Error(typeof reason === 'string' ? reason : 'data_unavailable'); if (reason === 'aborted') error.name = 'AbortError'; item.reject(error); }
  function receive(event) {
    if (event.source !== parentWindow || !event.data || typeof event.data !== 'object') return;
    var data = event.data;
    if (data.type === 'data:ready') { finishReady(data); return; }
    if (data.type === 'data:result' || data.type === 'data:error') {
      var item = pending.get(data.requestId); if (!item) return; pending.delete(data.requestId); clearTimeout(item.timer); if (item.signal && item.abort) item.signal.removeEventListener('abort', item.abort);
      if (data.type === 'data:error') item.reject(new Error(typeof data.reason === 'string' ? data.reason : 'data_unavailable')); else item.resolve(data.data); return;
    }
    if (data.type === 'data:event') {
      var key = String(data.binding || '') + ':' + String(data.subscription || ''), envelope = { binding: data.binding, subscription: data.subscription, event: data.event, id: data.id, data: data.data };
      var subscription = activeSubscriptions.get(data.requestId); if (subscription && subscription.key === key) { try { subscription.callback(envelope); } catch (_) {} }
    }
  }
  root.addEventListener('message', receive);
  function query(binding, operation, params, options) {
    options = options || {}; params = params || {};
    return ready.then(function (state) {
      if (!state.enabled) throw new Error('data_unavailable');
      if (pending.size >= 32) throw new Error('data_unavailable');
      if (!validName(binding) || !validName(operation) || !params || typeof params !== 'object' || Array.isArray(params)) throw new Error('bad_params');
      var requestId = id();
      return new Promise(function (resolve, reject) {
        var timer = setTimeout(function () { post({ type: 'data:cancel', requestId: requestId }); failPending(requestId, 'data_unavailable'); }, 32000), abort;
        pending.set(requestId, { resolve: resolve, reject: reject, timer: timer, signal: options.signal, abort: abort });
        if (options.signal) { if (options.signal.aborted) { failPending(requestId, 'aborted'); return; } abort = function () { if (pending.has(requestId)) { failPending(requestId, 'aborted'); post({ type: 'data:cancel', requestId: requestId }); } }; pending.get(requestId).abort = abort; options.signal.addEventListener('abort', abort, { once: true }); }
        post({ type: 'data:query', requestId: requestId, binding: binding, operation: operation, params: params });
      });
    });
  }
  function subscribe(binding, subscription, callback) {
    if (!validName(binding) || !validName(subscription) || typeof callback !== 'function') return function () {};
    var key = binding + ':' + subscription, set = listeners.get(key); if (!set) { set = new Set(); listeners.set(key, set); }
    if (activeSubscriptions.size >= 64) return function () {};
    set.add(callback); var requestId = id(), active = true, subscriptionKey = key;
    activeSubscriptions.set(requestId, { binding: binding, subscription: subscription, key: subscriptionKey, callback: callback });
    ready.then(function (state) { if (active && state.enabled) post({ type: 'data:subscribe', requestId: requestId, binding: binding, subscription: subscription }); });
    return function () { active = false; activeSubscriptions.delete(requestId); var current = listeners.get(key); if (current) { current.delete(callback); if (!current.size) listeners.delete(key); } if (settled && enabled) post({ type: 'data:unsubscribe', requestId: requestId, binding: binding, subscription: subscription }); };
  }
  root.artifact = root.artifact || {}; root.artifact.data = { ready: ready, query: query, subscribe: subscribe };
  root.addEventListener('pagehide', function () { pending.forEach(function (_, requestId) { failPending(requestId, 'aborted'); post({ type: 'data:cancel', requestId: requestId }); }); activeSubscriptions.forEach(function (value, requestId) { post({ type: 'data:unsubscribe', requestId: requestId, binding: value.binding, subscription: value.subscription }); }); activeSubscriptions.clear(); listeners.clear(); });
  function retryHello() { if (settled) return; post({ type: 'data:hello' }); setTimeout(retryHello, 100); }
  retryHello();
  setTimeout(function () { finishReady({ enabled: false, bindings: {} }); }, 1100);
})(window);
