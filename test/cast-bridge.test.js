import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = fs.readFileSync(new URL('../assets/cast.js', import.meta.url), 'utf8');
const shellSource = fs.readFileSync(new URL('../assets/shell.js', import.meta.url), 'utf8');

function harness(options = {}) {
  const posted = [];
  const appended = [];
  const listeners = new Map();
  const created = [];
  function element(tag) {
    const result = {
      tagName: tag,
      dataset: {},
      children: [],
      classList: { add() {}, remove() {} },
      setAttribute() {},
      append(...items) { this.children.push(...items); },
      appendChild(item) { this.children.push(item); appended.push(item); },
      addEventListener(name, fn) { listeners.set(`${this.className || tag}:${name}`, fn); },
      remove() { this.removed = true; },
      click() { this.clicked = true; },
      focus() {},
      close() { this.open = false; },
      showModal() { this.open = true; },
    };
    created.push(result);
    return result;
  }
  const doc = { body: element('body'), createElement: element };
  const frame = { contentWindow: { postMessage(message) { posted.push(message); } } };
  const win = {
    document: doc,
    navigator: {},
    location: { origin: 'https://artifact.neilblackman.dev', href: 'https://artifact.neilblackman.dev/abc', protocol: 'https:' },
    crypto: { getRandomValues(bytes) { bytes.fill(7); } },
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
    addEventListener() {},
  };
  vm.runInNewContext(source, { window: win, AbortSignal, URL });
  const bridge = win.createArtifactCastBridge({
    window: win,
    document: doc,
    frame,
    getCurrentFrame: () => frame.contentWindow,
    post: (message) => posted.push(message),
    artifactId: 'abc123',
    revision: 4,
    serverEnabled: true,
    authenticated: true,
    stateEnabled: true,
    ...(options.bridgeOptions || {}),
  });
  return { bridge, frame, posted, appended, listeners, created, win, doc };
}

function supportedHarness({ audio = true, matchingHandle = true, cropBehavior = null, stopGate = null } = {}) {
  const timeline = [];
  const captureSignals = [];
  const requests = [];
  let captureCalls = 0;
  let socketInstance = null;
  let recorderInstance = null;
  let pollCallback = null;
  let currentHandle = '';
  let handleMatches = matchingHandle;
  class CropTarget { static fromElement() { return new CropTarget(); } }
  class Track {
    getCaptureHandle() { return { handle: handleMatches ? currentHandle : 'other', origin: 'https://artifact.neilblackman.dev' }; }
    cropTo(target) { timeline.push('crop'); assert.ok(target instanceof CropTarget); return cropBehavior ? cropBehavior() : Promise.resolve(); }
    getSettings() { return { displaySurface: 'browser', restrictOwnAudio: false }; }
    addEventListener(name, callback) { this.handlers = this.handlers || {}; this.handlers[name] = callback; }
    stop() { this.stopped = true; }
  }
  const video = new Track();
  const sound = { getSettings() { return { restrictOwnAudio: false }; }, stop() { this.stopped = true; } };
  const stream = {
    getVideoTracks() { return [video]; },
    getAudioTracks() { return audio ? [sound] : []; },
    getTracks() { return audio ? [video, sound] : [video]; },
  };
  class Recorder {
    static isTypeSupported() { return true; }
    constructor(input) { assert.equal(input, stream); recorderInstance = this; this.state = 'inactive'; }
    start(ms) { this.interval = ms; this.state = 'recording'; timeline.push('record'); }
    stop() { this.state = 'inactive'; this.stopped = true; }
  }
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 0; this.bufferedAmount = 0; socketInstance = this; timeline.push('socket'); queueMicrotask(() => { this.readyState = Socket.OPEN; this.onopen(); }); }
    send(data) { this.sent = data; if (typeof data === 'string') queueMicrotask(() => this.onmessage({ data: JSON.stringify({ accepted: true }) })); }
    close() { this.readyState = 3; this.closed = true; if (this.onclose) this.onclose(); }
  }
  const mediaDevices = {
    setCaptureHandleConfig(config) { currentHandle = config.handle; return Promise.resolve(); },
    getDisplayMedia(constraints) { captureCalls++; timeline.push('capture'); this.constraints = constraints; assert.equal(timeline[timeline.length - 2], 'capture-lock'); return Promise.resolve(stream); },
  };
  const mockFetch = async (path, init = {}) => {
    requests.push({ path, init });
    if (path === '/cast/api/pair') return { ok: true, json: async () => ({ sessionId: 'session_abcdefghijklmnop', publishToken: 'token_abcdefghijklmnopqrs' }) };
    if (path.includes('/stop')) { if (stopGate) await stopGate(); return { ok: true, json: async () => ({ stopped: true }) }; }
    return { ok: true, json: async () => ({ state: 'paired' }) };
  };
  const env = harness({ bridgeOptions: { CropTarget, MediaStreamTrack: Track, MediaRecorder: Recorder, WebSocket: Socket, mediaDevices, fetch: mockFetch, navigator: { mediaDevices, userActivation: { isActive: true } }, onCaptureActive(active) { captureSignals.push(active); if (active) timeline.push('capture-lock'); }, setInterval: (callback) => { pollCallback = callback; return 1; }, clearInterval: () => {} } });
  return { ...env, timeline, requests, captureSignals, poll() { if (pollCallback) pollCallback(); }, changeHandle() { handleMatches = false; if (video.handlers && video.handlers.capturehandlechange) video.handlers.capturehandlechange(); }, get captureCalls() { return captureCalls; }, get socket() { return socketInstance; }, get recorder() { return recorderInstance; }, mediaDevices, stream, video, sound, CropTarget };
}

