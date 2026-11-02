    // Read ahead at least 3 blocks or about 60 s of listening, whichever is more, bounded to
    // 8 blocks and 24 MB of audio. Worker slots are shared by every listener (TTS_WORKER_CONCURRENCY),
    // so one viewer never runs more than 2 speech requests at once, and a 429 pauses read-ahead with
    // exponential backoff instead of retrying it.
    const AHEAD_MIN_BLOCKS = 3, AHEAD_SECONDS = 60, AHEAD_MAX_BLOCKS = 8, AHEAD_MAX_BYTES = 24 * 1024 * 1024, SPEECH_PARALLEL = 2;
    const aheadJobs = new Map(), aheadBytes = new Map();
    let aheadFrom = -1, aheadBackoff = 0, aheadRetryAt = 0, aheadTimer = null;
    function linkedController() {
      const local = new AbortController(), parent = controller.signal;
      if (parent.aborted) local.abort(); else parent.addEventListener('abort', () => local.abort(), {once:true});
      return local;
    }
    function aheadBusy() { aheadBackoff = Math.min(30000, aheadBackoff ? aheadBackoff * 2 : 2000); aheadRetryAt = Date.now() + aheadBackoff; }
    function forgetAhead(index) { const job = aheadJobs.get(index); aheadJobs.delete(index); aheadBytes.delete(index); if (job) job.abort(); cache.delete(index); streamRequests.delete(index); }
    function clearAhead() { clearTimeout(aheadTimer); aheadTimer = null; aheadFrom = -1; aheadJobs.clear(); aheadBytes.clear(); }
    // The block about to play outranks read-ahead: free a slot by dropping the farthest prefetch.
    function makeRoomFor(index) {
      if (cache.has(index) || streamRequests.has(index)) return;
      const far = [...aheadJobs.keys()].sort((a, b) => b - a);
      while (aheadJobs.size > SPEECH_PARALLEL - 1 && far.length) forgetAhead(far.shift());
    }
    function aheadDone(index, job, ok) {
      if (aheadJobs.get(index) !== job) return;
      aheadJobs.delete(index); if (ok) aheadBackoff = 0;
      fillAhead();
    }
    function fillAhead() {
      clearTimeout(aheadTimer); aheadTimer = null;
      if (captureActive || aheadFrom < 0 || aheadFrom !== position || !queue.length) return;
      const wait = aheadRetryAt - Date.now();
      if (wait > 0) { aheadTimer = setTimeout(fillAhead, wait); return; }
      let bytes = 0;
      for (const [index, size] of aheadBytes) { if (index > position && (cache.has(index) || streamRequests.has(index))) bytes += size; else aheadBytes.delete(index); }
      const rate = Number(get('rate').value) || 1, cps = charsPerSecond();
      let seconds = 0, count = 0;
      for (let index = position + 1; index < queue.length && count < AHEAD_MAX_BLOCKS && (count < AHEAD_MIN_BLOCKS || seconds < AHEAD_SECONDS); index++) {
        if (!mayContinue(index) || (previewEnd !== null && index >= previewEnd)) break;
        count++; seconds += blockSeconds(index, cps) / rate;
        if (cache.has(index) || streamRequests.has(index)) continue;
        if (aheadJobs.size >= SPEECH_PARALLEL || bytes >= AHEAD_MAX_BYTES) break;
        prefetch(index);
      }
    }
    function synth(index, ahead = false) {
      if (captureActive) return Promise.reject(new DOMException('Reader paused while casting.', 'AbortError'));
      if (cache.has(index)) return cache.get(index);
      if (!ahead) makeRoomFor(index);
      const job = linkedController(), signal = job.signal, entry = queue[index], chosenVoice = voiceFor(entry);
      if (ahead) aheadJobs.set(index, job);
      const work = (async () => {
        for (let attempt = 0; attempt < 5; attempt++) {
          const response = await fetch('/' + encodeURIComponent(artifactId) + '/speech', { method: 'POST', headers: { 'content-type': 'application/json' }, body: speechBody(entry.text, chosenVoice), signal });
          if (response.status === 429 && ahead) { aheadBusy(); throw new Error('Speech is busy.'); }
          if (response.status === 429 && attempt < 4) { await delay(1500, signal); continue; }
          if (!response.ok) throw new Error(response.status === 429 ? 'Speech is busy. Press Play to retry.' : 'Speech is unavailable. Press Play to retry.');
          if (!(response.headers.get('content-type') || '').startsWith('audio/wav')) throw new Error('Invalid audio response.');
          const buffer = await response.arrayBuffer();
          if (ahead) aheadBytes.set(index, buffer.byteLength);
          return buffer;
        }
      })();
      cache.set(index, work); work.catch(() => { if (cache.get(index) === work) cache.delete(index); });
      if (ahead) work.then(() => aheadDone(index, job, true), () => aheadDone(index, job, false));
      return work;
    }
    // RAVEN trial uses complete paragraph WAVs while its live-stream buzzing is investigated.
    function streamingVoice(entry = queue[position]) { return /^(qwen_|pocket_)/.test(voiceFor(entry)); }
    // Voice roles can change engine between blocks, so prefetch the way each block will play.
    function prefetch(index) { if (streamingVoice(queue[index]) && ensureAudioContext()) fetchStream(index, true).catch(() => {}); else synth(index, true).catch(() => {}); }
    // A read-ahead stream is read to the end so its worker slot is released, then replayed from memory.
    async function bufferedResponse(response, index, signal) {
      if (!response.ok || !response.body) return response;
      const reader = response.body.getReader(), parts = []; let size = 0;
      for (;;) {
        if (signal.aborted) { reader.cancel().catch(() => {}); throw new DOMException('Cancelled', 'AbortError'); }
        const step = await reader.read(); if (step.done) break;
        size += step.value.byteLength; if (size > 5 * 1024 * 1024) { reader.cancel().catch(() => {}); throw new Error('Streaming audio is too large.'); }
        parts.push(step.value);
      }
      aheadBytes.set(index, size);
      return new Response(new Blob(parts), { status: response.status, headers: { 'content-type': response.headers.get('content-type') || '' } });
    }
    function fetchStream(index, ahead = false) {
      if (captureActive) return Promise.reject(new DOMException('Reader paused while casting.', 'AbortError'));
      if (streamRequests.has(index)) return streamRequests.get(index);
      if (!ahead) makeRoomFor(index);
      const job = linkedController(), signal = job.signal, entry = queue[index], chosenVoice = voiceFor(entry);
      if (ahead) aheadJobs.set(index, job);
      const work = (async () => {
        for (let attempt = 0; attempt < 10; attempt++) {
          const timed = /^pocket_/.test(chosenVoice);
          const options = { method: 'POST', headers: { 'content-type': 'application/json' }, body: speechBody(entry.text, chosenVoice), signal };
          let response = await fetch('/' + encodeURIComponent(artifactId) + (timed ? '/speech/stream-timed' : '/speech/stream'), options);
          if (timed && [404, 415].includes(response.status)) {
            await response.body?.cancel();
            response = await fetch('/' + encodeURIComponent(artifactId) + '/speech/stream', options);
          }
          if (response.status === 429 && ahead) { await response.body?.cancel(); aheadBusy(); throw new Error('Speech is busy.'); }
          if (response.status !== 429 || attempt === 9) return ahead ? bufferedResponse(response, index, signal) : response;
          await response.body?.cancel(); await delay(500, signal);
        }
      })();
      streamRequests.set(index, work);
      // A failed read-ahead is dropped, so the block is requested again (with retries) when it plays.
      work.catch(() => { if (ahead && streamRequests.get(index) === work) streamRequests.delete(index); });
      if (ahead) work.then(() => aheadDone(index, job, true), () => aheadDone(index, job, false));
      return work;
    }
    async function streamCurrent(index) {
      if (captureActive) return true;
      const measurement = {startedAt:performance.now(),prefetched:streamRequests.has(index),firstScheduledAudioMs:null,interChunkGapMs:null,underruns:0,maxUnderrunMs:0,receivedAudioSeconds:0};
      readerMeasurements.push(measurement); if (readerMeasurements.length > 50) readerMeasurements.shift();
      const context = ensureAudioContext();
      if (!context) return false;
      const token = epoch, signal = controller.signal, entry = queue[index];
      await ensurePitchModule(context);
      if (token !== epoch || signal.aborted) return true;
      // Older/insecure browsers retain native pitch preservation through WAV playback.
      if (!pitchReady && Number(get('rate').value) !== 1) {
        const pending = streamRequests.get(index); streamRequests.delete(index);
        pending?.then(response => response.body?.cancel()).catch(() => {});
        return false;
      }
      const startup = streamRequests.has(index) ? 0.01 : 0.2;
      const work = fetchStream(index);
      let response;
      try { response = await work; }
      catch (error) { error.retryable = true; throw error; }
      finally { if (streamRequests.get(index) === work) streamRequests.delete(index); }
      if (token !== epoch || signal.aborted) { response.body?.cancel().catch(() => {}); return true; }
      if (response.status === 404 || response.status === 415) { await response.body?.cancel(); return false; }
      if (!response.ok) throw Object.assign(new Error(response.status === 429 ? 'Speech is busy. Press Play to resume.' : 'Speech is unavailable. Press Play to resume.'), {retryable:response.status >= 500});
      if (!response.body) return false;
      if (!(response.headers.get('content-type') || '').toLowerCase().startsWith('application/vnd.artifact.pcm')) throw new Error('Invalid streaming audio response.');
      if (token !== epoch || signal.aborted) { response.body.cancel().catch(() => {}); return true; }
      const timed = (response.headers.get('content-type') || '').toLowerCase().split(';')[0].trim() === 'application/vnd.artifact.pcm-timed';
      const reader = response.body.getReader();
      const seek = pendingSeek; pendingSeek = 0;
      const pauseMs = pauseBefore(entry, queue[index - 1], index === 0 && continuationPause); if (index === 0) continuationPause = false;
      const state = stream = { words:[], wordCursor:0, wordMapping:true, highlightedWord:-1, highlightFrame:0, baseOffset:seek, received:0, token, index, reader, sources: new Set(), frames: [], pending: new Uint8Array(0), done: false, started: false, bytes: 0, total: 0, played: 0, buffered: 0, cursor: context.currentTime + startup + pauseMs / 1000, rate: Number(get('rate').value), finished: null, finish: null, pauseMs };
      state.finished = new Promise((resolve, reject) => { state.finish = { resolve, reject }; });
      state.finished.catch(() => {}); // A processor failure can precede the network terminator.
      function acceptWord(bytes) {
        // Timing is optional. A bad text alignment must never interrupt sound.
        if (!state.wordMapping) return;
        try {
          if (bytes.length > 8192) throw new Error('Word event too large');
          const word = JSON.parse(new TextDecoder('utf-8', {fatal:true}).decode(bytes));
          if (!word || typeof word.word !== 'string' || !word.word.length || word.word.length > 1500 ||
              !Number.isInteger(word.index) || word.index < 0 || word.index > 2000 ||
              !Number.isFinite(word.start) || word.start < 0 || word.start > 90 ||
              (word.end !== undefined && (!Number.isFinite(word.end) || word.end < word.start || word.end > 90))) throw new Error('Invalid word timing');
          const previous = state.words[word.index];
          if (previous) {
            if (previous.word !== word.word || previous.start !== word.start) throw new Error('Changed word timing');
            if (word.end !== undefined) previous.end = word.end;
            paint(); return;
          }
          if (word.index !== state.words.length || word.start < (state.words.at(-1)?.start || 0)) throw new Error('Out of order timing');
          const start = entry.text.indexOf(word.word, state.wordCursor);
          if (start < 0 || /[\p{L}\p{N}]/u.test(entry.text.slice(state.wordCursor, start))) throw new Error('Unmapped text');
          state.wordCursor = start + word.word.length;
          state.words.push({...word, textStart:start, textEnd:state.wordCursor}); paint();
        } catch (_) {
          state.wordMapping = false; state.words = [];
          if (state.highlightedWord !== -1) send('reader:highlight', {id:entry.id});
          state.highlightedWord = -1;
        }
      }
      function highlightPlayback() {
        if (stream !== state || token !== epoch) return;
        // AudioContext time freezes on pause. Wait through initial buffering and
        // the pitch processor's delay before replacing the passage highlight.
        if (wants && context.state === 'running' && state.wordMapping &&
            state.frames.some(f => f.played || f.source && context.currentTime >= f.start + (state.latency || 0))) {
          const time = mediaOffset();
          let current = -1;
          for (let i = 0; i < state.words.length; i++) {
            const word = state.words[i], end = word.end ?? state.words[i + 1]?.start ?? Infinity;
            if (word.start <= time && time < end) current = i;
            if (word.start > time) break;
          }
          if (current >= 0 && current !== state.highlightedWord) {
            const word = state.words[current]; state.highlightedWord = current; get('replay').disabled = replaySentenceStart() === null;
            const mapped = pronunciationRange(entry.pronunciation,entry.textOffset+word.textStart,entry.textOffset+word.textEnd);
            if (mapped && entry.sourceText.length <= 3000) send('reader:word', {id:entry.id, text:entry.sourceText, offset:entry.sourceOffset, start:mapped.start-entry.sourceOffset, end:mapped.end-entry.sourceOffset});
            else send('reader:highlight',{id:entry.id});
          }
        }
        state.highlightFrame = requestAnimationFrame(highlightPlayback);
      }
      state.highlightFrame = requestAnimationFrame(highlightPlayback);
      function resetPitch(rate) {
        state.pitch?.disconnect();
        if (state.silence) { state.silence.stop(); state.silence.disconnect(); }
        state.pitch = null; state.silence = null; state.latency = rate === 1 ? 0 : 0.12;
        if (rate === 1) return;
        const node = state.pitch = new AudioWorkletNode(context, 'artifact-pitch', {
          outputChannelCount: [1], parameterData: {pitch:1, pitchSemitones:0, playbackRate:rate}
        });
        const fail = () => {
          if (stream !== state || state.pitch !== node) return;
          state.audioError = new Error('Audio playback failed. Press Play to resume.');
          reader.cancel().catch(() => {}); state.finish.reject(state.audioError);
        };
        node.onprocessorerror = fail;
        node.port.onmessage = event => { if (event.data?.type === 'error') fail(); };
        node.connect(limiter || context.destination);
        // Keep feeding silence between frames and after EOS to drain the DSP tail.
        state.silence = context.createConstantSource(); state.silence.offset.value = 0;
        state.silence.connect(node); state.silence.start();
      }
      function stopFrame(frame) {
        for (const source of [frame.source, frame.marker]) if (source) {
          source.onended = null; try { source.stop(); } catch (_) {} state.sources.delete(source);
        }
        frame.source = null; frame.marker = null;
      }
      if (!wants && context.state === 'running') context.suspend().catch(() => {});
      function scheduleBuffer(frame, rate, start) {
        const source = context.createBufferSource(); source.buffer = frame.buffer; source.playbackRate.value = rate;
        const gain = context.createGain(); gain.gain.value = Math.pow(10, voiceGain(voiceFor(entry)) / 20); source.connect(gain); gain.connect(state.pitch || limiter || context.destination); frame.source = source; frame.rate = rate; frame.start = start; state.sources.add(source);
        const marker = state.latency ? context.createBufferSource() : source;
        if (marker !== source) {
          marker.buffer = context.createBuffer(1, 1, context.sampleRate);
          marker.connect(context.destination); frame.marker = marker; state.sources.add(marker);
          marker.start(start + (frame.buffer.duration - (frame.offset || 0)) / rate + state.latency);
        }
        marker.onended = () => {
          if (stream !== state || frame.source !== source) return;
          frame.source = null; frame.marker = null; state.sources.delete(source); state.sources.delete(marker);
          if (!frame.played) { frame.played = true; state.played += frame.samples; state.buffered = Math.max(0, state.buffered - frame.samples); }
          box.dataset.playedSamples = String(state.played); box.dataset.bufferedSamples = String(state.buffered);
          if (state.done && !state.sources.size) state.finish.resolve();
        };
        source.start(start, frame.offset || 0);
      }
      function schedule(bytes) {
        if (stream !== state || token !== epoch) return;
        if (bytes.byteLength > 192 * 1024 || bytes.byteLength % 2) throw new Error('Invalid PCM frame.');
        const skip = Math.min(bytes.byteLength / 2, Math.max(0, Math.round(seek * 24000) - state.received));
        state.received += bytes.byteLength / 2;
        const samples = new Int16Array(bytes.buffer, bytes.byteOffset + skip * 2, bytes.byteLength / 2 - skip);
        if (!samples.length) return;
        const buffer = context.createBuffer(1, samples.length, 24000), channel = buffer.getChannelData(0);
        for (let i = 0; i < samples.length; i++) channel[i] = samples[i] / 32768;
        const frame = { buffer, samples: samples.length, source: null, start: 0, rate: state.rate, offset: 0, played: false };
        if (state.started && context.state === 'running' && context.currentTime > state.cursor + 0.005) {
          measurement.underruns++; measurement.maxUnderrunMs = Math.max(measurement.maxUnderrunMs,Math.round((context.currentTime-state.cursor)*1000));
        }
        const start = Math.max(state.cursor, context.currentTime + (state.started ? 0 : startup));
        if (!state.started) {
          const scheduled = performance.now() + Math.max(0,start-context.currentTime+(state.latency || 0))*1000;
          measurement.firstScheduledAudioMs = Math.round(scheduled-measurement.startedAt);
          if (previousAudioEnd !== null) measurement.interChunkGapMs = Math.max(0,Math.round(scheduled-previousAudioEnd));
        }
        measurement.receivedAudioSeconds = Math.round(state.received / 24) / 1000; state.started = true; state.cursor = start + buffer.duration / state.rate;
        state.frames.push(frame); state.total += frame.samples; state.buffered += frame.samples; scheduleBuffer(frame, state.rate, start);
        loaded = index; loading = false; box.dataset.streamState = wants ? 'playing' : 'paused'; box.dataset.bufferedSamples = String(state.buffered); box.dataset.playedSamples = String(state.played);
        status(readingMessage()); paint();
      }
      state.seekTo = function(target) {
        if (target < state.baseOffset) return false;
        resetPitch(state.rate);
        let cursor = context.currentTime + 0.01, offset = state.baseOffset;
        state.played = 0; state.buffered = 0;
        for (const frame of state.frames) {
          stopFrame(frame);
          frame.offset = Math.min(frame.buffer.duration, Math.max(0, target - offset)); offset += frame.buffer.duration;
          frame.played = frame.offset >= frame.buffer.duration;
          if (frame.played) state.played += frame.samples;
          else { state.buffered += frame.samples; scheduleBuffer(frame, state.rate, cursor); cursor += (frame.buffer.duration - frame.offset) / state.rate; }
        }
        state.cursor = cursor; return true;
      };
      state.changeRate = function(rate) {
        const now = context.currentTime;
        for (const frame of state.frames) {
          if (frame.played) continue;
          frame.offset += Math.min(frame.buffer.duration - frame.offset, Math.max(0, now - frame.start - state.latency) * frame.rate);
          stopFrame(frame);
        }
        resetPitch(rate); state.rate = rate;
        let cursor = now + 0.01;
        for (const frame of state.frames) {
          if (frame.played) continue;
          if (frame.offset >= frame.buffer.duration - 0.00001) {
            frame.played = true; state.played += frame.samples; state.buffered -= frame.samples; continue;
          }
          scheduleBuffer(frame, rate, cursor); cursor += (frame.buffer.duration - frame.offset) / rate;
        }
        state.cursor = cursor;
        if (state.done && !state.sources.size) state.finish.resolve();
      };

      try {
        resetPitch(state.rate);
        for (;;) {
          let result;
          try { result = await reader.read(); }
          catch (error) { error.retryable = true; throw error; }
          if (result.done) break;
          const incoming = new Uint8Array(result.value), combined = new Uint8Array(state.pending.length + incoming.length);
          combined.set(state.pending); combined.set(incoming, state.pending.length); state.pending = combined;
          if (state.pending.length > 4 * 1024 * 1024) throw new Error('Streaming audio is too large.');
          while (state.pending.length >= 4) {
            const length = new DataView(state.pending.buffer, state.pending.byteOffset, 4).getUint32(0);
            if (length === 0) { state.done = true; state.pending = state.pending.slice(4); break; }
            if (length > 192 * 1024) throw new Error('Streaming frame is too large.');
            if (state.bytes + length > 4 * 1024 * 1024) throw new Error('Streaming audio is too large.');
            if (state.pending.length < length + 4) break;
            const frame = state.pending.slice(4, length + 4); state.pending = state.pending.slice(length + 4); state.bytes += length;
            if (!timed) schedule(frame);
            else if (frame[0] === 1) schedule(frame.slice(1));
            else if (frame[0] === 2) acceptWord(frame.slice(1));
            else throw new Error('Invalid timed stream frame.');
          }
          if (state.done) { if (state.pending.length) throw new Error('Trailing data after streaming terminator.'); await reader.cancel(); break; }
        }
        if (state.audioError) throw state.audioError;
        if (!state.done || state.pending.length || !state.received) throw Object.assign(new Error('Audio was interrupted. Press Play to resume.'), {retryable:true});
        loading = false;
        // Generation is finished, but audio is still playing: give the next
        // paragraph that remaining playback time to prepare its first frames.
        if (token === epoch) { aheadFrom = index; fillAhead(); }
        if (!state.sources.size) state.finish.resolve();
        await state.finished;
        if (stream !== state || token !== epoch) return true;
        previousAudioEnd = performance.now(); measurement.completed = true;
        savePlace(); durations.set(index, state.received / 24000); cache.delete(index); position++; clearStream();
        if (wants) loadCurrent();
        return true;
      } catch (error) {
        if (token !== epoch || signal.aborted) return true;
        // Capture what was heard, not how much audio arrived over the network.
        const offset = mediaOffset(); savePlace();
        clearStream(); pendingSeek = offset;
        throw error;
      }
    }
    async function playLoaded() {
      if (captureActive) return;
      audio.volume = Math.min(1, Math.pow(10, voiceGain(voiceFor(queue[position])) / 20));
      const pause = pauseBefore(queue[position], queue[position - 1], position === 0 && continuationPause); if (position === 0) continuationPause = false;
      if (pause && !audio.currentTime) await new Promise(resolve => { pauseTimer = setTimeout(() => { pauseTimer = null; resolve(); }, pause); });
      if (!wants || captureActive) return;
      try { await audio.play(); }
      catch (_) { wants = false; status('Press Play to start audio.'); paint(); }
    }
    async function loadCurrent(retry = 0) {
      if (captureActive || loading) return;
      if (checkSleep()) return; armSleep();
      const token = epoch, index = position;
      if (previewEnd !== null && index >= previewEnd) {
        const restart = previewStart; stop('Preview finished. Play to continue reading.', true); position = restart; return;
      }
      if (!mayContinue(index) || (index >= queue.length && ['section','chapter'].includes(get('sleep').value))) { stop('Sleep timer ended. Your place is saved.'); cancelSleep(); return; }
      if (index >= queue.length) {
        if (chapter && chapter.hasNext && ['page','here'].includes(mode)) { requestContent(true); return; }
        const finishedKey = queueKey; stop('Finished'); try { if (finishedKey) localStorage.removeItem(finishedKey); } catch (_) {} if (finishedKey) placeSync.clear(pageOfKey(finishedKey)); refreshSaved(); return;
      }
      playbackConfig = {voice, mode, style:get('style').value, custom:get('custom').value};
      loading = true; send('reader:highlight', {id:queue[index].id}); status('Preparing audio…'); paint();
      try {
        if (streamingVoice(queue[index]) && ensureAudioContext()) {
          const streamed = await streamCurrent(index);
          if (streamed) return;
        }
        const buffer = await synth(index);
        if (token !== epoch) return;
        loading = false;
        if (currentUrl) URL.revokeObjectURL(currentUrl);
        currentUrl = URL.createObjectURL(new Blob([buffer], { type: 'audio/wav' }));
        const seek = pendingSeek; pendingSeek = 0;
        audio.onloadedmetadata = () => { if (token !== epoch) return; durations.set(index, audio.duration); audio.currentTime = Math.min(seek, Math.max(0, audio.duration - 0.01)); };
        audio.src = currentUrl; audio.preservesPitch = true; audio.playbackRate = Number(get('rate').value); loaded = index;
        audio.onended = () => { if (token !== epoch) return; savePlace(); cache.delete(position); position++; loaded = -1; if (wants) loadCurrent(); };
        send('reader:highlight', { id: queue[index].id });
        status(readingMessage());
        paint(); if (wants) await playLoaded();
        // Only begin read-ahead after the current synthesis completes.
        if (token === epoch) { aheadFrom = index; fillAhead(); }
      } catch (error) {
        if (token !== epoch) return;
        if (error.retryable && retry < 2 && wants) {
          loading = true; status('Audio interrupted. Reconnecting…'); paint();
          try { await delay(750 * (retry + 1), controller.signal); }
          catch (_) { return; }
          if (token !== epoch) return;
          loading = false;
          if (wants) return loadCurrent(retry + 1);
          status('Paused. Press Play to resume.'); paint(); return;
        }
        loading = false; wants = false; failure = failureReason(error); savePlace(); placeSync.flush(); status(error.message || 'Speech is unavailable. Press Play to resume.'); paint();
      }
    }
    function requestContent(nextChapter, preview = false) {
      if (captureActive) return;
      const scopeKey = !nextChapter && ['view','detail','section'].includes(mode) ? restartScopeKey : '';
      const intended = wants; stop(nextChapter ? 'Opening next chapter…' : 'Reading page…'); wants = intended; previewRequested = preview; continuationPause = !!nextChapter;
      extraction = 'read-' + (++serial);
      send(nextChapter ? 'reader:next' : 'reader:extract', { requestId: extraction, mode: nextChapter ? 'page' : mode, scopeKey });
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not read this page. Press Play to retry.', false, 'Could not read this page'); }, 5000); paint();
    }
    function placePanel() { get('panel').style.top = innerWidth <= 760 ? (document.querySelector('.vbar').getBoundingClientRect().bottom + 6) + 'px' : ''; }
    get('toggle').onclick = () => { setPlayerView('compact'); get('mini-play').focus(); };
    get('minimize').onclick = () => { setPlayerView('compact'); get('expand').focus(); };
    get('expand').onclick = () => { setPlayerView('full'); get('minimize').focus(); };
    get('mini-play').onclick = () => get('play').click();
    get('mini-resume').onclick = () => get('resume').click();
    get('mini-rewind').onclick = () => get('rewind').click();
    get('mini-next').onclick = () => get('next').click();
    get('mini-close').onclick = () => { stop(); cancelSleep(); setPlayerView('closed'); get('toggle').focus(); };
    get('outline').onchange = paint;
    get('jump').onclick = () => {
      const value = get('outline').value, detailMatch = /^detail:(\d+)$/.exec(value), match = /^(section|chapter|detail):(\d+)$/.exec(value);
      if (detailMatch && detailTargets[Number(detailMatch[1])]) { readDetails(detailTargets[Number(detailMatch[1])]); return; }
      if (!match) return;
      jumpTo(match[1] === 'chapter' ? {chapterIndex:Number(match[2])} : {ordinal:Number(match[2]),fingerprint:outlineFingerprint});
    };
    get('inline-read').onclick = get('target-read').onclick = get('mini-target-read').onclick = event => {
      if (clickedTarget) { jumpTo({ordinal:clickedTarget.ordinal,fingerprint:clickedTarget.fingerprint}); if (event.currentTarget === get('inline-read')) get(compact ? 'mini-play' : 'play').focus(); }
    };
    function readDetails(target) {
      if (captureActive) return;
      if (!target || typeof target.scopeKey !== 'string' || !target.scopeKey || target.scopeKey.length > 500) return;
      stop('Opening details…'); mode = 'detail'; get('mode').value = mode; wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null; if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial);
      send('reader:extract', {requestId:extraction, mode:'detail', scopeKey:target.scopeKey});
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not read these details.', false, 'Could not read these details'); }, 5000); paint();
    }
    get('target-details').onclick = () => { if (clickedTarget?.detailAvailable) readDetails(clickedTarget); };
    get('target-dismiss').onclick = get('mini-target-dismiss').onclick = clearTarget;
    if (commentToggle) new MutationObserver(() => { pickMode(); if (commentMode) clearTarget(); paint(); }).observe(commentToggle, {attributes:true,attributeFilter:['aria-pressed']});
    new ResizeObserver(() => { placePanel(); placeTargetAction(); }).observe(document.querySelector('.vbar'));
    window.addEventListener('resize', placeTargetAction);
    document.addEventListener('keydown', event => { if (event.key === 'Escape' && !get('panel').hidden) { setPlayerView(queue.length || extraction ? 'compact' : 'closed'); get('toggle').focus(); } });
    get('play').onclick = () => {
      if (captureActive) return;
      try { if (navigator.audioSession) navigator.audioSession.type = 'playback'; } catch (_) {}
      if (checkSleep()) return; armSleep();
      if (wants) { savePlace(); placeSync.flush(); wants = false; audio.pause(); if (audioContext && audioContext.state === 'running' && streamingVoice()) audioContext.suspend().catch(() => {}); if (stream) { stream.boxState = 'paused'; box.dataset.streamState = 'paused'; } status(readingMessage()); paint(); return; }
      wants = true; if (loaded >= 0 || stream) status(readingMessage()); paint();
      if (streamingVoice()) { const context = ensureAudioContext(); if (context && context.state === 'suspended') context.resume().catch(() => {}); }
      if (stream && audioContext && audioContext.state === 'suspended') audioContext.resume().catch(() => {});
      if (stream) { stream.boxState = 'playing'; box.dataset.streamState = 'playing'; return; }
      if (extraction || loading) return;
      // Nothing loaded yet: continue from the saved place when there is one (phone, browser, or car).
      // Only for the same whole-page or section reading; an explicit view, detail, or "here" pick wins.
      // A saved place from another device may still be loading; wait for it briefly (ADR-0022).
      if (!queue.length && ['page','section'].includes(mode) && !placeSync.settled()) { paint(); placeSync.ready.then(() => { if (wants && !queue.length && !extraction && !loading) startFromSaved(); }); return; }
      if (!queue.length) startFromSaved(); else if (loaded === position) playLoaded(); else loadCurrent();
    };
    function startFromSaved() { if (['page','section'].includes(mode) && savedPlace()?.mode === mode) get('resume').click(); else requestContent(false); }
    get('stop').onclick = () => { stop(); cancelSleep(); };
    get('sleep').onchange = () => { sleepDeadline = 0; sleepBoundary = null; if (wants || loaded >= 0) armSleep(); checkSleep(); };
    get('replay').onclick = () => {
      const start = replaySentenceStart();
      if (start === null || !stream?.seekTo(start)) return;
      stream.highlightedWord = -1;
      wants = true;
      if (audioContext?.state === 'suspended') audioContext.resume().catch(() => {});
      box.dataset.streamState = 'playing';
      status(readingMessage()); savePlace(); paint();
    };
    get('rewind').onclick = () => {
      let target = position, offset = mediaOffset() - 15;
      while (offset < 0 && target > 0 && durations.has(target - 1)) offset += durations.get(--target);
      offset = Math.max(0, offset);
      if (target === position && stream?.seekTo(offset)) { savePlace(); return; }
      if (target === position && !stream && loaded === position) { audio.currentTime = offset; savePlace(); return; }
      const playing = wants; stop('Rewinding…', true); position = target; pendingSeek = offset; wants = playing; loadCurrent();
    };
