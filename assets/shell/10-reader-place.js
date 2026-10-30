    // ADR-0022: the saved place follows a signed-in viewer across devices. The checkpoint is copied to
    // viewer-scope state key "listen-place" ({version:1, pages:{<bundle page or "">:{savedAt, place}}});
    // localStorage stays the offline cache and the newer copy (by savedAt) wins on load. Writes go at
    // most every 30 s while playing, and at once on pause, stop, hide or page exit. Any failure, or
    // state being unavailable, leaves Listen on its local copy.
    const placeSync = (() => {
      const url = '/' + encodeURIComponent(artifactId) + '/state/listen-place?scope=viewer', MAX_BYTES = 48 * 1024, MAX_PAGES = 16;
      const pages = {};
      let disabled = shellConfig.stateEnabled !== '1' || historicalView || !configLiteral('viewerEmail');
      let started = false, settled = disabled, dirty = false, busy = false, timer = null, lastWrite = 0, blockedUntil = 0, settle;
      const ready = disabled ? Promise.resolve() : new Promise(resolve => { settle = () => { settled = true; resolve(); }; });
      const bytes = text => new TextEncoder().encode(text).length;
      function validEntry(entry) { return !!(entry && typeof entry === 'object' && Number.isFinite(entry.savedAt) && entry.savedAt > 0 && entry.place && typeof entry.place === 'object' && entry.place.version === 2 && entry.place.current && typeof entry.place.current === 'object'); }
      function readLocal(page) { try { const value = JSON.parse(localStorage.getItem(placeKey(page)) || 'null'); return value && value.version === 2 && value.current ? value : null; } catch (_) { return null; } }
      function entryOf(stored) { return {savedAt:Number(stored.savedAt), place:{version:2, current:stored.current, scopes:stored.scopes && typeof stored.scopes === 'object' ? stored.scopes : {}}}; }
      function load() {
        if (disabled || started) return ready; started = true;
        const timeout = setTimeout(settle, 2500);
        fetch(url, {cache:'no-store', credentials:'same-origin'}).then(response => {
          if (response.status === 404) return null;
          if (!response.ok) { if ([400, 401, 403, 503].includes(response.status)) disabled = true; return null; }
          return response.json();
        }).then(body => {
          const value = body && body.value;
          if (value && value.version === 1 && value.pages && typeof value.pages === 'object' && !Array.isArray(value.pages)) {
            for (const [page, entry] of Object.entries(value.pages).slice(0, MAX_PAGES)) {
              if (page.length > 512 || !validEntry(entry)) continue;
              pages[page] = entry;
              const local = readLocal(page);
              if (local && Number(local.savedAt) > entry.savedAt) { pages[page] = entryOf(local); dirty = true; continue; }
              try { localStorage.setItem(placeKey(page), JSON.stringify({...entry.place, savedAt:entry.savedAt})); } catch (_) {}
            }
          }
          // A place saved only on this device (for example while offline) is uploaded once.
          const local = isBundle ? null : readLocal('');
          if (local && Number(local.savedAt) > 0 && !pages['']) { pages[''] = entryOf(local); dirty = true; }
        }).catch(() => {}).finally(() => { clearTimeout(timeout); settle(); refreshSaved(); if (dirty) schedule(); });
        return ready;
      }
      function payload() {
        const list = Object.entries(pages).sort((a, b) => b[1].savedAt - a[1].savedAt).slice(0, MAX_PAGES);
        let text = JSON.stringify({version:1, pages:Object.fromEntries(list)});
        while (bytes(text) > MAX_BYTES && list.length > 1) { list.pop(); text = JSON.stringify({version:1, pages:Object.fromEntries(list)}); }
        if (bytes(text) > MAX_BYTES && list.length) {
          const [page, entry] = list[0];
          text = JSON.stringify({version:1, pages:{[page]:{savedAt:entry.savedAt, place:{version:2, current:entry.place.current, scopes:{}}}}});
        }
        return bytes(text) > MAX_BYTES ? null : text;
      }
      function write(keepalive) {
        clearTimeout(timer); timer = null;
        if (disabled || !dirty) return;
        if (!settled || busy) { timer = setTimeout(() => { timer = null; write(keepalive); }, 1000); return; }
        const wait = blockedUntil - Date.now(); if (wait > 0) { timer = setTimeout(() => write(false), wait); return; }
        const remove = !Object.keys(pages).length, text = remove ? '' : payload();
        dirty = false; if (!remove && !text) return;
        busy = true; lastWrite = Date.now();
        const init = remove ? {method:'DELETE', keepalive} : {method:'PUT', headers:{'content-type':'application/json'}, body:'{"value":' + text + '}', keepalive};
        fetch(url, init).then(response => {
          if (response.status === 429) { blockedUntil = Date.now() + 60000; dirty = true; }
          else if (remove && response.status === 404) {}
          else if ([400, 401, 403, 404, 409, 413].includes(response.status)) disabled = true;
          else if (!response.ok) dirty = true;
        }).catch(() => { dirty = true; }).finally(() => { busy = false; if (dirty) schedule(); });
      }
      function schedule() {
        if (disabled || timer) return;
        timer = setTimeout(() => { timer = null; write(false); }, Math.max(500, lastWrite + 30000 - Date.now(), blockedUntil - Date.now()));
      }
      return {
        ready, load, settled: () => settled,
        save(page, stored) {
          if (disabled || page === null || !stored || !Number.isFinite(stored.savedAt)) return;
          if (pages[page] && pages[page].savedAt === stored.savedAt) return;
          pages[page] = entryOf(stored); dirty = true; schedule();
        },
        clear(page) { if (disabled || page === null || !settled || !pages[page]) return; delete pages[page]; dirty = true; write(true); },
        flush(force) {
          if (!dirty || disabled) return;
          // Pause and stop write at once, but never more than one write every 2 s; hiding or
          // leaving the page always writes, since the page may not run again.
          const gap = lastWrite + 2000 - Date.now();
          if (gap > 0 && !force) { clearTimeout(timer); timer = setTimeout(() => { timer = null; write(false); }, gap); return; }
          write(true);
        }
      };
    })();
    function validCheckpoint(value) {
      return value && (value.version === 1 || value.version === 2) && (value.chapterIndex === undefined || Number.isInteger(value.chapterIndex) && value.chapterIndex >= 0) && typeof value.fingerprint === 'string' && value.fingerprint.length <= 100 &&
        ['page','view','selection','here','section','detail'].includes(value.mode) && Number.isInteger(value.ordinal) && value.ordinal >= 0 &&
        Number.isInteger(value.chunk) && value.chunk >= 0 && Number.isFinite(value.offset) && value.offset >= 0 && value.offset < 3600 &&
        Number.isInteger(value.start) && Number.isInteger(value.end) && value.start <= value.ordinal && value.end >= value.ordinal &&
        typeof value.voice === 'string' && Array.from(get('rate').options).some(o => o.value === value.rate) && (value.version === 1 || typeof value.scopeKey === 'string' && value.scopeKey.length <= 500);
    }
    function savedPlace() {
      try {
        const stored = JSON.parse(localStorage.getItem(checkpointKey()) || 'null');
        let value = stored;
        if (stored && stored.version === 2 && stored.current) {
          value = stored.current;
          if (readerScopeKey && stored.scopes) {
            const scoped = stored.scopes[mode + '\u0000' + readerScopeKey];
            if (scoped) value = scoped;
          }
        }
        if (validCheckpoint(value)) {
          if (!Array.from(get('voice').options).some(o => o.value === value.voice)) {
            const fallback = Array.from(get('voice').options).find(o => o.value === 'pocket_alba');
            if (!fallback) return null;
            // Different voices can use different chunks and timings. Keep the paragraph.
            value.voice = fallback.value; value.chunk = 0; value.offset = 0;
          }
          return value;
        }
      } catch (_) {} return null;
    }
    function refreshSaved() { get('resume').hidden = !savedPlace(); get('resume').disabled = !ready || !enabled || !!extraction; get('mini-resume').hidden = get('resume').hidden || !!queue.length || !!extraction; get('mini-resume').disabled = get('resume').disabled; }
    function mediaOffset() {
      if (!stream) return loaded === position ? audio.currentTime || 0 : pendingSeek;
      let offset = stream.baseOffset;
      for (const f of stream.frames) offset += f.played ? f.buffer.duration : Math.min(f.buffer.duration, f.offset + (f.source ? Math.max(0, audioContext.currentTime - f.start - (stream.latency || 0)) * f.rate : 0));
      return offset;
    }
    // Synthesized durations are media seconds at 1×. Unplayed blocks are estimated from their text,
    // calibrated against blocks already heard (about 15 characters a second before any are known).
    function charsPerSecond() {
      let chars = 0, secs = 0;
      durations.forEach((seconds, index) => { if (queue[index] && seconds > 0) { chars += queue[index].text.length; secs += seconds; } });
      return secs >= 5 && chars > 0 ? chars / secs : 15;
    }
    function blockSeconds(index, cps = charsPerSecond()) { return durations.get(index) ?? (queue[index] ? queue[index].text.length / cps : 0); }
    function progressFields() {
      if (!queue.length) return {};
      if (!progressCache || progressCache.queue !== queue || progressCache.known !== durations.size) {
        const cps = charsPerSecond(), starts = [0]; queue.forEach((_, index) => starts.push(starts[index] + blockSeconds(index, cps)));
        progressCache = {queue, known:durations.size, starts};
      }
      const at = Math.min(position, queue.length), starts = progressCache.starts, total = starts[queue.length];
      const within = at < queue.length ? Math.min(Math.max(0, mediaOffset()), Math.max(0, starts[at + 1] - starts[at])) : 0;
      const result = {position:Math.min(total, starts[at] + within), duration:total};
      const chapterAt = chapter && Number.isInteger(chapter.index) ? chapterOrder.indexOf(chapter.index) : -1;
      if (chapterAt >= 0) result.chapterIndex = chapterAt;
      return result;
    }
    function failureReason(error) {
      const text = String(error && error.message || '');
      if ((typeof navigator !== 'undefined' && navigator.onLine === false) || error?.name === 'TypeError' || /interrupted/i.test(text)) return 'No connection';
      if (/busy/i.test(text)) return 'Speech is busy';
      if (/Audio playback failed/i.test(text)) return 'Audio playback failed';
      return 'Speech is unavailable';
    }
    function savePlace() {
      const entry = queue[position];
      if (!entry || !queueKey || !contentFingerprint || (playbackConfig?.mode || mode) === 'selection' || previewEnd !== null || previewRequested || restoring || loaded !== position) return;
      try {
        const checkpoint = {version:2, fingerprint:contentFingerprint, ordinal:entry.ordinal, chunk:entry.chunk, offset:mediaOffset(), start:scopeStart, end:scopeEnd, chapterIndex:chapter?.index, voice:playbackConfig?.voice || voice, rate:get('rate').value, mode:playbackConfig?.mode || mode, scopeKey:readerScopeKey, style:playbackConfig?.style || get('style').value, custom:playbackConfig?.custom ?? get('custom').value};
        let stored = null; try { stored = JSON.parse(localStorage.getItem(queueKey) || 'null'); } catch (_) {}
        if (!stored || stored.version !== 2 || !stored.scopes || typeof stored.scopes !== 'object') stored = {version:2, current:null, scopes:{}};
        const scopeId = checkpoint.mode + '\u0000' + checkpoint.scopeKey;
        if (JSON.stringify(stored.current) !== JSON.stringify(checkpoint) || !Number.isFinite(stored.savedAt)) stored.savedAt = Date.now();
        stored.current = checkpoint; stored.scopes[scopeId] = checkpoint;
        const keys = Object.keys(stored.scopes); while (keys.length > 12) delete stored.scopes[keys.shift()];
        localStorage.setItem(queueKey, JSON.stringify(stored));
        placeSync.save(pageOfKey(queueKey), stored);
      } catch (_) {}
      refreshSaved();
    }
    function cancelSleep() { sleepDeadline = 0; sleepBoundary = null; get('sleep').value = 'off'; get('sleep-hint').textContent = ''; }
    function armSleep() {
      const choice = get('sleep').value;
      if (/^\d+$/.test(choice) && !sleepDeadline) sleepDeadline = Date.now() + Number(choice) * 60000;
      if (choice === 'section' && sleepBoundary === null && queue[position]) sleepBoundary = queue[position].sectionEndOrdinal;
    }
    function mayContinue(index) { return !(get('sleep').value === 'section' && sleepBoundary !== null && queue[index]?.ordinal > sleepBoundary); }
    function checkSleep() {
      if (sleepDeadline && Date.now() >= sleepDeadline) { stop('Sleep timer ended. Your place is saved.'); cancelSleep(); return true; }
      get('sleep-hint').textContent = sleepDeadline ? Math.ceil((sleepDeadline - Date.now()) / 60000) + ' min remaining' : '';
      return false;
    }
    setInterval(() => { checkSleep(); savePlace(); if (nativeProgress && wants) mirrorNative(); }, 2000);
    window.addEventListener('pagehide', () => { stop('Paused', true); placeSync.flush(true); });
    document.addEventListener('visibilitychange', () => { checkSleep(); savePlace(); if (document.visibilityState === 'hidden') placeSync.flush(true); });

    const readingStyles = {
      calm: 'Read as a calm audiobook narrator. Use consistent pacing, restrained emotion, and natural sentence endings. Avoid exaggerated emphasis and dramatic pitch changes.',
      neutral: 'Read clearly with an even, neutral delivery, consistent pacing, and natural pauses. Keep emotional expression minimal.',
      expressive: 'Read expressively with natural emotional variation appropriate to the text, clear phrasing, and varied emphasis.'
    };
    let previewEnd = null, previewStart = 0, previewRequested = false;
    let cast = null, roleVoices = {}, characterVoices = true, availableVoices = [], voiceGains = new Map(), preferences = {}, pendingCast;
    function supportsStyle(chosenVoice = voice) { return chosenVoice === 'qwen_ryan' || chosenVoice === 'qwen_aiden'; }
    function instructions(chosenVoice = voice) { return supportsStyle(chosenVoice) ? (get('style').value === 'custom' ? get('custom').value.trim() : readingStyles[get('style').value] || '') : ''; }
    function speechBody(text, chosenVoice) {
      const body = { text, voice: chosenVoice };
      if (supportsStyle(chosenVoice) && instructions(chosenVoice)) body.instructions = instructions(chosenVoice);
      return JSON.stringify(body);
    }
    function updateStyle() {
      get('style-panel').hidden = !supportsStyle();
      get('custom-label').hidden = get('style').value !== 'custom';
      get('style-hint').textContent = ({ calm: 'Steady pacing, restrained emotion, gentle emphasis.', neutral: 'Clear, even delivery with minimal emotion.', expressive: 'More variation in tone and emphasis.', default: 'Uses the model’s own delivery, without style instructions.', custom: 'Up to 500 characters. Style guides delivery; results can vary.' })[get('style').value];
      placePanel(); paint();
    }
    try {
      const saved = JSON.parse(localStorage.getItem('artifact-reader-style') || 'null');
      if (saved && ['calm','neutral','expressive','default','custom'].includes(saved.style)) get('style').value = saved.style;
      if (saved && typeof saved.custom === 'string') get('custom').value = saved.custom.slice(0,500);
    } catch (_) {}
    let savedVoice = '';
    try {
      const saved = JSON.parse(localStorage.getItem('artifact-reader-preferences') || 'null');
      if (saved && typeof saved === 'object') preferences = saved;
      if (preferences && typeof preferences.voice === 'string') savedVoice = preferences.voice;
      if (saved && Array.from(get('rate').options).some(option => option.value === saved.rate)) get('rate').value = saved.rate;
      if (saved && ['natural','short','off'].includes(saved.pauses)) get('pauses').value = saved.pauses;
    } catch (_) {}
    function savePreferences() {
      try { localStorage.setItem('artifact-reader-preferences', JSON.stringify({...preferences, voice, rate: get('rate').value, pauses:get('pauses').value})); } catch (_) {}
    }
    function defaultRoleVoices(nextCast, voices, mainVoice) {
      const ids = voices.map(item => typeof item === 'string' ? item : item.id).filter(Boolean), result = {};
      const family = String(mainVoice || '').split('_')[0], used = new Set();
      for (const role of nextCast.roles) {
        if (role.id === nextCast.default) { result[role.id] = ''; continue; }
        const suggested = (role.suggest || []).find(id => ids.includes(id));
        if (suggested) { result[role.id] = suggested; used.add(suggested); continue; }
        const candidates = ids.filter(id => id !== mainVoice && !used.has(id));
        const chosen = candidates.find(id => id.split('_')[0] === family) || candidates[0] || '';
        result[role.id] = chosen; if (chosen) used.add(chosen);
      }
      return result;
    }
    function validCast(value) {
      if (!value || value.version !== 1 || !Array.isArray(value.roles) || value.roles.length < 1 || value.roles.length > 24 || typeof value.default !== 'string') return null;
      const ids = new Set(), roles = [];
      for (const role of value.roles) {
        if (!role || !/^[a-z][a-z0-9_-]{0,31}$/.test(role.id) || ids.has(role.id) || typeof role.label !== 'string' || !role.label.trim() || role.label.length > 60 || typeof role.description !== 'string' || role.description.length > 160 || !Array.isArray(role.suggest) || role.suggest.length > 4 || role.suggest.some(id => typeof id !== 'string' || id.length > 64 || !/^[a-z0-9_]+$/.test(id))) return null;
        ids.add(role.id); roles.push({id:role.id,label:role.label,description:role.description,suggest:role.suggest});
      }
      return ids.has(value.default) ? {version:1,default:value.default,roles} : null;
    }
    function voiceFor(entry) { return characterVoices && cast && entry?.role && roleVoices[entry.role] ? roleVoices[entry.role] : voice; }
    function discardFutureVoiceAudio() {
      for (const index of cache.keys()) if (index > position) cache.delete(index);
      for (const [index, work] of streamRequests) if (index > position) { work.then(response => response.body?.cancel()).catch(() => {}); streamRequests.delete(index); }
      for (const index of [...aheadJobs.keys()]) if (index > position) forgetAhead(index);
      fillAhead();
    }
    function saveCastPreferences() {
      if (!cast) return;
      const casts = preferences.casts && typeof preferences.casts === 'object' ? preferences.casts : {};
      casts[artifactId] = {roles:cast.roles.map(role => role.id), enabled:characterVoices, voices:roleVoices};
      preferences = {...preferences, casts}; savePreferences();
    }
    function previewRole(role) {
      const chosenVoice = voiceFor({role:role.id});
      fetch('/' + encodeURIComponent(artifactId) + '/speech', {method:'POST',headers:{'content-type':'application/json'},body:speechBody(role.description || ('This is ' + role.label + '.'), chosenVoice)})
        .then(response => response.ok ? response.arrayBuffer() : Promise.reject()).then(buffer => { const sample = new Audio(URL.createObjectURL(new Blob([buffer],{type:'audio/wav'}))); sample.addEventListener('ended',()=>URL.revokeObjectURL(sample.src),{once:true}); return sample.play(); }).catch(() => status('Voice preview is unavailable.'));
    }
    function renderCast(nextCast) {
      // Role defaults depend on the voice list; wait for it rather than saving empty choices.
      if (nextCast && !availableVoices.length) { pendingCast = nextCast; return; }
      pendingCast = undefined;
      const signatureOf = value => value ? JSON.stringify(value) : '';
      if (cast && signatureOf(validCast(nextCast)) === signatureOf(cast)) return;
      cast = validCast(nextCast); const section = get('cast'), rows = get('cast-roles');
      rows.replaceChildren(); section.hidden = !cast;
      if (!cast) { roleVoices = {}; characterVoices = true; return; }
      const saved = preferences.casts?.[artifactId];
      const defaults = defaultRoleVoices(cast, availableVoices, voice);
      roleVoices = {...defaults};
      if (saved && Array.isArray(saved.roles) && saved.roles.join('\u0000') === cast.roles.map(role => role.id).join('\u0000')) {
        characterVoices = saved.enabled !== false;
        for (const role of cast.roles) if (saved.voices && (saved.voices[role.id] === '' || availableVoices.some(item => item.id === saved.voices[role.id]))) roleVoices[role.id] = saved.voices[role.id];
      } else characterVoices = true;
      get('character-voices').checked = characterVoices;
      for (const role of cast.roles) {
        const row = document.createElement('div'); row.className = 'vreader-cast-role';
        const title = document.createElement('strong'); title.textContent = role.label;
        const description = document.createElement('p'); description.textContent = role.description || ('This is ' + role.label + '.');
        const select = document.createElement('select'); select.setAttribute('aria-label', role.label + ' voice'); select.dataset.role = role.id; select.appendChild(new Option('Main voice',''));
        appendVoiceOptions(select, {current: roleVoices[role.id] || ''}); select.value = roleVoices[role.id] || '';
        select.addEventListener('change', () => { roleVoices[role.id] = select.value; discardFutureVoiceAudio(); saveCastPreferences(); paint(); });
        const preview = document.createElement('button'); preview.type = 'button'; preview.className = 'vreader-control'; preview.textContent = 'Preview'; preview.addEventListener('click', () => previewRole(role));
        row.append(title, description, select, preview); rows.appendChild(row);
      }
      get('character-voices').onchange = () => { characterVoices = get('character-voices').checked; discardFutureVoiceAudio(); saveCastPreferences(); paint(); };
    }
    function styleChanged() {
      stop('Style changed. Play or preview this paragraph.', true);
      try { localStorage.setItem('artifact-reader-style', JSON.stringify({ style: get('style').value, custom: get('custom').value })); } catch (_) {}
      updateStyle();
    }
    function preparePreview() {
      previewStart = position;
      const block = queue[position]?.block;
      while (position > 0 && queue[position - 1].block === block) position--;
      previewEnd = position;
      while (previewEnd < queue.length && queue[previewEnd].block === block) previewEnd++;
    }
    const send = (type, fields) => postToFrame(type, fields);
    function status(text) { get('status').textContent = text; get('mini-status').textContent = text; get('mini-status').title = text; }
    function readingMessage() {
      const entry = queue[position];
      const label = chapter?.label || entry?.sectionLabel || (entry ? 'block ' + (entry.ordinal + 1) : '');
      const role = cast && entry?.role ? cast.roles.find(item => item.id === entry.role) : null;
      const roleText = role ? role.label + ' · ' + (availableVoices.find(item => item.id === voiceFor(entry))?.name || voiceFor(entry)) : '';
      return (wants ? 'Reading' : 'Paused') + (roleText ? ' · ' + roleText : label ? ' · ' + label : '');
    }
