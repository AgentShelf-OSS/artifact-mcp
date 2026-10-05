// Trusted viewer bridge for casting one reviewed artifact revision.
// Raw artifact code can request operations through postMessage, but it never
// receives this bridge's publish token, session ID, or network destination.
(function (root) {
  'use strict';

  function createArtifactCastBridge(options) {
    options = options || {};
    var win = options.window || root;
    var doc = options.document || win.document;
    var nav = options.navigator || win.navigator;
    var mediaDevices = options.mediaDevices || nav && nav.mediaDevices;
    var FrameTrack = options.MediaStreamTrack || win.MediaStreamTrack;
    var Target = options.CropTarget || win.CropTarget;
    var Recorder = options.MediaRecorder || win.MediaRecorder;
    var Socket = options.WebSocket || win.WebSocket;
    var fetcher = options.fetch || win.fetch && win.fetch.bind(win);
    var frame = options.frame;
    var getCurrentFrame = options.getCurrentFrame || function () { return frame && frame.contentWindow; };
    var post = options.post || function (message) {
      var active = getCurrentFrame();
      if (active && active.postMessage) active.postMessage(message, '*');
    };
    var artifactId = typeof options.artifactId === 'string' ? options.artifactId : '';
    var revision = Number(options.revision);
    var serverEnabled = options.serverEnabled === true;
    var authenticated = options.authenticated === true;
    var stateEnabled = options.stateEnabled === true;
    var AbortSignalApi = options.AbortSignal || win.AbortSignal || root.AbortSignal;
    var setIntervalFn = options.setInterval || win.setInterval.bind(win);
    var clearIntervalFn = options.clearInterval || win.clearInterval.bind(win);
    var captureActive = false;
    var channel = randomToken();
    var ready = false;
    var session = null;
    var pairing = false;
    var operation = 0;
    var pendingStart = null;
    var stream = null;
    var videoTrack = null;
    var recorder = null;
    var socket = null;
    var pollTimer = null;
    var uploadGeneration = 0;
    var stopping = false;
    var destroyed = false;
    var revoked = false;
    var initialLoadSeen = false;
    var everReady = false;
    var dialog = null;
    var startButton = null;
    var cancelButton = null;
    var dialogStatus = null;
    var captureHandle = randomToken();
    var captureHandleReady = Promise.resolve(false);

    function randomToken() {
      try {
        var bytes = new Uint8Array(24);
        win.crypto.getRandomValues(bytes);
        return Array.prototype.map.call(bytes, function (value) { return value.toString(16).padStart(2, '0'); }).join('');
      } catch (_) {
        return '';
      }
    }

    function currentFrame() {
      try { return !destroyed && frame && getCurrentFrame() === frame.contentWindow; }
      catch (_) { return false; }
    }

    function hasTrackMethod(name) {
      return !!(FrameTrack && FrameTrack.prototype && typeof FrameTrack.prototype[name] === 'function');
    }

    function hasCropSupport() {
      var browserTrack = win.BrowserCaptureMediaStreamTrack;
      return !!(Target && typeof Target.fromElement === 'function' && mediaDevices &&
        typeof mediaDevices.getDisplayMedia === 'function' &&
        typeof mediaDevices.setCaptureHandleConfig === 'function' &&
        hasTrackMethod('getCaptureHandle') &&
        (hasTrackMethod('cropTo') || browserTrack && browserTrack.prototype && typeof browserTrack.prototype.cropTo === 'function') &&
        Recorder && typeof Recorder.isTypeSupported === 'function' && Socket && fetcher && captureHandle);
    }

    var supported = hasCropSupport();
    if (supported) {
      try {
        captureHandleReady = Promise.resolve(mediaDevices.setCaptureHandleConfig({
          exposeOrigin: true,
          handle: captureHandle,
          permittedOrigins: [win.location.origin],
        })).then(function () { return true; }, function () { return false; });
      } catch (_) {
        captureHandleReady = Promise.resolve(false);
      }
    }

    function createDialog() {
      if (!doc || !doc.createElement || !doc.body) return;
      dialog = doc.createElement('dialog');
      dialog.className = 'vcast-dialog';
      dialog.setAttribute('aria-labelledby', 'vcast-confirm-title');
      dialog.setAttribute('aria-describedby', 'vcast-confirm-copy vcast-confirm-status');
      var panel = doc.createElement('div');
      panel.className = 'vcast-dialog-panel';
      var kicker = doc.createElement('p');
      kicker.className = 'vcast-dialog-kicker';
      kicker.textContent = 'Private tab capture';
      var title = doc.createElement('h2');
      title.id = 'vcast-confirm-title';
      title.textContent = 'Send this sleep scene to your Roku?';
      var copy = doc.createElement('p');
      copy.id = 'vcast-confirm-copy';
      copy.textContent = 'The viewer captures only the sleep scene inside this page. The browser asks you to confirm the tab. The rest of the page and other tabs are not sent.';
      dialogStatus = doc.createElement('p');
      dialogStatus.id = 'vcast-confirm-status';
      dialogStatus.className = 'vcast-dialog-status';
      dialogStatus.setAttribute('role', 'status');
      dialogStatus.setAttribute('aria-live', 'polite');
      var actions = doc.createElement('div');
      actions.className = 'vcast-dialog-actions';
      cancelButton = doc.createElement('button');
      cancelButton.type = 'button';
      cancelButton.className = 'vcast-dialog-cancel';
      cancelButton.textContent = 'Cancel';
      startButton = doc.createElement('button');
      startButton.type = 'button';
      startButton.className = 'vcast-dialog-start';
      startButton.textContent = 'Choose this tab and share';
      actions.append(cancelButton, startButton);
      panel.append(kicker, title, copy, dialogStatus, actions);
      dialog.append(panel);
      doc.body.append(dialog);
      cancelButton.addEventListener('click', cancelCaptureRequest);
      startButton.addEventListener('click', beginCaptureFromTrustedClick);
      dialog.addEventListener('cancel', function (event) {
        event.preventDefault();
        cancelCaptureRequest();
      });
    }

    createDialog();

    function envelope(type, fields) {
      var result = { type: type, v: 1, channel: channel, artifactId: artifactId, revision: revision };
      if (fields) Object.keys(fields).forEach(function (key) { result[key] = fields[key]; });
      return result;
    }

    function sendChild(type, fields) {
      if (!ready || revoked || !currentFrame()) return false;
      try { post(envelope(type, fields)); return true; }
      catch (_) { return false; }
    }

    function setCaptureActive(active) {
      active = active === true;
      if (captureActive === active) return;
      captureActive = active;
      if (typeof options.onCaptureActive === 'function') {
        try { options.onCaptureActive(active); } catch (_) {}
      }
    }

    function detailText(value, fallback) {
      if (typeof value !== 'string') return fallback;
      return value.replace(/[\u0000-\u001f\u007f]/g, ' ').trim().slice(0, 180) || fallback;
    }

    function state(name, detail) {
      sendChild('cast:state', { state: name, detail: detailText(detail, 'Casting status updated.') });
      if (dialogStatus && dialog && dialog.open) dialogStatus.textContent = detailText(detail, 'Casting status updated.');
    }

    function hostInit() {
      if (!channel || destroyed || revoked || !currentFrame()) return;
      var currentChannel = channel;
      captureHandleReady.then(function (handleReady) {
        if (channel !== currentChannel || destroyed || !currentFrame()) return;
        var enabled = serverEnabled && authenticated && stateEnabled && supported && handleReady;
        try {
          post({
            type: 'cast:host-init', v: 1, channel: channel,
            artifactId: artifactId, revision: revision,
            enabled: !!enabled, supported: !!(supported && handleReady),
          });
        } catch (_) {}
      });
    }

    function validHello(data) {
      return !!data && !Array.isArray(data) && typeof data === 'object' &&
        data.type === 'cast:hello' && data.v === 1;
    }

    function validEnvelope(data) {
      return !!data && !Array.isArray(data) && typeof data === 'object' &&
        data.v === 1 && data.channel === channel &&
        data.artifactId === artifactId && data.revision === revision;
    }

    function requestHeaders(current) {
      var headers = { 'content-type': 'application/json' };
      if (current && current.publishToken) headers.Authorization = 'Bearer ' + current.publishToken;
      return headers;
    }

    async function request(path, init, current) {
      if (!fetcher || typeof path !== 'string' || path.indexOf('/cast/') !== 0) throw new Error('cast_unavailable');
      var response = await fetcher(path, Object.assign({
        credentials: 'same-origin', cache: 'no-store',
        signal: AbortSignalApi && AbortSignalApi.timeout ? AbortSignalApi.timeout(10000) : undefined,
      }, init || {}, { headers: Object.assign(requestHeaders(current), init && init.headers || {}) }));
      var body = await response.json().catch(function () { return {}; });
      if (!response.ok) {
        var failure = new Error(response.status === 429 ? 'too_many_requests' : response.status === 401 ? 'access_required' : response.status === 404 ? 'pairing_unavailable' : 'cast_unavailable');
        failure.status = response.status;
        throw failure;
      }
      return body;
    }

    function stopLocalMedia() {
      uploadGeneration++;
      if (pollTimer !== null) { clearIntervalFn(pollTimer); pollTimer = null; }
      var currentRecorder = recorder;
      recorder = null;
      if (currentRecorder) {
        currentRecorder.ondataavailable = null;
        currentRecorder.onerror = null;
        if (currentRecorder.state && currentRecorder.state !== 'inactive') {
          try { currentRecorder.stop(); } catch (_) {}
        }
      }
      var currentSocket = socket;
      socket = null;
      if (currentSocket) {
        currentSocket.onclose = null;
        currentSocket.onerror = null;
        try { currentSocket.close(1000, 'Cast stopped'); } catch (_) {}
      }
      var currentStream = stream;
      stream = null;
      videoTrack = null;
      if (currentStream && currentStream.getTracks) {
        currentStream.getTracks().forEach(function (track) { try { track.stop(); } catch (_) {} });
      }
      setCaptureActive(false);
    }

    function closeDialog() {
      pendingStart = null;
      if (startButton) startButton.disabled = false;
      if (cancelButton) cancelButton.disabled = false;
      if (dialog && dialog.open && dialog.close) { try { dialog.close(); } catch (_) {} }
    }

    async function stopSession(detail, acknowledge) {
      operation++;
      stopping = true;
      pairing = false;
      closeDialog();
      stopLocalMedia();
      var current = session;
      session = null;
      if (acknowledge) {
        state('ended', detailText(detail, 'Casting stopped.'));
        sendChild('cast:stopped', { detail: detailText(detail, 'Casting stopped.') });
      }
      if (current && current.sessionId && current.publishToken) {
        try {
          await request('/cast/api/session/' + encodeURIComponent(current.sessionId) + '/stop', { method: 'POST' }, current);
        } catch (_) {}
      }
      stopping = false;
    }

    async function pair(code) {
      if (ready && stopping) { state('error', 'Finishing the previous cast. Wait a moment, then reconnect.'); return; }
      if (!ready || !serverEnabled || !authenticated || !stateEnabled || !supported || pairing || session || stopping) return;
      if (typeof code !== 'string' || !/^\d{6}$/.test(code)) { state('error', 'Enter the six-digit code shown on the Roku.'); return; }
      pairing = true;
      var requestOperation = ++operation;
      state('pairing', 'Connecting to the Roku…');
      try {
        var body = await request('/cast/api/pair', { method: 'POST', body: JSON.stringify({ code: code }) }, null);
        var received = {
          sessionId: typeof body.sessionId === 'string' ? body.sessionId : '',
          publishToken: typeof body.publishToken === 'string' ? body.publishToken : '',
        };
        if (!/^[A-Za-z0-9_-]{16,180}$/.test(received.sessionId) || !/^[A-Za-z0-9_-]{24,180}$/.test(received.publishToken)) throw new Error('invalid_pair_response');
        if (requestOperation !== operation || !ready || !currentFrame()) {
          try { await request('/cast/api/session/' + encodeURIComponent(received.sessionId) + '/stop', { method: 'POST' }, received); } catch (_) {}
          return;
        }
        session = received;
        state('paired', 'Roku paired. Start the sleep scene when you are ready.');
        pollStatus();
        pollTimer = setIntervalFn(pollStatus, 3000);
      } catch (error) {
        if (requestOperation === operation) state('error', error.message === 'too_many_requests' ? 'Too many attempts. Wait a minute and try again.' : error.message === 'access_required' ? 'Sign in to the viewer and try again.' : 'That Roku code is unavailable. Check it and try again.');
      } finally {
        if (requestOperation === operation) pairing = false;
      }
    }

    function isCropTarget(value) {
      try { return !!(Target && value instanceof Target); }
      catch (_) { return false; }
    }

    function beginStart(data) {
      if (!session || stopping || pendingStart || !supported) { state('error', 'Pair the Roku before starting a cast.'); return; }
      if (stream || socket || recorder) { state('live', 'This scene is already casting. Stop it before starting another cast.'); return; }
      if (typeof data.expectsAudio !== 'boolean' || !isCropTarget(data.cropTarget)) { state('error', 'The sleep scene is not ready for private capture.'); return; }
      pendingStart = { expectsAudio: data.expectsAudio, cropTarget: data.cropTarget };
      if (!dialog || typeof dialog.showModal !== 'function') { pendingStart = null; state('error', 'This browser cannot open the secure capture confirmation.'); return; }
      dialogStatus.textContent = 'Confirm to choose this browser tab.';
      try { dialog.showModal(); startButton.focus(); }
      catch (_) { pendingStart = null; state('error', 'The capture confirmation could not be opened.'); }
      state('confirming', 'Confirm sharing to choose this browser tab.');
    }

    function cancelCaptureRequest() {
      if (!pendingStart) return;
      uploadGeneration++;
      stopLocalMedia();
      closeDialog();
      state('paired', 'Cast cancelled. Your Roku is still paired; you can try again.');
    }

    function beginCaptureFromTrustedClick(event) {
      if (!pendingStart || stopping || !session) return;
      var activation = nav && nav.userActivation;
      if (!event || event.isTrusted !== true || activation && activation.isActive !== true) {
        state('confirming', 'Use the Share button in this dialog to open the browser confirmation.');
        return;
      }
      if (startButton) startButton.disabled = true;
      if (cancelButton) cancelButton.disabled = true;
      var requested = pendingStart;
      var capturePromise;
      setCaptureActive(true);
      try {
        // Keep this call in the trusted viewer button's click stack.
        capturePromise = mediaDevices.getDisplayMedia({
          video: { displaySurface: 'browser', width: { ideal: 1280, max: 1280 }, height: { ideal: 720, max: 720 }, frameRate: { ideal: 15, max: 15 } },
          audio: requested.expectsAudio ? { restrictOwnAudio: false, suppressLocalAudioPlayback: true, echoCancellation: false, noiseSuppression: false, autoGainControl: false } : false,
          preferCurrentTab: true,
          selfBrowserSurface: 'include',
          monitorTypeSurfaces: 'exclude',
          surfaceSwitching: 'exclude',
          systemAudio: 'exclude',
        });
      } catch (error) {
        captureFailed(error, requested);
        return;
      }
      runCapture(capturePromise, requested);
    }

    async function runCapture(capturePromise, requested) {
      var run = ++uploadGeneration;
      try {
        var nextStream = await capturePromise;
        if (run !== uploadGeneration || !session || stopping) { nextStream.getTracks().forEach(function (track) { track.stop(); }); return; }
        stream = nextStream;
        var tracks = nextStream.getVideoTracks();
        var track = tracks && tracks[0];
        if (!track || !track.getSettings || track.getSettings().displaySurface !== 'browser') throw new Error('Choose the current viewer tab in Chrome.');
        if (typeof track.getCaptureHandle !== 'function' || typeof track.cropTo !== 'function') throw new Error('This browser cannot safely isolate the sleep scene. Update Chrome and try again.');
        var configured = await captureHandleReady;
        var handle = track.getCaptureHandle();
        if (!configured || !handle || handle.handle !== captureHandle || handle.origin !== win.location.origin) throw new Error('Choose this Artifact viewer tab to continue.');
        if (!isCropTarget(requested.cropTarget)) throw new Error('The sleep scene changed. Start the cast again.');
        await track.cropTo(requested.cropTarget);
        if (run !== uploadGeneration || !session || stopping) throw new Error('cast_stopped');
        var croppedHandle = track.getCaptureHandle();
        if (!croppedHandle || croppedHandle.handle !== captureHandle || croppedHandle.origin !== win.location.origin) throw new Error('The captured tab changed. Choose this Artifact viewer tab again.');
        var audioTracks = nextStream.getAudioTracks ? nextStream.getAudioTracks() : [];
        if (requested.expectsAudio && (!audioTracks || !audioTracks.length)) throw new Error('Allow this tab to share its sound, then try again.');
        if (!requested.expectsAudio && audioTracks && audioTracks.length) throw new Error('Unexpected tab audio. Choose video-only sharing for this silent routine.');
        if (audioTracks && audioTracks.some(function (audioTrack) { return audioTrack.getSettings && audioTrack.getSettings().restrictOwnAudio === true; })) throw new Error('Chrome restricted the sleep sound. Choose this viewer tab and allow its audio.');
        videoTrack = track;
        track.addEventListener('capturehandlechange', function () {
          if (run !== uploadGeneration) return;
          var activeHandle = null;
          try { activeHandle = track.getCaptureHandle(); } catch (_) {}
          if (!activeHandle || activeHandle.handle !== captureHandle || activeHandle.origin !== win.location.origin) {
            stopSession('The captured tab changed. Pair the Roku again to retry.', true);
          }
        });
        state('connecting', 'The sleep scene is isolated. Connecting to the Roku…');
        var current = session;
        var connection = await connectUpload(current, requested.expectsAudio);
        if (run !== uploadGeneration || session !== current || stopping) throw new Error('cast_stopped');
        socket = connection;
        var mimeType = ['video/webm;codecs=vp8,opus', 'video/webm;codecs=vp9,opus', 'video/webm'].find(function (type) { return Recorder.isTypeSupported(type); });
        if (!mimeType) throw new Error('Chrome cannot encode this stream. Update Chrome and try again.');
        var nextRecorder = new Recorder(nextStream, { mimeType: mimeType, videoBitsPerSecond: 2000000, audioBitsPerSecond: requested.expectsAudio ? 128000 : 1 });
        recorder = nextRecorder;
        var pending = Promise.resolve();
        var pendingChunks = 0;
        var pendingBytes = 0;
        nextRecorder.ondataavailable = function (event) {
          if (!event.data || !event.data.size) return;
          if (event.data.size > 2 * 1024 * 1024 || pendingChunks >= 8 || pendingBytes + event.data.size > 8 * 1024 * 1024) {
            state('error', 'The connection is too slow. Casting stopped.');
            stopSession('The connection is too slow. Casting stopped.', true);
            return;
          }
          pendingChunks++;
          pendingBytes += event.data.size;
          pending = pending.then(async function () {
            try {
              if (run !== uploadGeneration || connection.readyState !== Socket.OPEN) return;
              if (connection.bufferedAmount > 4 * 1024 * 1024) throw new Error('backpressure');
              var bytes = await event.data.arrayBuffer();
              if (run === uploadGeneration && connection.readyState === Socket.OPEN) connection.send(bytes);
            } catch (_) {
              state('error', 'The connection is too slow. Casting stopped.');
              stopSession('The connection is too slow. Casting stopped.', true);
            } finally {
              pendingChunks--;
              pendingBytes -= event.data.size;
            }
          });
        };
        nextRecorder.onerror = function () { stopSession('The browser stopped recording. Pair the Roku again to retry.', true); };
        connection.onclose = function () { if (run === uploadGeneration && !stopping) stopSession('The stream disconnected. Pair the Roku again to retry.', true); };
        connection.onmessage = function (message) {
          try { var body = JSON.parse(message.data); if (body && typeof body.error === 'string') { state('error', detailText(body.error, 'The stream stopped.')); stopSession('The stream stopped.', true); } }
          catch (_) {}
        };
        track.addEventListener('ended', function () { if (run === uploadGeneration) stopSession('Browser sharing ended.', true); }, { once: true });
        closeDialog();
        nextRecorder.start(1000);
        sendChild('cast:enter-tv');
        state('buffering', 'Building the Roku video buffer. Keep this viewer open.');
      } catch (error) {
        if (stream !== null && stream !== undefined) {
          var failedStream = stream;
          stream = null;
          if (failedStream.getTracks) failedStream.getTracks().forEach(function (track) { try { track.stop(); } catch (_) {} });
        }
        if (typeof nextStream !== 'undefined' && nextStream && nextStream.getTracks) nextStream.getTracks().forEach(function (track) { try { track.stop(); } catch (_) {} });
        if (socket) { try { socket.close(); } catch (_) {} socket = null; }
        if (recorder && recorder.state !== 'inactive') { try { recorder.stop(); } catch (_) {} }
        recorder = null;
        closeDialog();
        setCaptureActive(false);
        if (error && error.name === 'NotAllowedError') {
          state('paired', 'Sharing was cancelled. Your Roku stays paired; you can try again.');
          return;
        }
        if (error && error.message === 'cast_stopped') return;
        if (session) state('paired', detailText(error && error.message, 'The cast could not start. Your Roku stays paired; try again.'));
      }
    }

    function captureFailed(error) {
      closeDialog();
      setCaptureActive(false);
      if (error && error.name === 'NotAllowedError') state('paired', 'Sharing was cancelled. Your Roku stays paired; you can try again.');
      else state('paired', 'The cast could not start. Your Roku stays paired; try again.');
    }

    function connectUpload(current, hasAudio) {
      return new Promise(function (resolve, reject) {
        if (!current || !current.sessionId || !current.publishToken) { reject(new Error('cast_unavailable')); return; }
        var url = new URL('/cast/api/session/' + encodeURIComponent(current.sessionId) + '/upload', win.location.href);
        url.protocol = win.location.protocol === 'https:' ? 'wss:' : 'ws:';
        var connection;
        try { connection = new Socket(url.href); }
        catch (error) { reject(error); return; }
        socket = connection;
        var settled = false;
        var timeout = win.setTimeout(function () { finish(new Error('cast_timeout')); }, 10000);
        function finish(error) {
          if (settled) return;
          settled = true;
          win.clearTimeout(timeout);
          if (error) { if (socket === connection) socket = null; try { connection.close(); } catch (_) {} reject(error); }
          else resolve(connection);
        }
        connection.onopen = function () {
          try { connection.send(JSON.stringify({ token: current.publishToken, hasAudio: hasAudio })); }
          catch (error) { finish(error); }
        };
        connection.onmessage = function (event) {
          try { var response = JSON.parse(event.data); if (response && response.error) finish(new Error('cast_unavailable')); else if (response && response.accepted === true) finish(); }
          catch (_) { finish(new Error('cast_unavailable')); }
        };
        connection.onerror = function () { finish(new Error('cast_unavailable')); };
        connection.onclose = function () { finish(new Error('cast_unavailable')); };
      });
    }

    async function pollStatus() {
      var current = session;
      if (!current || stopping || !ready) return;
      try {
        var body = await request('/cast/api/session/' + encodeURIComponent(current.sessionId), { method: 'GET' }, current);
        if (session !== current || !ready) return;
        var stateName = typeof body.state === 'string' ? body.state : '';
        var message = stateName === 'buffering' ? 'Building the Roku video buffer.' : stateName === 'live' ? (body.receiverConnected ? 'The sleep scene is streaming to the Roku.' : 'The stream is ready. Open Sleep cast on the Roku.') : stateName === 'paired' ? 'Roku paired. Start the sleep scene when ready.' : '';
        if (stateName === 'ended' || stateName === 'expired') { await stopSession('The Roku cast ended.', true); return; }
        if (stateName === 'paired') { if (!pendingStart) state(stateName, message); }
        else if (stateName === 'buffering' || stateName === 'live') state(stateName, message);
        else { state('error', 'The cast status could not be read.'); await stopSession('The cast status could not be read.', true); }
      } catch (_) {
        state('error', 'The cast connection was lost. Reconnect to try again.');
        await stopSession('The cast connection was lost.', true);
      }
    }

    function stopMessage() {
      stopSession('Casting stopped. The sleep routine can keep running.', true);
    }

    function handle(event) {
      if (!event || revoked || !currentFrame() || event.source !== getCurrentFrame()) return false;
      var data = event.data;
      if (validHello(data)) { if (!initialLoadSeen) initialLoadSeen = true; hostInit(); return true; }
      if (!validEnvelope(data) || typeof data.type !== 'string') return false;
      if (data.type === 'cast:ready') {
        ready = true;
        everReady = true;
        state(serverEnabled && authenticated && stateEnabled && supported ? 'ready' : 'unsupported', supported ? 'Open Sleep cast on your Roku, then pair it here.' : 'This browser does not support private scene capture.');
        return true;
      }
      if (!ready) return false;
      if (data.type === 'cast:pair') { pair(data.code); return true; }
      if (data.type === 'cast:start') { beginStart(data); return true; }
      if (data.type === 'cast:stop') { stopMessage(); return true; }
      if (data.type === 'cast:download') {
        var anchor = doc.createElement('a');
        anchor.href = '/cast/receiver.zip';
        anchor.download = 'roku-cast-receiver.zip';
        anchor.rel = 'noreferrer';
        anchor.hidden = true;
        doc.body.appendChild(anchor);
        anchor.click();
        anchor.remove();
        return true;
      }
      return false;
    }

    function onFrameLoad() {
      if (initialLoadSeen || everReady) {
        revoked = true;
        ready = false;
        channel = '';
        pairing = false;
        operation++;
        closeDialog();
        stopSession('The viewer changed. Reload the artifact to cast again.', false);
        return;
      }
      initialLoadSeen = true;
      operation++;
      ready = false;
      var nextChannel = randomToken();
      channel = nextChannel;
      pairing = false;
      stopping = false;
      closeDialog();
      stopLocalMedia();
      var previous = session;
      session = null;
      if (previous && previous.sessionId && previous.publishToken) {
        request('/cast/api/session/' + encodeURIComponent(previous.sessionId) + '/stop', { method: 'POST' }, previous).catch(function () {});
      }
      hostInit();
    }

    function destroy() {
      if (destroyed) return;
      destroyed = true;
      operation++;
      ready = false;
      closeDialog();
      stopLocalMedia();
      var previous = session;
      session = null;
      if (previous && previous.sessionId && previous.publishToken) {
        request('/cast/api/session/' + encodeURIComponent(previous.sessionId) + '/stop', { method: 'POST' }, previous).catch(function () {});
      }
      if (dialog && dialog.remove) dialog.remove();
    }

    return {
      handle: handle,
      onFrameLoad: onFrameLoad,
      destroy: destroy,
      getReady: function () { return ready; },
      getSupported: function () { return supported; },
    };
  }

  root.createArtifactCastBridge = createArtifactCastBridge;
})(window);
