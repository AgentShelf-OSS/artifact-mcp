  function createNativeReader() {
    if (!frame || !document.getElementById('vshare-toggle')) return;
    const box = document.createElement('section');
    box.className = 'vreader'; box.hidden = true;
    box.setAttribute('aria-label', 'Read aloud');
    box.innerHTML = '<button id="vreader-toggle" type="button" class="vreader-toggle" aria-expanded="false" aria-controls="vreader-mini vreader-panel">Listen</button><div id="vreader-panel" class="vreader-panel" hidden><div class="vreader-head"><div><span class="vreader-kicker">Audio reader</span><strong>Read aloud</strong></div><button id="vreader-minimize" type="button" class="vreader-control vreader-minimize" aria-label="Minimize audio player">Minimize</button><span id="vreader-status" role="status" aria-live="polite">Ready</span></div><div class="vreader-actions" role="group" aria-label="Playback controls"><button id="vreader-play" type="button" class="vreader-control vreader-play">Play</button><button id="vreader-prev" type="button" class="vreader-control vreader-skip" aria-label="Previous block">Previous</button><button id="vreader-next" type="button" class="vreader-control vreader-skip" aria-label="Next block">Next</button><button id="vreader-stop" type="button" class="vreader-control vreader-stop">Stop</button></div><div class="vreader-navigator"><label for="vreader-outline">Jump to</label><div class="vreader-navigator-row"><select id="vreader-outline" disabled><option value="">Loading sections…</option></select><button id="vreader-jump" type="button" class="vreader-control" aria-label="Start reading from selected section or chapter" disabled>Read</button></div><p class="vreader-pick-hint">Click text in the artifact to choose where to read.</p><div id="vreader-target" class="vreader-target" hidden><span id="vreader-target-label"></span><button id="vreader-target-read" type="button" class="vreader-control">Read from here</button><button id="vreader-target-details" type="button" class="vreader-control" hidden>Read details</button><button id="vreader-target-dismiss" type="button" class="vreader-control vreader-dismiss" aria-label="Dismiss reading suggestion">×</button></div></div><div class="vreader-settings"><label>Read <select id="vreader-mode"><option value="page">Document / chapter</option><option value="view">Current view</option><option value="detail" hidden>Selected details</option><option value="selection">Selection</option><option value="here">From here</option><option value="section">This section</option></select></label><label>Voice <select id="vreader-voice"></select></label><label>Speed <select id="vreader-rate"><option value="0.75">0.75×</option><option value="0.8">0.8×</option><option value="0.9">0.9×</option><option value="1" selected>1×</option><option value="1.1">1.1×</option><option value="1.25">1.25×</option><option value="1.5">1.5×</option><option value="1.75">1.75×</option><option value="2">2×</option><option value="2.5">2.5×</option><option value="3">3×</option></select></label></div><div id="vreader-finder" class="vreader-finder" hidden><input id="vreader-voice-search" type="search" placeholder="Find a voice" aria-label="Find a voice" autocomplete="off"><div class="vreader-chips" role="group" aria-label="Filter voices"><button type="button" data-voice-filter="all" aria-pressed="true">All</button><button type="button" data-voice-filter="female" aria-pressed="false">Female</button><button type="button" data-voice-filter="male" aria-pressed="false">Male</button><button type="button" data-voice-filter="saved" aria-pressed="false">Saved</button></div><select id="vreader-voice-accent" aria-label="Accent" hidden><option value="">Any accent</option></select><button type="button" id="vreader-voice-save" class="vreader-save" aria-pressed="false">Save this voice</button><p id="vreader-voice-count" class="vreader-voice-count" aria-live="polite"></p></div><div id="vreader-style-panel" class="vreader-style-panel" hidden><div class="vreader-style-row"><label for="vreader-style">Reading style<select id="vreader-style"><option value="calm">Calm audiobook</option><option value="neutral">Neutral</option><option value="expressive">Expressive</option><option value="default">Model default</option><option value="custom">Custom instructions</option></select></label></div><label id="vreader-custom-label" class="vreader-custom" hidden>Delivery instructions<textarea id="vreader-custom" rows="3" maxlength="500" placeholder="Describe the pacing, tone, and emphasis you want."></textarea></label><p id="vreader-style-hint" class="vreader-style-hint">Steady pacing, restrained emotion, gentle emphasis.</p></div><div class="vreader-utility-row"><button id="vreader-rewind" type="button" class="vreader-control" aria-label="Rewind 15 seconds">↶ 15 seconds</button><button id="vreader-forward" type="button" class="vreader-control" aria-label="Forward 30 seconds">30 seconds ↷</button><button id="vreader-resume" type="button" class="vreader-control" hidden>Resume saved place</button></div><label class="vreader-sleep">Sleep timer<select id="vreader-sleep"><option value="off">Off</option><option value="15">15 minutes</option><option value="30">30 minutes</option><option value="60">60 minutes</option><option value="section">End of section</option><option value="chapter" hidden disabled>End of chapter</option></select><span id="vreader-sleep-hint"></span></label><div class="vreader-preview-row"><button id="vreader-preview" type="button" class="vreader-control">Preview block</button></div><audio id="vreader-audio" preload="auto"></audio></div><div id="vreader-mini" class="vreader-mini" hidden role="region" aria-label="Compact audio player"><div id="vreader-mini-target" class="vreader-target" hidden><span id="vreader-mini-target-label"></span><button id="vreader-mini-target-read" type="button" class="vreader-control">Read from here</button><button id="vreader-mini-target-dismiss" type="button" class="vreader-control vreader-dismiss" aria-label="Dismiss reading suggestion">×</button></div><div class="vreader-mini-row"><div class="vreader-mini-info"><span id="vreader-mini-meta" class="vreader-kicker">Listen</span><span id="vreader-mini-status" role="status" aria-live="polite">Ready</span></div><button id="vreader-mini-rewind" type="button" class="vreader-control" aria-label="Rewind 15 seconds">↶ 15</button><button id="vreader-mini-play" type="button" class="vreader-control vreader-play">Play</button><button id="vreader-mini-next" type="button" class="vreader-control" aria-label="Next block">Next</button><button id="vreader-expand" type="button" class="vreader-control" aria-label="Expand audio player" title="Voice, speed, and reading settings">Expand</button><button id="vreader-mini-close" type="button" class="vreader-control vreader-dismiss" aria-label="Stop and close audio player">×</button></div></div>';
    box.querySelector('#vreader-mini').insertAdjacentHTML('beforeend', '<button id="vreader-mini-resume" type="button" class="vreader-mini-resume" hidden>Resume saved place</button>');
    // Tabler Icons player-play and player-pause, MIT, https://github.com/tabler/tabler-icons.
    box.insertAdjacentHTML('beforeend', '<button id="vreader-inline-read" class="vreader-inline-read" type="button" aria-label="Read from here" title="Read from here" hidden><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 4v16l13 -8l-13 -8" /></svg><span>Read from here</span></button>');
    box.querySelector('.vreader-utility-row').insertAdjacentHTML('beforeend', '<button id="vreader-replay" type="button" class="vreader-control" disabled title="Replay the current sentence when word timings are available">Replay sentence</button>');
    box.querySelector('#vreader-style-panel').insertAdjacentHTML('afterend', '<section id="vreader-cast" class="vreader-cast" hidden><div class="vreader-cast-head"><strong>Voices</strong><label><input id="vreader-character-voices" type="checkbox" checked> Character voices</label></div><div id="vreader-cast-roles"></div></section>');
    // Keep playback together, followed by reading position and voice preferences.
    const readerPanel = box.querySelector('.vreader-panel');
    const playbackGroup = document.createElement('div'); playbackGroup.className = 'vreader-playback';
    readerPanel.prepend(playbackGroup);
    for (const selector of ['.vreader-head','.vreader-actions','.vreader-utility-row']) playbackGroup.appendChild(box.querySelector(selector));
    const readingGroup = box.querySelector('.vreader-navigator');
    const scopeLabel = box.querySelector('#vreader-mode').parentElement;
    scopeLabel.className = 'vreader-scope'; readingGroup.prepend(scopeLabel);
    const listeningSettings = box.querySelector('.vreader-settings');
    listeningSettings.setAttribute('role','group'); listeningSettings.setAttribute('aria-label','Voice and speed');
    const finishRow = document.createElement('div'); finishRow.className = 'vreader-finish-row';
    finishRow.append(box.querySelector('.vreader-sleep'),box.querySelector('.vreader-preview-row'));
    readerPanel.insertBefore(finishRow,box.querySelector('audio'));
    const share = document.getElementById('vshare-toggle'); share.parentNode.insertBefore(box, share);
    const get = name => box.querySelector('#vreader-' + name), audio = get('audio');
    const pauseLabel = document.createElement('label'); pauseLabel.textContent = 'Pauses ';
    const pauseSelect = document.createElement('select'); pauseSelect.id = 'vreader-pauses'; pauseSelect.innerHTML = '<option value="natural">Natural</option><option value="short">Short</option><option value="off">Off</option>';
    pauseLabel.appendChild(pauseSelect); box.querySelector('.vreader-settings').appendChild(pauseLabel);
    const readerMeasurements = [];
    window.artifactReaderDiagnostics = () => readerMeasurements.map(item => ({...item}));
    let previousAudioEnd = null;
    let ready = false, enabled = false, captureActive = false, epoch = 0, serial = 0, extraction = '', extractionTimer;
    let queue = [], position = 0, wants = false, loading = false, currentUrl = null, loaded = -1, chapter = null;
    let controller = new AbortController(), cache = new Map(), voice = '', mode = 'page';
    let audioContext = null, stream = null, limiter = null, pauseTimer = null, continuationPause = false;
    const streamRequests = new Map();
    let playbackConfig = null;
    let contentFingerprint = '', readerScopeKey = '', restartScopeKey = '', queueKey = '', scopeStart = 0, scopeEnd = 0, restoring = null, pendingSeek = 0;
    const durations = new Map();
    // ADR-0022: native progress (position, chapters, failure reason) for hosts with media.progress.
    let nativeProgress = false, failure = '', chapterOrder = [], progressCache = null;
    let sleepDeadline = 0, sleepBoundary = null;
    let compact = false, outlineRequest = '', outlineTimer, outlineFingerprint = '', detailTargets = [], clickedTarget = null, targetGeometry = null;
    function readerVisible() { return !get('panel').hidden || compact; }
    function pickMode() { send('reader:pick-mode', {enabled:enabled && ready && readerVisible() && !commentMode}); }
    function refreshOutline() {
      if (!enabled || !ready || !(readerVisible() || nativeProgress)) return;
      clearTimeout(outlineTimer); outlineRequest = 'outline-' + (++serial);
      get('outline').disabled = true; get('jump').disabled = true;
      send('reader:outline', {requestId:outlineRequest});
      outlineTimer = setTimeout(() => {
        if (!outlineRequest) return;
        outlineRequest = ''; get('outline').replaceChildren(new Option('Sections unavailable', ''));
      }, 5000);
    }
    function clearTarget() {
      clickedTarget = null; targetGeometry = null;
      get('target').hidden = get('mini-target').hidden = get('inline-read').hidden = get('target-details').hidden = true;
      send('reader:clear-target');
    }
    function placeTargetAction() {
      const action = get('inline-read'); action.hidden = true;
      if (!clickedTarget || !targetGeometry || !readerVisible() || commentMode) return;
      const bounds = frame.getBoundingClientRect(), g = targetGeometry;
      const sx = bounds.width / g.viewport.width, sy = bounds.height / g.viewport.height;
      const left = bounds.left + g.rect.left * sx, right = bounds.left + g.rect.right * sx;
      const top = bounds.top + g.rect.top * sy, bottom = bounds.top + g.rect.bottom * sy;
      const minY = Math.max(bounds.top, document.querySelector('.vbar').getBoundingClientRect().bottom) + 6;
      const maxY = Math.min(bounds.bottom, innerHeight) - 50;
      if (bottom <= minY || top >= maxY + 44 || right <= bounds.left || left >= bounds.right || maxY < minY) return;
      const y = Math.max(minY, Math.min(top, maxY)), above = Math.max(minY, Math.min(top - 50, maxY));
      const panel = (compact ? get('mini') : get('panel')).getBoundingClientRect();
      const candidates = [[right+4,y],[left>=bounds.left+38 ? Math.max(bounds.left,left-44) : -100,y],[Math.min(right-44,innerWidth-50),above],[Math.max(left,6),above]];
      const candidate = candidates.find(([x,cy]) => x>=Math.max(0,bounds.left) && x+44<=Math.min(innerWidth-6,bounds.right-6) && !(x<panel.right && x+44>panel.left && cy<panel.bottom && cy+44>panel.top));
      if (!candidate) return;
      const [x, cy] = candidate;
      action.querySelector('span').style.left = x < innerWidth / 2 ? '0' : 'auto';
      action.querySelector('span').style.right = x < innerWidth / 2 ? 'auto' : '0';
      action.style.left = x + 'px'; action.style.top = cy + 'px'; action.hidden = false;
    }
    function setPlayerView(view) {
      compact = view === 'compact'; get('panel').hidden = view !== 'full'; get('mini').hidden = !compact;
      get('toggle').setAttribute('aria-expanded', String(view !== 'closed'));
      if (view === 'closed') clearTarget();
      placePanel(); pickMode(); placeTargetAction();
      if (view !== 'closed') refreshOutline();
    }
    function jumpTo(fields) {
      if (captureActive || !enabled || !ready || (supportsStyle() && get('style').value === 'custom' && !instructions())) return;
      stop('Opening reading position…'); outlineRequest = ''; clearTimeout(outlineTimer); get('outline').disabled = true;
      mode = Number.isInteger(fields.chapterIndex) ? 'page' : 'here'; get('mode').value = mode;
      sleepBoundary = null; clearTarget(); wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null;
      if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial); send('reader:jump', Object.assign({requestId:extraction,mode}, fields));
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not open that position. Choose it again.', false, 'Could not open that position'); }, 5000); paint();
    }

    function checkpointKey() { if (isBundle && !currentPage) return ''; return placeKey(isBundle ? currentPage : ''); }
    function placeKey(page) { return 'artifact-reader-place:' + JSON.stringify([artifactId, configLiteral('viewerId'), page]); }
    function pageOfKey(key) { try { const parts = JSON.parse(key.slice('artifact-reader-place:'.length)); return typeof parts[2] === 'string' ? parts[2] : null; } catch (_) { return null; } }