async function readyAndPair(env) {
  env.bridge.onFrameLoad();
  await new Promise((resolve) => setImmediate(resolve));
  const channel = env.posted[0].channel;
  const base = { v: 1, channel, artifactId: 'abc123', revision: 4 };
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:ready' } });
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:pair', code: '123456' } });
  await new Promise((resolve) => setImmediate(resolve));
  return { channel, base };
}

const tick = () => new Promise((resolve) => setImmediate(resolve));

test('current digest on the iframe URL does not disable cast; outer revision views do', () => {
  assert.match(shellSource, /var outerRevisionQuery=new URLSearchParams\(window\.location\.search\)/);
  assert.match(shellSource, /var historicalView=outerRevisionQuery\.has\('v'\)\|\|outerRevisionQuery\.has\('revision'\)/);
  assert.match(shellSource, /shellConfig\.castEnabled==='1'&&!historicalView&&!isBundle/);
  assert.doesNotMatch(shellSource, /shellConfig\.castEnabled==='1'&&!versionQuery/);
});

test('accepts hello only from the current frame and sends the required host envelope', async () => {
  const { bridge, frame, posted } = harness();
  assert.equal(bridge.handle({ source: {}, data: { type: 'cast:hello', v: 1 } }), false);
  assert.equal(bridge.handle({ source: frame.contentWindow, data: { type: 'cast:hello', v: 1 } }), true);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(posted.length, 1);
  assert.equal(posted[0].type, 'cast:host-init');
  assert.equal(posted[0].artifactId, 'abc123');
  assert.equal(posted[0].revision, 4);
  assert.equal(typeof posted[0].channel, 'string');
  assert.equal(posted[0].enabled, false);
});

test('ready requires the current nonce, and download uses only the fixed receiver path', async () => {
  const { bridge, frame, posted, appended } = harness();
  bridge.onFrameLoad();
  await new Promise((resolve) => setImmediate(resolve));
  const channel = posted[0].channel;
  const envelope = { v: 1, channel, artifactId: 'abc123', revision: 4 };
  assert.equal(bridge.handle({ source: frame.contentWindow, data: { ...envelope, type: 'cast:ready', channel: 'forged' } }), false);
  assert.equal(bridge.handle({ source: frame.contentWindow, data: { ...envelope, type: 'cast:ready' } }), true);
  assert.equal(bridge.handle({ source: frame.contentWindow, data: { ...envelope, type: 'cast:download', url: 'https://evil.example/steal.zip' } }), true);
  const anchor = appended.at(-1);
  assert.equal(anchor.href, '/cast/receiver.zip');
  assert.equal(anchor.download, 'roku-cast-receiver.zip');
  assert.equal(anchor.clicked, true);
  assert.equal(anchor.removed, true);
});

