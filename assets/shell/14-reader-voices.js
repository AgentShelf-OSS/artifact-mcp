    // ADR-0020: with many voices, search and filter the list by catalog tags; saved voices are
    // kept in this browser for every artifact.
    const finder = {query:'', filter:'all', accent:''};
    const FAVORITES_KEY = 'artifact-reader-favorite-voices';
    function favoriteVoices() { try { const list = JSON.parse(localStorage.getItem(FAVORITES_KEY) || '[]'); return Array.isArray(list) ? list.filter(v => typeof v === 'string').slice(0, 200) : []; } catch (_) { return []; } }
    function setFavorites(list) { try { localStorage.setItem(FAVORITES_KEY, JSON.stringify(list.slice(0, 200))); } catch (_) {} }
    function voiceMatches(item, favorites) {
      const tags = item.tags || {};
      if (finder.filter === 'female' && tags.gender !== 'female') return false;
      if (finder.filter === 'male' && tags.gender !== 'male') return false;
      if (finder.filter === 'saved' && !favorites.includes(item.id)) return false;
      if (finder.accent && tags.accent !== finder.accent) return false;
      const q = finder.query.trim().toLowerCase();
      return !q || [item.name, tags.accent, tags.style, tags.gender].filter(Boolean).join(' ').toLowerCase().includes(q);
    }
    // One builder for every voice menu (main voice and character roles), so they always offer the
    // same voices, grouped and labelled the same way: saved voices first, then each engine, with
    // the Pocket list alphabetical.
    function voiceGroupLabel(item) { return item.provider || (/pocket/i.test(item.id + ' ' + item.name) ? 'Pocket TTS' : /^raven_/.test(item.id) ? 'RAVEN trial' : /qwen/i.test(item.id + ' ' + item.name) ? 'Qwen3-TTS' : /^moss_/.test(item.id) ? 'MOSS-TTS-Nano' : 'Kokoro'); }
    function voiceOptionLabel(item) { const tags = item.tags || {}, details = [tags.accent, tags.style].filter(Boolean).join(', '); return item.name.replace(/ · (Kokoro|Pocket|RAVEN|Qwen|MOSS(?:-TTS Nano)?)(?: TTS)?/i, '') + (details ? ' (' + details + ')' : ''); }
    function appendVoiceOptions(select, options) {
      const opts = options || {}, favorites = favoriteVoices(), groups = new Map(), saved = [];
      let shown = 0;
      for (const item of availableVoices) {
        const keep = item.id === opts.current;
        if (!keep && opts.filter && !opts.filter(item, favorites)) continue;
        if (!keep || !opts.filter || opts.filter(item, favorites)) shown++;
        if (favorites.includes(item.id)) { saved.push(item); continue; }
        const label = voiceGroupLabel(item); if (!groups.has(label)) groups.set(label, []); groups.get(label).push(item);
      }
      const add = (label, items) => { if (!items.length) return; const group = document.createElement('optgroup'); group.label = label; items.forEach(item => group.appendChild(new Option(voiceOptionLabel(item), item.id))); select.appendChild(group); };
      add('Saved voices', saved.sort((a, b) => favorites.indexOf(a.id) - favorites.indexOf(b.id)));
      groups.forEach((items, label) => add(label, label === 'Pocket TTS' ? items.slice().sort((a, b) => voiceOptionLabel(a).localeCompare(voiceOptionLabel(b))) : items));
      return shown;
    }
    function renderVoiceOptions(keep) {
      const select = get('voice'), current = keep || select.value;
      select.textContent = '';
      const shown = appendVoiceOptions(select, {current, filter: voiceMatches});
      if (current && Array.from(select.options).some(option => option.value === current)) select.value = current;
      const count = get('voice-count'); if (count) count.textContent = shown + ' of ' + availableVoices.length + ' voices';
      const save = get('voice-save'); if (save) { const savedNow = favoriteVoices().includes(select.value); save.textContent = savedNow ? 'Saved voice' : 'Save this voice'; save.setAttribute('aria-pressed', String(savedNow)); }
      refreshRoleOptions();
    }
    // Character role menus follow the same list; rebuilt when saved voices change.
    function refreshRoleOptions() {
      const rows = get('cast-roles'); if (!rows) return;
      rows.querySelectorAll('select[data-role]').forEach(select => { const value = select.value; select.textContent = ''; select.appendChild(new Option('Main voice', '')); appendVoiceOptions(select, {current: value}); select.value = value; });
    }
    function setupFinder() {
      const box2 = get('finder'); if (!box2) return;
      box2.hidden = availableVoices.length < 12;
      const accents = Array.from(new Set(availableVoices.map(v => v.tags && v.tags.accent).filter(Boolean))).sort();
      const accentSelect = get('voice-accent'); accentSelect.hidden = !accents.length;
      accentSelect.innerHTML = '<option value="">Any accent</option>' + accents.map(a => '<option></option>').join('');
      accents.forEach((a, i) => { accentSelect.options[i + 1].value = a; accentSelect.options[i + 1].textContent = a; });
      if (box2.dataset.wired) return; box2.dataset.wired = '1';
      get('voice-search').addEventListener('input', e => { finder.query = e.target.value; renderVoiceOptions(); });
      accentSelect.addEventListener('change', e => { finder.accent = e.target.value; renderVoiceOptions(); });
      box2.querySelector('.vreader-chips').addEventListener('click', e => { const b = e.target.closest('[data-voice-filter]'); if (!b) return; finder.filter = b.dataset.voiceFilter; box2.querySelectorAll('[data-voice-filter]').forEach(x => x.setAttribute('aria-pressed', String(x === b))); renderVoiceOptions(); });
      get('voice-save').addEventListener('click', () => { const id = get('voice').value; if (!id) return; const list = favoriteVoices(); const at = list.indexOf(id); if (at >= 0) list.splice(at, 1); else list.unshift(id); setFavorites(list); renderVoiceOptions(); });
    }
    function loadVoices() { return fetch('/' + encodeURIComponent(artifactId) + '/speech/voices').then(r => r.ok ? r.json() : null).then(data => {
      if (!data || !data.enabled || !Array.isArray(data.voices) || !data.voices.length) { box.hidden = true; return; }
      availableVoices = data.voices.filter(item => item && typeof item.id === 'string');
      voiceGains = new Map(availableVoices.filter(item => Number.isFinite(item.gainDb)).map(item => [item.id, item.gainDb]));
      if (pendingCast) renderCast(pendingCast);
      renderVoiceOptions(savedVoice); setupFinder();
      if (Array.from(get('voice').options).some(option => option.value === savedVoice)) get('voice').value = savedVoice;
      voice = get('voice').value; enabled = true; get('toggle').disabled = false; placeSync.load(); updateStyle(); send('reader:hello'); paint();
    }).catch(() => { box.hidden = true; }); }
    get('toggle').disabled = true; box.hidden = false; get('status').textContent = 'Loading voices…';
    paint();
    function setCaptureActive(active) {
      active = active === true;
      if (captureActive === active) return;
      captureActive = active;
      if (active) {
        stop('Reading paused while casting. Your place is saved.', true);
        box.inert = true;
      } else {
        box.inert = false;
        paint();
      }
    }
    return { setCaptureActive, loadVoices };
  }
  nativeReader=createNativeReader();
  function scheduleOptionalViewerWork(callback){
    if(typeof window.requestIdleCallback==='function'){window.requestIdleCallback(callback,{timeout:2000});return;}
    setTimeout(callback,0);
  }
  scheduleOptionalViewerWork(function(){if(nativeReader)nativeReader.loadVoices();});
