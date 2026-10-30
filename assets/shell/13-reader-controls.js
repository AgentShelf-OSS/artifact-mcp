    // Mirror of rewind: seek within the current block when its audio is there, otherwise carry the
    // remainder into the following blocks (known durations first, then text-length estimates).
    function forward(seconds) {
      if (captureActive || !queue.length || extraction || position >= queue.length) return;
      let target = position, offset = mediaOffset() + seconds;
      if (stream && loaded === position) {
        const received = stream.received / 24000;
        if (offset < received - 0.05 && stream.seekTo(offset)) { stream.highlightedWord = -1; savePlace(); paint(); return; }
        if (!stream.done) { const playing = wants; stop('Skipping ahead…', true); position = target; pendingSeek = Math.min(offset, 3599); wants = playing; loadCurrent(); return; }
        offset -= received; target++;
      } else if (!stream && loaded === position && Number.isFinite(audio.duration) && audio.duration > 0) {
        if (offset < audio.duration - 0.05) { audio.currentTime = offset; savePlace(); paint(); return; }
        offset -= audio.duration; target++;
      }
      const cps = charsPerSecond();
      while (target < queue.length && offset >= blockSeconds(target, cps)) offset -= blockSeconds(target++, cps);
      if (target >= queue.length) { if (wants) skip(1); return; }
      const playing = wants; stop('Skipping ahead…', true); position = target; pendingSeek = Math.max(0, Math.min(offset, 3599)); wants = playing; loadCurrent();
    }
    get('forward').onclick = () => forward(30);
    get('resume').onclick = () => {
      const saved = savedPlace(); if (!saved) return;
      stop('Restoring saved place…'); restoring = saved;
      voice = saved.voice; get('voice').value = voice; get('rate').value = saved.rate; mode = saved.mode; get('mode').value = mode;
      if (['calm','neutral','expressive','default','custom'].includes(saved.style)) get('style').value = saved.style;
      if (typeof saved.custom === 'string') get('custom').value = saved.custom.slice(0,500);
      updateStyle(); wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null; if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial); const resume = {requestId:extraction, mode, fingerprint:saved.fingerprint, chapterIndex:saved.chapterIndex};
      if (['view','detail','section'].includes(mode) && saved.scopeKey) resume.scopeKey = saved.scopeKey;
      send('reader:resume', resume);
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not restore this place. Start reading again.', false, 'Could not restore the saved place'); }, 5000); paint();
    };
    function skip(direction) {
      if (streamingVoice()) { const context = ensureAudioContext(); if (context && context.state === 'suspended') context.resume().catch(() => {}); }
      if (!queue.length) { wants = true; requestContent(false); return; }
      const block = queue[position] && queue[position].block;
      let target = position;
      if (direction > 0) { while (target < queue.length && queue[target].block === block) target++; }
      else { target = Math.max(0, position - 1); while (target > 0 && queue[target - 1].block === queue[target].block) target--; }
      stop('Preparing audio…', true); position = target; wants = true; loadCurrent();
    }
    get('prev').onclick = () => skip(-1); get('next').onclick = () => skip(1);
    get('mode').onchange = () => { stop(); restartScopeKey = ''; mode = get('mode').value; };
    get('voice').onchange = () => { voice = get('voice').value; savePreferences(); stop('Voice changed'); updateStyle(); const save = get('voice-save'); if (save) { const saved = favoriteVoices().includes(voice); save.textContent = saved ? 'Saved voice' : 'Save this voice'; save.setAttribute('aria-pressed', String(saved)); } };
    get('style').onchange = styleChanged;
    get('custom').oninput = styleChanged;
    get('preview').onclick = () => {
      const context = streamingVoice() ? ensureAudioContext() : null; if (context && context.state === 'suspended') context.resume().catch(() => {});
      if (queue.length && position < queue.length) {
        stop('Preparing preview…', true); preparePreview(); wants = true; loadCurrent();
      } else { wants = true; requestContent(false, true); }
    };
    get('pauses').onchange = () => savePreferences();
    get('rate').onchange = () => { savePreferences(); const value = Number(get('rate').value); audio.playbackRate = value; if (stream && !pitchReady && value !== 1) {
        const offset = mediaOffset(), playing = wants; stop('Preparing audio…', true); pendingSeek = offset; wants = playing; loadCurrent();
      } else if (stream && stream.changeRate) stream.changeRate(value); paint(); };
    audio.onerror = () => { if (audio.getAttribute('src')) stop('Could not play audio. Press Play to retry.', false, 'Audio playback failed'); };
    frame.addEventListener('load', () => { stop(); restartScopeKey = ''; clearTarget(); outlineRequest = ''; clearTimeout(outlineTimer); get('outline').replaceChildren(new Option('Loading sections…','')); get('outline').disabled = true; ready = false; send('reader:hello'); paint(); });
    window.addEventListener('message', event => {
      if (event.source !== frame.contentWindow || !event.data || typeof event.data !== 'object') return;
      const data = event.data;
      if (data.type === 'anchor:ready') setTimeout(() => { if (queue.length && !queueKey) queueKey = checkpointKey(); refreshSaved(); }, 0);
      if (data.type === 'reader:ready') { ready = true; pickMode(); refreshOutline(); paint(); maybeAutoplay(); watchNativeProgress(); return; }
      if (data.type === 'reader:changed') {
        const changedScope = typeof data.scopeKey === 'string' ? data.scopeKey : '';
        if ((!changedScope && !readerScopeKey) || (changedScope && changedScope === readerScopeKey)) { stop('Page changed. Press Play to read it.'); clearTarget(); refreshOutline(); }
        return;
      }
      if (data.type === 'reader:error' && outlineRequest && data.requestId === outlineRequest) { clearTimeout(outlineTimer); outlineRequest = ''; get('outline').replaceChildren(new Option('Sections unavailable','')); get('outline').disabled = true; paint(); return; }
      if (data.type === 'reader:target-position') {
        const rect = data.rect, viewport = data.viewport;
        if (!rect) { targetGeometry = null; get('inline-read').hidden = true; return; }
        if (!clickedTarget || !viewport || ![rect.left,rect.right,rect.top,rect.bottom,viewport.width,viewport.height].every(n=>Number.isFinite(n) && Math.abs(n)<10000000) || viewport.width<=0 || viewport.height<=0 || rect.right<rect.left || rect.bottom<rect.top) return;
        targetGeometry = {rect,viewport}; placeTargetAction(); return;
      }
      if (data.type === 'reader:target') {
        if (!readerVisible() || commentMode || !Number.isInteger(data.ordinal) || data.ordinal < 0 || data.ordinal >= 5000 || typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || typeof data.label !== 'string' || typeof data.scopeKey !== 'string' || data.scopeKey.length > 500) return;
        targetGeometry = null; clickedTarget = {ordinal:data.ordinal,fingerprint:data.fingerprint,scopeKey:data.scopeKey,detailAvailable:data.detailAvailable === true};
        get('target-label').textContent = get('mini-target-label').textContent = data.label.slice(0,160);
        get('target').hidden = get('mini-target').hidden = false; get('target-details').hidden = !clickedTarget.detailAvailable; paint(); return;
      }
      if (data.type === 'reader:outline' && outlineRequest && data.requestId === outlineRequest) {
        clearTimeout(outlineTimer); outlineRequest = '';
        const valid = (items, key) => Array.isArray(items) && items.length <= 500 && items.every(item => item && Number.isInteger(item[key]) && item[key] >= 0 && typeof item.label === 'string' && item.label.length <= 120);
        const validDetails = items => Array.isArray(items) && items.length <= 100 && items.every(item => item && typeof item.label === 'string' && item.label.length <= 120 && typeof item.scopeKey === 'string' && item.scopeKey.length <= 500);
        if (typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || !valid(data.sections,'ordinal') || !valid(data.chapters,'index') || !validDetails(data.details || [])) return;
        outlineFingerprint = data.fingerprint;
        renderCast(data.cast);
        chapterOrder = data.chapters.map(item => item.index);
        if (typeof nativeBroker !== 'undefined' && nativeBroker && nativeBroker.mirrorChapters) nativeBroker.mirrorChapters(data.chapters.map(item => item.label));
        detailTargets = data.details || [];
        const select = get('outline'); select.replaceChildren(new Option('Choose a section…',''));
        for (const [items, prefix, label, key] of [[data.chapters,'chapter','Chapters','index'],[data.sections,'section',data.chapters.length ? 'On this page' : 'Sections','ordinal']]) {
          if (!items.length) continue;
          const group = document.createElement('optgroup'); group.label = label;
          for (const item of items) group.appendChild(new Option(item.label, prefix + ':' + item[key]));
          select.appendChild(group);
        }
        if (detailTargets.length) {
          const group = document.createElement('optgroup'); group.label = 'Details';
          detailTargets.forEach((item, index) => group.appendChild(new Option(item.label, 'detail:' + index)));
          select.appendChild(group);
        }
        select.disabled = select.options.length < 2;
        if (select.disabled) select.options[0].textContent = 'No readable sections';
        paint(); return;
      }
      if ((data.type !== 'reader:content' && data.type !== 'reader:error') || !extraction || data.requestId !== extraction) return;
      extraction = ''; clearTimeout(extractionTimer);
      if (data.type === 'reader:error') { stop(typeof data.message === 'string' ? data.message.slice(0,200) : 'Could not read this page.', false, 'Could not read this page'); clearTarget(); refreshOutline(); return; }
      let total = 0;
      if (!Array.isArray(data.blocks) || data.blocks.length > 5000 || data.blocks.some(b => !b || typeof b.id !== 'string' || b.id.length > 80 || typeof b.text !== 'string' || (total += b.text.length) > 500000)) { stop('This page returned too much text.', false, 'This page is too long to read'); return; }
      renderCast(data.cast);
      const saved = restoring; restoring = null;
      queue = data.blocks.flatMap((b, index) => {
        let cursor = 0;
        const pronunciation = pronunciationPlan(b.text,b.pronunciations);
        return chunks(pronunciation.text, voiceFor({role:b.role})).map((text, chunk) => {
          const textOffset = pronunciation.text.indexOf(text, cursor);
          cursor = textOffset < 0 ? pronunciation.text.length : textOffset + text.length;
          const source = pronunciationRange(pronunciation,textOffset,textOffset+text.length);
          const sourceOffset = source?.start || 0, sourceText = source ? b.text.slice(source.start,source.end) : '';
          return {id:b.id, text, textOffset, pronunciation, sourceOffset, sourceText, block:index, ordinal:Number.isInteger(b.ordinal) ? b.ordinal : index, chunk, pause:['heading','scene','segment','none','short','long'].includes(b.pause) ? b.pause : '', role:cast && typeof b.role === 'string' && cast.roles.some(role => role.id === b.role) ? b.role : cast?.default || '', sectionLabel:typeof b.sectionLabel === 'string' ? b.sectionLabel.slice(0,120) : '', sectionEndOrdinal:Number.isInteger(b.sectionEndOrdinal) ? b.sectionEndOrdinal : data.blocks.length - 1};
        });
      });
      if (queue.reduce((size,entry)=>size+entry.text.length,0) > 500000) { stop('This page returned too much spoken text.', false, 'This page is too long to read'); return; }
      if (typeof data.scopeKey !== 'string' || data.scopeKey.length > 500) { stop('This reading scope is invalid.', false, 'Could not read this page'); return; }
      readerScopeKey = restartScopeKey = data.scopeKey; contentFingerprint = typeof data.fingerprint === 'string' && data.fingerprint.length <= 100 ? data.fingerprint : ''; queueKey = checkpointKey();
      if (saved) {
        if (contentFingerprint !== saved.fingerprint || saved.version === 2 && saved.scopeKey !== readerScopeKey) { stop('Content changed or this place belongs to a different view. Start reading again.', false, 'The saved place has changed'); return; }
        queue = queue.filter(entry => entry.ordinal >= saved.start && entry.ordinal <= saved.end);
        position = queue.findIndex(entry => entry.ordinal === saved.ordinal && entry.chunk === saved.chunk);
        if (position < 0) { stop('Saved place is no longer available. Start reading again.', false, 'The saved place is unavailable'); return; }
        pendingSeek = saved.offset;
      } else position = 0;
      scopeStart = queue[0]?.ordinal || 0; scopeEnd = queue[queue.length - 1]?.ordinal || 0;
      chapter = data.chapter && typeof data.chapter.label === 'string' ? { index: Number.isInteger(data.chapter.index) ? data.chapter.index : undefined, label: data.chapter.label.slice(0,120), hasNext: data.chapter.hasNext === true } : null;
      const chapterOption = get('sleep').querySelector('[value=chapter]'); chapterOption.hidden = chapterOption.disabled = !chapter;
      if (!chapter && get('sleep').value === 'chapter') cancelSleep();
      if (!queue.length) { stop(mode === 'selection' ? 'Select text in the artifact first.' : 'No readable text found.', false, mode === 'selection' ? 'No text is selected' : 'No readable text found'); return; }
      if (previewRequested) { previewRequested = false; preparePreview(); }
      status(data.truncated ? 'Long page shortened to the reading limit.' : chapter ? chapter.label + (['page','here'].includes(mode) ? ' · continues across chapters' : ' · this section') : 'Ready'); paint();
      clearTarget(); refreshOutline(); if (wants) loadCurrent();
    });
