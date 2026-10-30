    // ADR-0019: inside the Android app, mirror the player so Android Auto and the media
    // notification show it, and route their buttons back to these controls.
    function mirrorNative(clear) {
      if (typeof nativeBroker === 'undefined' || !nativeBroker || !nativeBroker.mirrorMedia) return;
      if (wants) failure = '';
      if (clear) { nativeBroker.mirrorMedia({state:'none', error:failure}); return; }
      const entry = queue[position], voiceName = availableVoices.find(item => item.id === voiceFor(entry))?.name || voiceFor(entry) || 'Listen';
      const role = cast && entry?.role ? cast.roles.find(item => item.id === entry.role)?.label : '';
      // ADR-0022: "buffering" while Listen wants to play but waits for text, audio, or a reconnect.
      const state = wants ? (loading || extraction || restoring ? 'buffering' : 'playing') : 'paused';
      nativeBroker.mirrorMedia(Object.assign({state, title:chapter?.label || entry?.sectionLabel || document.title || 'Listen', artist:role ? role + ' · ' + voiceName : voiceName, album:document.title || '', rate:get('rate').value, error:failure,
        handlers:{play:()=>{if(!wants)get('play').click();}, pause:()=>{if(wants)get('play').click();}, next:()=>skip(1), previous:()=>skip(-1), stop:()=>get('stop').click(),
          rewind:()=>{const button = get('rewind'); if (button && !button.disabled) button.click();},
          forward:()=>forward(30),
          chapter:index=>{const target = chapterOrder[index]; if (Number.isInteger(target)) jumpTo({chapterIndex:target});},
          speed:()=>{const select = get('rate'), steps = ['1','1.25','1.5','1.75','2'], at = steps.indexOf(select.value); select.value = steps[(at + 1) % steps.length]; select.dispatchEvent(new Event('change', {bubbles:true})); paint();}}}, nativeProgress ? progressFields() : {}));
    }
    // `?listen=autoplay` starts reading once, but only inside the native host: the Android app uses
    // it when an artifact is picked from the car's media library (ADR-0019).
    let autoplayDone = false;
    // Hosts with media.progress also get the chapter list, so ask the bridge for the outline even
    // while the player is closed (for example a car-started narration).
    function watchNativeProgress() {
      if (nativeProgress || typeof nativeBroker === 'undefined' || !nativeBroker || !nativeBroker.hostHas) return;
      nativeBroker.hostHas('media.progress').then(ok => { if (!ok || nativeProgress) return; nativeProgress = true; refreshOutline(); paint(); });
    }
    function maybeAutoplay() {
      if (autoplayDone || !enabled || !ready || wants || new URLSearchParams(window.location.search).get('listen') !== 'autoplay') return;
      if (typeof nativeBroker === 'undefined' || !nativeBroker || !nativeBroker.hostHas) return;
      Promise.all([nativeBroker.hostHas('media.mirror'), placeSync.ready]).then(([ok]) => { if (!ok || autoplayDone || wants) return; autoplayDone = true; get(savedPlace() ? 'resume' : 'play').click(); });
    }
    function updateMediaSession(clear) {
      mirrorNative(clear);
      const session = navigator.mediaSession; if (!session) return;
      try {
        if (clear) { session.metadata = null; session.playbackState = 'none'; ['play','pause','stop','seekbackward','seekforward','previoustrack','nexttrack'].forEach(action => { try { session.setActionHandler(action, null); } catch (_) {} }); return; }
        const entry = queue[position], voiceName = availableVoices.find(item => item.id === voiceFor(entry))?.name || voiceFor(entry) || 'Listen';
        const role = cast && entry?.role ? cast.roles.find(item => item.id === entry.role)?.label : '';
        session.metadata = new MediaMetadata({title:chapter?.label || entry?.sectionLabel || document.title,artist:role ? role + ' · ' + voiceName : voiceName,album:document.title,artwork:[{src:'/icons/app-192.png',sizes:'192x192',type:'image/png'},{src:'/icons/app-512.png',sizes:'512x512',type:'image/png'}]});
        session.playbackState = wants ? 'playing' : 'paused';
        const handlers={play:()=>get('play').click(),pause:()=>{if(wants)get('play').click();},stop:()=>get('stop').click(),seekbackward:()=>get('rewind').click(),seekforward:()=>forward(30),previoustrack:()=>skip(-1),nexttrack:()=>skip(1)};
        Object.entries(handlers).forEach(([action,handler]) => { try { session.setActionHandler(action,handler); } catch (_) {} });
      } catch (_) {}
    }
    function paint() { updateMediaSession(!queue.length && !loading); get('replay').disabled = replaySentenceStart() === null; get('rewind').disabled = loaded < 0; get('forward').disabled = loaded < 0 || !!extraction; refreshSaved(); box.classList.toggle('is-playing', wants); box.classList.toggle('is-loading', loading); get('play').textContent = wants ? 'Pause' : loaded >= 0 || loading ? 'Resume' : 'Play'; get('play').setAttribute('aria-label', wants ? 'Pause reading' : loaded >= 0 || loading ? 'Resume reading' : 'Start reading'); get('play').disabled = !enabled || !ready || (!wants && supportsStyle() && get('style').value === 'custom' && !instructions()); get('preview').disabled = !enabled || !ready || (supportsStyle() && get('style').value === 'custom' && !instructions()); get('stop').disabled = !queue.length && !extraction;
      const icon = wants
        ? '<path d="M6 6a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1l0 -12" /><path d="M14 6a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1l0 -12" />'
        : '<path d="M7 4v16l13 -8l-13 -8" />';
      get('mini-play').innerHTML = '<svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + icon + '</svg>';
      const voiceName = get('voice').selectedOptions[0]?.textContent.split(' · ')[0] || 'Listen';
      const scopeName = {page:'Document',view:'Current view',detail:'Details',section:'Section',selection:'Selection',here:'From here'}[mode] || 'Listen';
      get('mini-meta').textContent = scopeName + ' · ' + voiceName + ' · ' + get('rate').value + '×';
      get('mini-play').title = get('play').getAttribute('aria-label');
      get('mini-play').disabled = get('play').disabled;
      get('mini-play').setAttribute('aria-label', get('play').getAttribute('aria-label'));
      get('mini-rewind').disabled = get('rewind').disabled;
      get('mini-next').disabled = !queue.length || !!extraction;
      const canJump = enabled && ready && !commentMode && !extraction && !(supportsStyle() && get('style').value === 'custom' && !instructions());
      get('jump').disabled = !canJump || !get('outline').value || get('outline').disabled;
      get('target-read').disabled = get('mini-target-read').disabled = get('inline-read').disabled = !canJump;
      placeTargetAction();
    }
    function replaySentenceStart() {
      // Reuse only the current stream's decoded audio. No new audio cache,
      // storage writes, network request, or synthesis is needed for replay.
      if (!stream?.wordMapping || !stream.words.length || typeof Intl.Segmenter !== 'function') return null;
      const entry = queue[position];
      if (!entry || loaded !== position) return null;
      const time = mediaOffset();
      const word = stream.words.findLast(w => w.start <= time);
      if (!word) return null;
      const sentence = Array.from(new Intl.Segmenter('en', {granularity:'sentence'}).segment(entry.text))
        .find(s => word.textStart >= s.index && word.textStart < s.index + s.segment.length);
      if (!sentence) return null;
      const first = stream.words.find(w => w.textStart >= sentence.index);
      // A stream resumed partway through a sentence may not retain its start.
      return first && first.start >= stream.baseOffset ? first.start : null;
    }
    function clearStream() {
      if (!stream) return;
      cancelAnimationFrame(stream.highlightFrame);
      if (stream.reader) stream.reader.cancel().catch(() => {});
      stream.sources.forEach(source => { try { source.onended = null; source.stop(); } catch (_) {} });
      stream.pitch?.disconnect(); if (stream.silence) { stream.silence.stop(); stream.silence.disconnect(); }
      stream.sources.clear(); if (stream.finish) stream.finish.resolve(); stream = null; loaded = -1;
      box.removeAttribute('data-stream-state'); box.removeAttribute('data-played-samples'); box.removeAttribute('data-buffered-samples');
    }
    let pitchModule = null, pitchReady = false;
    function ensurePitchModule(context) {
      if (!context.audioWorklet || !window.AudioWorkletNode) return Promise.resolve(false);
      if (!pitchModule) pitchModule = context.audioWorklet.addModule('/reader-audio/pitch-v1.js')
        .then(() => pitchReady = true).catch(() => false);
      return pitchModule;
    }
    function ensureAudioContext() {
      if (!window.AudioContext && !window.webkitAudioContext) return null;
      if (!audioContext) { try { audioContext = new (window.AudioContext || window.webkitAudioContext)(); limiter = audioContext.createDynamicsCompressor(); limiter.threshold.value=-3; limiter.knee.value=0; limiter.ratio.value=20; limiter.attack.value=.003; limiter.release.value=.25; limiter.connect(audioContext.destination); } catch (_) { return null; } }
      return audioContext;
    }
    function stop(message, preserve, reason) {
      savePlace(); placeSync.flush(); pendingSeek = 0; restoring = null; failure = reason || ''; clearAhead();
      epoch++; wants = false; loading = false; extraction = ''; previewEnd = null; previewRequested = false; clearTimeout(extractionTimer);
      controller.abort(); controller = new AbortController(); clearTimeout(pauseTimer); pauseTimer = null; cache.clear();
      streamRequests.forEach(work => { work.then(response => response.body?.cancel()).catch(() => {}); });
      streamRequests.clear();
      clearStream();
      audio.onended = null; audio.pause(); audio.removeAttribute('src'); audio.load(); loaded = -1;
      if (currentUrl) URL.revokeObjectURL(currentUrl); currentUrl = null;
      if (!preserve) { previousAudioEnd = null; queue = []; position = 0; chapter = null; durations.clear(); contentFingerprint = ''; readerScopeKey = ''; queueKey = ''; const option = get('sleep').querySelector('[value=chapter]'); option.hidden = option.disabled = true; }
      send('reader:highlight', { id: null }); updateMediaSession(!preserve); status(message || 'Ready'); paint();
    }
    function pronunciationPlan(text, hints) {
      const segments = []; let spoken = '', cursor = 0;
      if (!Array.isArray(hints) || hints.length > 128) hints = [];
      const valid = hints.every(h => h && Number.isInteger(h.start) && Number.isInteger(h.end) && h.start >= cursor && h.end > h.start && h.end <= text.length && h.end-h.start <= 200 && typeof h.text === 'string' && h.text.trim().length > 0 && h.text.length <= 200 && !/[\u0000-\u001f]/.test(h.text) && (cursor = h.end));
      if (!valid) hints = [];
      cursor = 0;
      function add(value, start, end, replacement) {
        if (!value) return;
        segments.push({start:spoken.length,end:spoken.length+value.length,sourceStart:start,sourceEnd:end,replacement}); spoken += value;
      }
      for (const hint of hints) {
        add(text.slice(cursor,hint.start),cursor,hint.start,false);
        add(hint.text.replace(/\s+/g,' ').trim(),hint.start,hint.end,true); cursor = hint.end;
      }
      add(text.slice(cursor),cursor,text.length,false);
      return {text:spoken,segments};
    }
    function pronunciationRange(plan, start, end) {
      const first = plan.segments.find(s=>s.start<=start && start<s.end);
      const last = plan.segments.find(s=>s.start<end && end<=s.end);
      if (!first || !last) return null;
      return {start:first.replacement ? first.sourceStart : first.sourceStart+start-first.start,
        end:last.replacement ? last.sourceEnd : last.sourceStart+end-last.start};
    }
    function splitLongText(text, limit) {
      const result = [], chars = Array.from(text); let start = 0;
      while (start < chars.length) {
        let end = Math.min(start + limit, chars.length);
        if (end < chars.length) {
          for (let i = end; i > start + Math.floor(limit / 2); i--) {
            if (/\s/.test(chars[i - 1])) { end = i; break; }
          }
        }
        const value = chars.slice(start, end).join('').trim(); if (value) result.push(value); start = end;
      }
      return result;
    }
    function chunks(text, selectedVoice) {
      const limit = /^moss_/.test(selectedVoice) ? 300 : 600;
      if (!/^(pocket_|raven_)/.test(selectedVoice) || typeof Intl.Segmenter !== 'function') return splitLongText(text, limit);
      // Keep complete sentences together. Only split within a sentence when it
      // exceeds the worker chunk limit, including text without punctuation.
      const result = []; let pending = '';
      for (const { segment } of new Intl.Segmenter('en', { granularity: 'sentence' }).segment(text)) {
        const sentence = segment.trim();
        if (!sentence) continue;
        if (pending && Array.from(pending + ' ' + sentence).length > limit) { result.push(pending); pending = ''; }
        if (Array.from(sentence).length > limit) {
          const parts = splitLongText(sentence, limit); result.push(...parts.slice(0, -1)); pending = parts[parts.length - 1] || '';
        } else pending = pending ? pending + ' ' + sentence : sentence;
      }
      if (pending) result.push(pending);
      return result;
    }
    function pauseBefore(entry, previous, chapterContinuation) {
      if (entry.chunk > 0 || !previous && !chapterContinuation) return 0;
      const base = chapterContinuation ? 1500 : entry.pause === 'none' ? 0 : entry.pause === 'scene' ? 1800 : entry.pause === 'long' || entry.pause === 'heading' ? 1200 : entry.pause === 'short' ? 300 : previous.pause === 'heading' ? 900 : entry.pause === 'segment' ? 150 : 600;
      const setting = get('pauses').value;
      return (setting === 'off' ? 0 : setting === 'short' ? base / 2 : base) / Number(get('rate').value);
    }
    function voiceGain(voiceId) {
      const supplied = voiceGains.get(voiceId);
      const gain = Number.isFinite(supplied) ? supplied : Number(VOICE_GAIN_DB[voiceId] || 0);
      return Math.max(-12, Math.min(12, gain));
    }
    function delay(ms, signal) {
      return new Promise((resolve, reject) => {
        if (signal.aborted) return reject(new DOMException('Cancelled', 'AbortError'));
        const cancel = () => { clearTimeout(timer); reject(new DOMException('Cancelled', 'AbortError')); };
        const timer = setTimeout(() => { signal.removeEventListener('abort', cancel); resolve(); }, ms);
        signal.addEventListener('abort', cancel, { once: true });
      });
    }
