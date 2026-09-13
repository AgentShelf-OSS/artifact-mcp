(function () {
  'use strict';
  if (window.parent === window) return;
  const MAX_CHARS = 500000, MAX_BLOCKS = 5000;
  const excluded = 'script,style,noscript,template,nav,form,input,button,select,textarea,label,[hidden],[aria-hidden="true"],[data-artifact-readable="false"],[role="navigation"],[role="toolbar"]';
  const blockTags = new Set('H1 H2 H3 H4 H5 H6 P LI DT DD BLOCKQUOTE PRE FIGCAPTION CAPTION TD TH DIV SECTION ARTICLE MAIN BODY HEADER FOOTER'.split(' '));
  const ids = new WeakMap();
  let snapshot = new Map(), active = null, signature = null, timer, selection = '', selectionRange = null, playbackSelectionRange = null, point = null, pickMode = false, pickedElement = null, targetFrame = 0;
  const post = message => window.parent.postMessage(message, '*');
  const wordSources = new Map();
  const normalize = text => text.replace(/\s+/g, ' ').trim();
  const style = document.createElement('style');
  style.textContent = '[data-artifact-reader-target]{outline:1px dashed #9b681f!important;outline-offset:4px;background-color:rgba(181,107,44,.18)!important;border-radius:2px} [data-artifact-reader-active]{outline:2px solid #b56b2c!important;outline-offset:3px;background-color:rgba(181,107,44,.12)!important} ::highlight(artifact-reader-passage){background-color:rgba(181,107,44,.24);color:inherit} ::highlight(artifact-reader-word){background-color:rgba(255,194,74,.75);color:inherit;border-radius:2px}';
  (document.head || document.documentElement).appendChild(style);
  function allowed(element) {
    if (!element || element.closest(excluded)) return false;
    for (let node = element; node; node = node.parentElement) {
      const css = getComputedStyle(node);
      if (css.display === 'none' || css.visibility === 'hidden' || css.visibility === 'collapse') return false;
    }
    return true;
  }
  function scanRoot(root, range, mapPositions = false) {
    const groups = []; let current = null, chars = 0, truncated = false;
    function append(text, owner, node, start) {
      if (current && current.element === owner) current.text += text;
      else { current = { element: owner, text, positions:[] }; groups.push(current); }
      if (mapPositions) for (let i = 0; i < text.length; i++) current.positions.push({node, offset:start + i});
      chars += text.length;
    }
    function walk(node, owner) {
      if (chars >= MAX_CHARS || groups.length >= MAX_BLOCKS) { truncated = true; return; }
      if (node.nodeType === Node.TEXT_NODE) {
        if (!range || range.intersectsNode(node)) {
          let start = range && range.startContainer === node ? range.startOffset : 0;
          let end = range && range.endContainer === node ? range.endOffset : node.length;
          const text = node.data.slice(start, end);
          if (text.length > MAX_CHARS - chars) truncated = true;
          append(text.slice(0, MAX_CHARS - chars), owner, node, start);
        }
      } else if (node.nodeType === Node.ELEMENT_NODE && allowed(node)) {
        const isBlock = blockTags.has(node.tagName) || ['block', 'flex', 'grid', 'table-row'].includes(getComputedStyle(node).display);
        if (isBlock) current = null;
        for (const child of node.childNodes) walk(child, isBlock ? node : owner);
        if (node.tagName === 'BR' && current) { current.text += ' '; if (mapPositions) current.positions.push(null); }
        if (isBlock) current = null;
      }
    }
    walk(root, root);
    return { groups: groups.map(g => {
      if (!mapPositions) return {element:g.element, text:normalize(g.text)};
      let text = '', pending = false; const positions = [];
      for (let i = 0; i < g.text.length; i++) {
        if (/\s/.test(g.text[i])) { if (text) pending = true; continue; }
        if (pending) { text += ' '; positions.push(null); pending = false; }
        text += g.text[i]; positions.push(g.positions[i]);
      }
      return {element:g.element, text, positions};
    }).filter(g => g.text), truncated };
  }
  function scan(range) {
    if (range) return scanRoot(document.body, range);
    const adapted = window.__artifactEreader && window.__artifactEreader.scan();
    if (adapted) {
      let total = 0;
      const groups = adapted.groups.slice(0, MAX_BLOCKS).map(g => { const text = g.text.slice(0, Math.max(0, MAX_CHARS - total)); total += text.length; return { element: g.element, text }; }).filter(g => g.text);
      return Object.assign({}, adapted, { groups, truncated: total >= MAX_CHARS });
    }
    for (const root of document.querySelectorAll('article,main')) {
      if (!allowed(root)) continue;
      const found = scanRoot(root);
      if (found.groups.length) return found;
    }
    return scanRoot(document.body || document.documentElement);
  }
  function fingerprintText(value) {
    let hash = 2166136261;
    for (let i = 0; i < value.length; i++) { hash ^= value.charCodeAt(i); hash = Math.imul(hash, 16777619); }
    return (hash >>> 0).toString(16).padStart(8, '0') + ':' + value.length;
  }
  function fingerprint(found) {
    const adapted = window.__artifactEreader && window.__artifactEreader.fingerprint;
    const visibleText = found.groups.map(g => g.text).join('\n');
    if (typeof adapted === 'function') return fingerprintText(adapted() + '|' + visibleText);
    return typeof adapted === 'string' ? fingerprintText(adapted + '|' + visibleText) : fingerprintText(visibleText);
  }
  function isHeading(element) {
    return !!(element && (/^H[1-6]$/.test(element.tagName) || element.classList && element.classList.contains('ch-head')));
  }
  function sectionSlice(blocks) {
    if (!blocks.length) return blocks;
    let anchor = point ? blocks.findIndex(b => snapshot.get(b.id) === point || snapshot.get(b.id).contains && snapshot.get(b.id).contains(point)) : -1;
    if (anchor < 0) anchor = blocks.findIndex(b => { const el = snapshot.get(b.id), r = el && el.getBoundingClientRect(); return r && r.bottom > 0 && r.top < innerHeight; });
    if (anchor < 0) anchor = 0;
    const meta = sectionMeta(blocks, anchor);
    return blocks.slice(meta.start, meta.end + 1);
  }
  function sectionMeta(blocks, ordinal) {
    const element = snapshot.get(blocks[ordinal].id);
    const semantic = element && element.closest && element.closest('section');
    let start = ordinal, end = ordinal;
    if (semantic) {
      while (start > 0 && semantic.contains(snapshot.get(blocks[start - 1].id))) start--;
      while (end + 1 < blocks.length && semantic.contains(snapshot.get(blocks[end + 1].id))) end++;
    } else {
      while (start > 0 && !isHeading(snapshot.get(blocks[start].id))) start--;
      if (!isHeading(snapshot.get(blocks[start].id))) start = 0;
      end = start + 1;
      while (end < blocks.length && !isHeading(snapshot.get(blocks[end].id))) end++;
      end--;
    }
    const heading = snapshot.get(blocks[start].id);
    return { start, end, label: isHeading(heading) ? normalize(heading.innerText || heading.textContent || '') : null };
  }
  function content(mode, full) {
    wordSources.clear();
    if (mode === 'selection') { playbackSelectionRange = selectionRange?.cloneRange() || null; signature = scan().groups.map(g => g.text).join('\n'); return { blocks: selection ? [{ id: 'selection', text: selection }] : [], truncated: selection.length >= MAX_CHARS }; }
    const found = scan(); snapshot = new Map();
    let blocks = found.groups.map((group, ordinal) => {
      const id = 'r' + ordinal; snapshot.set(id, group.element);
      return { id, ordinal, text: group.text };
    });
    blocks = blocks.map((block, ordinal) => { const meta = sectionMeta(blocks, ordinal); return Object.assign(block, { sectionEndOrdinal: meta.end, sectionLabel: meta.label }); });
    if (mode === 'section' && !full) blocks = sectionSlice(blocks);
    if (mode === 'here') {
      let index = point ? blocks.findIndex(b => { const el = snapshot.get(b.id); return el === point || el.contains(point); }) : -1;
      if (index < 0) index = blocks.findIndex(b => { const r = snapshot.get(b.id).getBoundingClientRect(); return r.bottom > 0 && r.top < innerHeight && r.right > 0 && r.left < innerWidth; });
      blocks = blocks.slice(Math.max(0, index));
    }
    signature = found.groups.map(g => g.text).join('\n');
    return { blocks, truncated: found.truncated, chapter: found.chapter || null, fingerprint: fingerprint(found), label: found.chapter && found.chapter.label || null };
  }
  function outline() {
    const savedSnapshot = snapshot, found = scan();
    const rows = found.groups.map((group, ordinal) => ({ id:'r' + ordinal, ordinal, element:group.element, text:group.text }));
    const sections = [], seen = new Set();
    try {
      snapshot = new Map(rows.map(row => [row.id, row.element]));
      for (const row of rows) {
        const start = isHeading(row.element) ? row.ordinal : sectionMeta(rows, row.ordinal).start;
        if (!seen.has(start)) { seen.add(start); sections.push({ordinal:start,label:normalize(rows[start].text || 'Page text').slice(0,120)}); }
      }
    } finally { snapshot = savedSnapshot; }
    // Observe navigation even when Listen is opened before the first playback.
    if (signature === null) signature = found.groups.map(group => group.text).join('\n');
    const adapted = window.__artifactEreader;
    return {fingerprint:fingerprint(found),sections:sections.sort((a,b)=>a.ordinal-b.ordinal).slice(0,500),chapters:adapted && adapted.outline ? adapted.outline().chapters.slice(0,500) : [],chapter:found.chapter || null};
  }
  function highlight(id) {
    if (active) active.removeAttribute('data-artifact-reader-active');
    clearWordHighlight();
    if (id === null) { clearPassageHighlight(); playbackSelectionRange = null; wordSources.clear(); }
    else if (id === 'selection' && playbackSelectionRange) cssHighlight('artifact-reader-passage', playbackSelectionRange);
    else clearPassageHighlight();
    active = snapshot.get(id) || null;
    if (active) {
      active.setAttribute('data-artifact-reader-active', '');
      const box = active.getBoundingClientRect();
      if (box.top < 0 || box.bottom > innerHeight) active.scrollIntoView({ block: 'center' });
    }
  }
  function cssHighlight(name, range) {
    if (typeof CSS === 'undefined' || typeof CSS.highlights?.set !== 'function' || typeof Highlight !== 'function') return false;
    if (range) CSS.highlights.set(name, new Highlight(range)); else CSS.highlights.delete(name);
    return true;
  }
  function clearWordHighlight() { cssHighlight('artifact-reader-word', null); }
  function clearPassageHighlight() { cssHighlight('artifact-reader-passage', null); }
  function mappedSource(id) {
    if (wordSources.has(id)) return wordSources.get(id);
    const root = id === 'selection' ? document.body : snapshot.get(id);
    if (!root || !root.isConnected || id === 'selection' && !playbackSelectionRange) return null;
    const groups = scanRoot(root, id === 'selection' ? playbackSelectionRange : null, true).groups;
    const source = {text:'', positions:[]};
    for (const group of groups) {
      if (source.text) { source.text += ' '; source.positions.push(null); }
      source.text += group.text; for (const position of group.positions) source.positions.push(position);
    }
    wordSources.set(id, source); return source;
  }
  function wordRange(id, text, offset, start, end) {
    if (typeof text !== 'string' || text.length > 3000 || !Number.isInteger(offset) ||
        !Number.isInteger(start) || !Number.isInteger(end) || offset < 0 || start < 0 || end <= start || end > text.length) return null;
    const source = mappedSource(id);
    if (!source || source.text.slice(offset, offset + text.length) !== text) return null;
    const first = source.positions[offset + start], last = source.positions[offset + end - 1];
    if (!first?.node?.isConnected || !last?.node?.isConnected) return null;
    const range = document.createRange(); range.setStart(first.node, first.offset); range.setEnd(last.node, last.offset + 1); return range;
  }
  function highlightWord(data) {
    let range;
    try { range = wordRange(data.id, data.text, data.offset, data.start, data.end); } catch (_) {}
    if (!range) { highlight(data.id); return false; }
    const applied = cssHighlight('artifact-reader-word', range);
    if (!applied) return false;
    if (active) active.removeAttribute('data-artifact-reader-active'); active = null;
    clearPassageHighlight(); targetResize.disconnect(); clearPicked();
    if (applied && data.id === 'selection') { const current = getSelection(); if (current) current.removeAllRanges(); }
    return applied;
  }
  function clearPicked() {
    if (pickedElement) pickedElement.removeAttribute('data-artifact-reader-target');
    pickedElement = null;
  }
  function targetPosition() {
    targetFrame = 0;
    if (!pickedElement || !pickedElement.isConnected || !allowed(pickedElement)) { clearPicked(); post({type:'reader:target-position',rect:null}); return; }
    const rect = pickedElement.getBoundingClientRect();
    post({type:'reader:target-position',rect:{left:rect.left,right:rect.right,top:rect.top,bottom:rect.bottom},viewport:{width:innerWidth,height:innerHeight}});
  }
  function scheduleTargetPosition() { if (pickedElement && !targetFrame) targetFrame = requestAnimationFrame(targetPosition); }
  // Track nested scrollers too; position updates are coalesced to one animation frame.
  document.addEventListener('scroll', scheduleTargetPosition, true);
  window.addEventListener('resize', scheduleTargetPosition);
  const targetResize = new ResizeObserver(scheduleTargetPosition);
  function pick(element) {
    targetResize.disconnect(); clearPicked(); pickedElement = element;
    element.setAttribute('data-artifact-reader-target',''); targetResize.observe(element); targetPosition();
  }
  function rememberSelection() {
    const value = getSelection();
    if (value && !value.isCollapsed && value.rangeCount) {
      selectionRange = value.getRangeAt(0).cloneRange();
      selection = scan(value.getRangeAt(0)).groups.map(g => g.text).join(' ').slice(0, MAX_CHARS);
    } else if (document.hasFocus()) { selection = ''; selectionRange = null; clearPassageHighlight(); }
  }
  document.addEventListener('selectionchange', rememberSelection);
  document.addEventListener('pointerdown', event => { point = event.target; }, true);
  document.addEventListener('click', event => {
    if (!pickMode || event.button !== 0 || getSelection() && !getSelection().isCollapsed) return;
    if (event.target.isContentEditable || event.target.closest('a,button,input,select,textarea,label,summary,form,nav,[role=button],[role=link],[role=tab],[role=menuitem]')) return;
    const target = event.target.closest && event.target.closest('h1,h2,h3,h4,h5,h6,p,li,blockquote,pre,figcaption,caption,td,dd,dt');
    if (!target || !allowed(target) || target.closest('a,button,form,nav,[contenteditable="true"]')) return;
    const found = scan(), ordinal = found.groups.findIndex(group => group.element === target || group.element.contains && group.element.contains(target));
    if (ordinal >= 0) { post({ type: 'reader:target', ordinal, fingerprint: fingerprint(found), label: normalize(target.innerText || target.textContent || '').slice(0, 160) }); pick(found.groups[ordinal].element); }
  }, true);
  // Retain the viewer helper when the anchor bridge follows an internal bundle link.
  document.addEventListener('click', event => {
    const link = event.target.closest && event.target.closest('a[href]');
    if (!link || link.target && link.target !== '_self') return;
    try {
      const url = new URL(link.href, location.href), parts = location.pathname.split('/');
      const prefix = parts.slice(0, 3).join('/') + '/';
      if (parts[1] === 'raw' && url.origin === location.origin && url.pathname.startsWith(prefix)) {
        url.searchParams.set('reader', '1'); link.href = url.href;
      }
    } catch (_) {}
  }, true);
  window.addEventListener('message', event => {
    const data = event.data;
    if (event.source !== window.parent || !data || typeof data !== 'object' || Array.isArray(data)) return;
    if (data.type === 'reader:hello') { post({ type: 'reader:ready', version: 1 }); return; }
    if (data.type === 'reader:highlight') { if (data.id === null || typeof data.id === 'string' && data.id.length <= 80) { highlight(data.id); } return; }
    if (data.type === 'reader:word') { if (typeof data.id === 'string' && data.id.length <= 80) highlightWord(data); return; }
    if (data.type === 'reader:pick-mode') { pickMode = data.enabled === true; if (!pickMode) { targetResize.disconnect(); clearPicked(); } return; }
    if (data.type === 'reader:clear-target') { targetResize.disconnect(); clearPicked(); return; }
    if (data.type === 'reader:outline' && typeof data.requestId === 'string' && data.requestId.length <= 80) { try { post(Object.assign({ type: 'reader:outline', requestId: data.requestId }, outline())); } catch (_) { post({ type: 'reader:error', requestId: data.requestId, message: 'Could not build the reader outline.' }); } return; }
    if (!['reader:extract', 'reader:next', 'reader:resume', 'reader:jump'].includes(data.type) || typeof data.requestId !== 'string' || !data.requestId || data.requestId.length > 80) return;
    if (!['page', 'selection', 'here', 'section'].includes(data.mode)) { post({ type: 'reader:error', requestId: data.requestId, message: 'Invalid reading mode.' }); return; }
    if (data.type === 'reader:resume' && (typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || (data.chapterIndex !== undefined && (!Number.isInteger(data.chapterIndex) || data.chapterIndex < 0)))) { post({ type: 'reader:error', requestId: data.requestId, message: 'Invalid resume state.' }); return; }
    if (data.type === 'reader:jump' && (!['here','page'].includes(data.mode) || data.mode === 'here' && (typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || !Number.isInteger(data.ordinal) || data.ordinal < 0 || data.chapterIndex !== undefined) || data.mode === 'page' && (!Number.isInteger(data.chapterIndex) || data.chapterIndex < 0 || data.fingerprint !== undefined))) { post({ type: 'reader:error', requestId: data.requestId, message: 'Invalid reader jump.' }); return; }
    if (data.type === 'reader:next') {
      if (!window.__artifactEreader || !window.__artifactEreader.next()) { post({ type: 'reader:error', requestId: data.requestId, message: 'No next chapter is available.' }); return; }
    }
    if ((data.type === 'reader:resume' || data.type === 'reader:jump') && Number.isInteger(data.chapterIndex)) {
      if (!window.__artifactEreader || !window.__artifactEreader.seek || !window.__artifactEreader.seek(data.chapterIndex)) { post({ type: 'reader:error', requestId: data.requestId, message: 'Could not restore this chapter.' }); return; }
    }
    const respond = () => {
      try {
        const found = content(data.type === 'reader:resume' || data.type === 'reader:jump' ? 'page' : data.mode, data.type === 'reader:resume');
        if (data.type === 'reader:jump' && data.mode === 'here') {
          if (found.fingerprint !== data.fingerprint || data.ordinal >= found.blocks.length) { post({ type: 'reader:error', requestId: data.requestId, message: 'The saved reader position is no longer available.' }); return; }
          found.blocks = found.blocks.slice(data.ordinal);
        }
        if (data.type === 'reader:resume' && (found.fingerprint !== data.fingerprint || Number.isInteger(data.chapterIndex) && (!found.chapter || found.chapter.index !== data.chapterIndex)) || data.type === 'reader:jump' && data.mode === 'here' && found.fingerprint !== data.fingerprint || data.type === 'reader:jump' && data.mode === 'page' && (!found.chapter || found.chapter.index !== data.chapterIndex)) { post({ type: 'reader:error', requestId: data.requestId, message: 'The readable content or chapter has changed since this position was saved.' }); return; }
        post(Object.assign({ type: 'reader:content', requestId: data.requestId, mode: data.mode }, found));
      } catch (_) { post({ type: 'reader:error', requestId: data.requestId, message: 'Could not read this page.' }); }
    };
    if ((data.type === 'reader:resume' || data.type === 'reader:jump') && Number.isInteger(data.chapterIndex)) setTimeout(respond, 0); else respond();
  });
  new MutationObserver(() => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (signature === null) return;
      const next = scan().groups.map(g => g.text).join('\n');
      if (next !== signature) { targetResize.disconnect(); clearPicked(); signature = next; selection = ''; selectionRange = null; playbackSelectionRange = null; wordSources.clear(); clearPassageHighlight(); highlight(null); snapshot.clear(); post({ type: 'reader:changed', version: 1 }); }
    }, 160);
  }).observe(document.documentElement, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['hidden', 'aria-hidden', 'class', 'style', 'data-artifact-readable'] });
  post({ type: 'reader:ready', version: 1 });
})();
