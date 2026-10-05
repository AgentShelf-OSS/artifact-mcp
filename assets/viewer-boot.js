// Keep only the initial state handshake until the trusted shell is ready.
(function () {
  var pending = null;
  function receive(event) {
    var frame = document.getElementById('vframe'), data = event.data;
    if (frame && event.source === frame.contentWindow && data && typeof data === 'object' && !Array.isArray(data) && data.type === 'state:hello') pending = { type: 'state:hello' };
  }
  window.addEventListener('message', receive);
  window.__artifactMcpStateBoot = {
    take: function () {
      window.removeEventListener('message', receive);
      delete window.__artifactMcpStateBoot;
      var value = pending; pending = null;
      return value;
    }
  };
})();