test('ignores messages from stale frames and does not expose credentials or session metadata', async () => {
  const { bridge, frame, posted } = harness();
  bridge.onFrameLoad();
  await new Promise((resolve) => setImmediate(resolve));
  const channel = posted[0].channel;
  const envelope = { v: 1, channel, artifactId: 'abc123', revision: 4 };
  assert.equal(bridge.handle({ source: {}, data: { ...envelope, type: 'cast:ready' } }), false);
  bridge.handle({ source: frame.contentWindow, data: { ...envelope, type: 'cast:ready' } });
  assert.equal(bridge.handle({ source: frame.contentWindow, data: { ...envelope, type: 'cast:pair', code: '12345' } }), true);
  await new Promise((resolve) => setImmediate(resolve));
  for (const message of posted) {
    assert.equal(Object.hasOwn(message, 'publishToken'), false);
    assert.equal(Object.hasOwn(message, 'sessionId'), false);
  }
});

test('calls browser capture only from a trusted viewer click and sends no upload before the scene is cropped', async () => {
  const env = supportedHarness();
  const { base } = await readyAndPair(env);
  assert.deepEqual(JSON.parse(env.requests.find((item) => item.path === '/cast/api/pair').init.body), { code: '123456' });
  const target = new env.CropTarget();
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: true, cropTarget: target } });
  await tick();
  const pairedUpdates = env.posted.filter((message) => message.type === 'cast:state' && message.state === 'paired').length;
  env.poll();
  await tick();
  assert.equal(env.posted.filter((message) => message.type === 'cast:state' && message.state === 'paired').length, pairedUpdates);
  const start = env.created.find((item) => item.className === 'vcast-dialog-start');
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: false });
  assert.equal(env.captureCalls, 0);
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick(); await tick();
  assert.deepEqual(env.timeline.slice(0, 5), ['capture-lock', 'capture', 'crop', 'socket', 'record'], JSON.stringify(env.posted.filter((message) => message.type === 'cast:state')));
  assert.equal(env.mediaDevices.constraints.video.displaySurface, 'browser');
  assert.equal(env.mediaDevices.constraints.audio.restrictOwnAudio, false);
  assert.equal(env.socket.url, 'wss://artifact.neilblackman.dev/cast/api/session/session_abcdefghijklmnop/upload');
  assert.equal(env.posted.some((message) => message.type === 'cast:enter-tv'), true);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: true, cropTarget: target } });
  assert.equal(env.captureCalls, 1);
  assert.ok(start);
});

test('rejects a capture handle for another tab and rejects missing audio when the routine expects sound', async () => {
  const wrongTab = supportedHarness({ matchingHandle: false });
  let { base } = await readyAndPair(wrongTab);
  wrongTab.bridge.handle({ source: wrongTab.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: true, cropTarget: new wrongTab.CropTarget() } });
  wrongTab.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick();
  assert.equal(wrongTab.timeline.includes('socket'), false);
  assert.equal(wrongTab.video.stopped, true);
  assert.deepEqual(wrongTab.captureSignals, [true, false]);

  const noAudio = supportedHarness({ audio: false });
  ({ base } = await readyAndPair(noAudio));
  noAudio.bridge.handle({ source: noAudio.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: true, cropTarget: new noAudio.CropTarget() } });
  noAudio.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick();
  assert.equal(noAudio.timeline.includes('socket'), false);
  assert.equal(noAudio.video.stopped, true);
  assert.deepEqual(noAudio.captureSignals, [true, false]);
  assert.ok(noAudio.posted.some((message) => message.type === 'cast:state' && message.state === 'paired'));
});

test('video-only silent capture can upload and stop cleans media and server state', async () => {
  const env = supportedHarness({ audio: false });
  const { base } = await readyAndPair(env);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: false, cropTarget: new env.CropTarget() } });
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick(); await tick();
  assert.equal(env.mediaDevices.constraints.audio, false);
  assert.equal(env.timeline.includes('record'), true, JSON.stringify(env.posted.filter((message) => message.type === 'cast:state')));
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:stop' } });
  await tick(); await tick();
  assert.equal(env.video.stopped, true);
  assert.equal(env.recorder.stopped, true);
  assert.equal(env.socket.closed, true);
  assert.ok(env.requests.some((item) => item.path.endsWith('/stop')));
  assert.ok(env.posted.some((message) => message.type === 'cast:stopped'));
  assert.deepEqual(env.captureSignals, [true, false]);
});

test('pair during server cleanup returns a retryable status instead of staying in pairing', async () => {
  let finishStop;
  const env = supportedHarness({ audio: false, stopGate: () => new Promise((resolve) => { finishStop = resolve; }) });
  const { base } = await readyAndPair(env);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:stop' } });
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:pair', code: '123456' } });
  const status = env.posted.filter((message) => message.type === 'cast:state').at(-1);
  assert.equal(status.state, 'error');
  assert.equal(status.detail, 'Finishing the previous cast. Wait a moment, then reconnect.');
  assert.equal(env.requests.filter((item) => item.path === '/cast/api/pair').length, 1);
  finishStop();
  await tick();
});

test('stop during scene cropping acknowledges immediately and prevents a late upload', async () => {
  let finishCrop;
  const env = supportedHarness({ cropBehavior: () => new Promise((resolve) => { finishCrop = resolve; }) });
  const { base } = await readyAndPair(env);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: true, cropTarget: new env.CropTarget() } });
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick();
  assert.deepEqual(env.timeline, ['capture-lock', 'capture', 'crop']);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:stop' } });
  assert.ok(env.posted.some((message) => message.type === 'cast:stopped'));
  assert.deepEqual(env.captureSignals, [true, false]);
  finishCrop();
  await tick(); await tick();
  assert.equal(env.timeline.includes('socket'), false);
  assert.equal(env.video.stopped, true);
});

test('capture handle changes stop the stream immediately', async () => {
  const env = supportedHarness({ audio: false });
  const { base } = await readyAndPair(env);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: false, cropTarget: new env.CropTarget() } });
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick(); await tick();
  assert.equal(env.socket.closed, undefined);
  env.changeHandle();
  await tick();
  assert.equal(env.socket.closed, true);
  assert.equal(env.video.stopped, true);
  assert.ok(env.posted.some((message) => message.type === 'cast:stopped'));
  assert.deepEqual(env.captureSignals, [true, false]);
});

test('a second iframe load permanently revokes the capture channel', async () => {
  const env = supportedHarness({ audio: false });
  const { base } = await readyAndPair(env);
  env.bridge.onFrameLoad();
  const previousMessages = env.posted.length;
  assert.equal(env.bridge.handle({ source: env.frame.contentWindow, data: { type: 'cast:hello', v: 1 } }), false);
  assert.equal(env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:pair', code: '123456' } }), false);
  await tick();
  assert.equal(env.posted.length, previousMessages);
  assert.ok(env.requests.some((item) => item.path.endsWith('/stop')));
});

test('the first observed iframe load revokes if the shell missed the initial load event', async () => {
  const env = supportedHarness({ audio: false });
  assert.equal(env.bridge.handle({ source: env.frame.contentWindow, data: { type: 'cast:hello', v: 1 } }), true);
  await tick();
  const channel = env.posted[0].channel;
  const base = { v: 1, channel, artifactId: 'abc123', revision: 4 };
  assert.equal(env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:ready' } }), true);
  const sent = env.posted.length;
  env.bridge.onFrameLoad();
  assert.equal(env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:pair', code: '123456' } }), false);
  assert.equal(env.posted.length, sent);
});

test('MediaRecorder packet queue stops at its eight MiB bound', async () => {
  const env = supportedHarness({ audio: false });
  const { base } = await readyAndPair(env);
  env.bridge.handle({ source: env.frame.contentWindow, data: { ...base, type: 'cast:start', expectsAudio: false, cropTarget: new env.CropTarget() } });
  env.listeners.get('vcast-dialog-start:click')({ isTrusted: true });
  await tick(); await tick(); await tick();
  let arrayBufferCalls = 0;
  const blob = { size: 1024 * 1024, arrayBuffer: () => { arrayBufferCalls++; return new Promise(() => {}); } };
  for (let index = 0; index < 9; index++) env.recorder.ondataavailable({ data: blob });
  await tick(); await tick();
  assert.equal(arrayBufferCalls, 0);
  assert.equal(env.socket.closed, true);
  assert.equal(env.video.stopped, true);
  assert.ok(env.posted.some((message) => message.type === 'cast:stopped'));
});
