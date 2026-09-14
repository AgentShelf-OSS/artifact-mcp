(function () {
  'use strict';
  if (window.parent === window) return;
  const MAX_CHARS = 500000, MAX_BLOCKS = 5000;
  const excluded = 'script,style,noscript,template,nav,form,input,button,select,textarea,label,[hidden],[aria-hidden="true"],[data-artifact-readable="false"],[data-artifact-reader-region="exclude"],[role="navigation"],[role="toolbar"],[role="tablist"],[role="button"],[role="menu"],[role="timer"],[role="log"]:not([data-artifact-reader-block]),[contenteditable="true"]';
  const blockTags = new Set('H1 H2 H3 H4 H5 H6 P LI DT DD BLOCKQUOTE PRE FIGCAPTION CAPTION TD TH DIV SECTION ARTICLE MAIN BODY HEADER FOOTER'.split(' '));
  let activeScope = null;
  const viewSelector = '[data-artifact-reader-region="view"],[role="tabpanel"],[role="application"]';
  const detailSelector = 'table,tr,pre,[data-artifact-reader-detail]';
  let snapshot = new Map(), active = null, signature = null, timer, selection = '', selectionRange = null, playbackSelectionRange = null, point = null, pickMode = false, pickedElement = null, targetFrame = 0;
  const post = message => window.parent.postMessage(message, '*');
  const wordSources = new Map();
  let blockOnly = new Set();
  const normalize = text => text.replace(/\s+/g, ' ').trim();
  const style = document.createElement('style');
  style.textContent = '[data-artifact-reader-target]{outline:1px dashed #9b681f!important;outline-offset:4px;background-color:rgba(181,107,44,.18)!important;border-radius:2px} [data-artifact-reader-active]{outline:2px solid #b56b2c!important;outline-offset:3px;background-color:rgba(181,107,44,.12)!important} ::highlight(artifact-reader-passage){background-color:rgba(181,107,44,.24);color:inherit} ::highlight(artifact-reader-word){background-color:rgba(255,194,74,.75);color:inherit;border-radius:2px}';
  (document.head || document.documentElement).appendChild(style);
  function allowed(element) {
    if (!element || element.closest(excluded)) return false;
    for (let node = element; node; node = node.parentElement) {
      if (node.tagName === 'DETAILS' && !node.open && element !== node && !node.querySelector('summary')?.contains(element)) return false;
      const css = getComputedStyle(node);
      if (css.display === 'none' || css.visibility === 'hidden' || css.visibility === 'collapse') return false;
    }
    return true;
  }
  // Authored summaries are plain text. Never derive an interpretation from chart marks.
  function description(element) {
    const explicit = normalize(element.getAttribute('data-artifact-reader-summary') || '');
    if (explicit) return explicit;
    const references = (element.getAttribute('aria-describedby') || '').split(/\s+/).filter(Boolean)
      .map(id => document.getElementById(id)).filter(node => node && allowed(node));
    if (references.length) return references.map(node => scanRoot(node, null, false, true).groups.map(group => group.text).join(' ')).join(' ');
    const caption = element.querySelector(':scope > figcaption, :scope > caption');
    if (caption && allowed(caption)) return scanRoot(caption, null, false, true).groups.map(group => group.text).join(' ');
    return normalize(element.getAttribute('aria-label') || element.getAttribute('alt') || element.querySelector('desc')?.textContent || '');
  }
  function readingRoots() {
    const explicit = [...document.querySelectorAll('[data-artifact-reader-region]:not([data-artifact-reader-region="view"])')].filter(allowed);
    return explicit.length ? explicit.filter(node => !explicit.some(other => other !== node && other.contains(node))) : [document.body || document.documentElement];
  }
  function scopeKey(element) {
    if (element.id && document.getElementById(element.id) === element) return 'id:' + element.id;
    const path = [];
    for (let node = element; node && node !== document.documentElement; node = node.parentElement) {
      path.unshift([...node.parentElement.children].indexOf(node));
    }
    return 'path:' + path.join('.');
  }
  function scopeElement(key) {
    if (!key) return null;
    if (key.startsWith('id:')) return document.getElementById(key.slice(3));
    if (!/^path:(\d+\.)*\d+$/.test(key)) return null;
    let node = document.documentElement;
    for (const index of key.slice(5).split('.')) node = node?.children[Number(index)];
    return node || null;
  }
  function scopedScan(mode, key) {
    if (mode === 'selection') return scanRoot(document.body, playbackSelectionRange || selectionRange);
    if (mode === 'view' || mode === 'detail') {
      let root = key ? scopeElement(key) : point?.closest(mode === 'view' ? viewSelector : detailSelector);
      if (!root && !key && mode === 'view') root = [...document.querySelectorAll(viewSelector)].find(allowed) || readingRoots()[0];
      if (!root || !allowed(root)) throw new Error('This reading view is no longer visible.');
      if (mode === 'detail' && !root.matches(detailSelector)) throw new Error('Choose a table, code block, or described visual.');
      const detail = mode === 'detail' && normalize(root.getAttribute('data-artifact-reader-detail') || '');
      const found = detail ? {groups:[{element:root, text:detail.slice(0,MAX_CHARS), blockOnly:true}], truncated:detail.length > MAX_CHARS} : scanRoot(root, null, false, false, mode === 'detail');
      return {...found, scopeKey:scopeKey(root)};
    }
    const found = scan();
    if (mode !== 'section' || !found.groups.length) return found;
    const root = key ? scopeElement(key) : point;
    if (key && (!root || !allowed(root))) throw new Error('This section is no longer visible.');
    let index = root ? found.groups.findLastIndex(g => g.element === root || g.element.contains(root)) : -1;
    if (index < 0 && !key) index = found.groups.findIndex(g => g.element.getBoundingClientRect().bottom > 0);
    if (index < 0) throw new Error('This section is no longer available.');
    const section = found.groups[index].element.closest('section');
    let start = index, end = index + 1;
    if (section) {
      while (start > 0 && section.contains(found.groups[start-1].element)) start--;
      while (end < found.groups.length && section.contains(found.groups[end].element)) end++;
    } else {
      while (start > 0 && !isHeading(found.groups[start].element)) start--;
      end = start + 1;
      while (end < found.groups.length && !isHeading(found.groups[end].element)) end++;
    }
    return {...found, groups:found.groups.slice(start,end), scopeKey:scopeKey(found.groups[start].element)};
  }
  function scanRoot(root, range, mapPositions = false, literal = false, details = false) {
    const groups = []; let current = null, chars = 0, truncated = false;
    const consumed = new Set();
    if (!range && !literal) for (const visual of root.querySelectorAll('figure,svg,canvas,img,[role="img"],[data-artifact-reader-summary]')) {
      if (!allowed(visual) || visual.hasAttribute('data-artifact-reader-summary')) continue;
      for (const id of (visual.getAttribute('aria-describedby') || '').split(/\s+/)) {
        const node = document.getElementById(id);
        if (node && node !== visual && !node.contains(visual) && allowed(node)) consumed.add(node);
      }
    }
    function append(text, owner, node, start) {
      if (current && current.element === owner) current.text += text;
      else { current = { element: owner, text, positions:[] }; groups.push(current); }
      if (mapPositions) for (let i = 0; i < text.length; i++) current.positions.push({node, offset:start + i});
      chars += text.length;
    }
    function atomic(element, text, mapped = false) {
      current = null;
      const remaining = MAX_CHARS - chars;
      if (text.length > remaining) truncated = true;
      const value = text.slice(0, remaining);
      groups.push({element, text:value, blockOnly: !mapped, positions: mapPositions ? mapped ? scanRoot(element, null, true, true).groups.flatMap((group, index) => index ? [null, ...group.positions] : group.positions).slice(0, value.length) : Array(value.length).fill(null) : []});
      chars += value.length;
    }
    function plain(element) {
      return scanRoot(element, null, false, true).groups.map(group => group.text).join(' ');
    }
    function table(element) {
      const rows = [...element.rows].filter(allowed), caption = description(element);
      const simple = (details || rows.length <= 13) && rows.every(row => row.cells.length <= 6 && [...row.cells].every(cell => cell.colSpan === 1 && cell.rowSpan === 1));
      const header = rows[0], headers = header && [...header.cells];
      if (!simple || !headers?.length || !headers.every(cell => cell.tagName === 'TH')) {
        if (details) {
          if (caption) atomic(element, caption);
          atomic(element, 'Table details. Header associations are unavailable; reading visible rows in order.');
          for (const row of rows) { if (chars >= MAX_CHARS || groups.length >= MAX_BLOCKS) { truncated = true; break; } atomic(row, plain(row)); }
        } else atomic(element, caption || 'Table. Use Read details to hear its rows.');
        return;
      }
      if (caption) atomic(element, caption);
      for (const row of rows.slice(1)) {
        if (groups.length >= MAX_BLOCKS || chars >= MAX_CHARS) { truncated = true; break; }
        const cells = [...row.cells];
        if (cells.length !== headers.length) { atomic(row, plain(row)); continue; }
        atomic(row, cells.map((cell, index) => {
          const label = plain(headers[index]), value = plain(cell);
          return label ? label + ': ' + value : value;
        }).filter(Boolean).join('. '));
      }
    }
    function walk(node, owner) {
      if (consumed.has(node)) return;
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
        if (!range && !literal) {
          const tag = node.tagName.toUpperCase();
          if (node.hasAttribute('data-artifact-reader-summary') || ['FIGURE','SVG','CANVAS','IMG'].includes(tag) || node.getAttribute('role') === 'img') {
            if (node.getAttribute('role') === 'presentation' || node.getAttribute('role') === 'none' || tag === 'IMG' && node.getAttribute('alt') === '') return;
            const summary = description(node);
            // Unlabelled inline SVGs are usually icons. Standalone visuals get a short fallback.
            if (summary || tag !== 'SVG' || node.parentElement === root) atomic(node, summary || 'Visual. No description is available.');
            return;
          }
          if (details && tag === 'TR') {
            const headers = [...(node.closest('table')?.rows[0]?.cells || [])], cells = [...node.cells];
            const simple = headers.length === cells.length && headers.every(c => c.tagName === 'TH' && c.colSpan === 1 && c.rowSpan === 1) && cells.every(c => c.colSpan === 1 && c.rowSpan === 1);
            atomic(node, simple ? cells.map((cell, index) => plain(headers[index]) + ': ' + plain(cell)).join('. ') : plain(node)); return;
          }
          if (tag === 'TABLE') { table(node); return; }
          if (tag === 'PRE') { atomic(node, details ? plain(node) : 'Code block. Use Read details for literal reading.', details); return; }
          if ((tag === 'LI' || tag === 'DL' || node.hasAttribute('data-artifact-reader-block')) && !node.querySelector('figure,svg,canvas,img,table,pre,[role="img"],[data-artifact-reader-summary]')) { atomic(node, plain(node), true); return; }
        }
        const isBlock = blockTags.has(node.tagName) || ['block', 'flex', 'grid', 'table-row'].includes(getComputedStyle(node).display);
        if (isBlock) current = null;
        for (const child of node.childNodes) walk(child, isBlock ? node : owner);
        if (node.tagName === 'BR' && current) { current.text += ' '; if (mapPositions) current.positions.push(null); }
        if (isBlock) current = null;
      }
    }
    walk(root, root);
    return { groups: groups.map(g => {
      if (!mapPositions) return {element:g.element, text:normalize(g.text), blockOnly: g.blockOnly === true};
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
    const groups = []; let chars = 0, truncated = false;
    for (const root of readingRoots()) {
      const found = scanRoot(root); truncated ||= found.truncated;
      for (const group of found.groups) {
        if (chars >= MAX_CHARS || groups.length >= MAX_BLOCKS) { truncated = true; break; }
        const text = group.text.slice(0, MAX_CHARS - chars); truncated ||= text.length < group.text.length;
        groups.push({...group, text}); chars += text.length;
      }
    }
    return {groups, truncated};
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
    let anchor = point ? blocks.findLastIndex(b => snapshot.get(b.id) === point || snapshot.get(b.id).contains && snapshot.get(b.id).contains(point)) : -1;
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
  function content(mode, full, requestedScope) {
    wordSources.clear();
    if (mode === 'selection') { playbackSelectionRange = selectionRange?.cloneRange() || null; activeScope = {mode:'selection'}; signature = scopedScan('selection').groups.map(g => g.text).join('\n'); return { scopeKey:'', blocks: selection ? [{ id: 'selection', text: selection }] : [], truncated: selection.length >= MAX_CHARS }; }
    const found = scopedScan(mode, requestedScope); activeScope = {mode, key:found.scopeKey}; snapshot = new Map(); blockOnly = new Set();
    let blocks = found.groups.map((group, ordinal) => {
      const id = 'r' + ordinal; snapshot.set(id, group.element); if (group.blockOnly) blockOnly.add(id);
      return { id, ordinal, text: group.text };
    });
    blocks = blocks.map((block, ordinal) => { const meta = sectionMeta(blocks, ordinal); return Object.assign(block, { sectionEndOrdinal: meta.end, sectionLabel: meta.label }); });
    if (mode === 'here') {
      let index = point ? blocks.findLastIndex(b => { const el = snapshot.get(b.id); return el === point || el.contains(point); }) : -1;
      if (index < 0) index = blocks.findIndex(b => { const r = snapshot.get(b.id).getBoundingClientRect(); return r.bottom > 0 && r.top < innerHeight && r.right > 0 && r.left < innerWidth; });
      blocks = blocks.slice(Math.max(0, index));
    }
    signature = found.groups.map(g => g.text).join('\n');
    return { blocks, scopeKey:found.scopeKey || '', truncated: found.truncated, chapter: found.chapter || null, fingerprint: fingerprint(found), label: found.chapter && found.chapter.label || null };
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
    if (signature === null) { activeScope = {mode:'page'}; signature = found.groups.map(group => group.text).join('\n'); }
    const adapted = window.__artifactEreader;
    const details = [...document.querySelectorAll('table,pre,[data-artifact-reader-detail]')].filter(node => allowed(node) && readingRoots().some(root => root.contains(node))).slice(0,100).map(node => ({scopeKey:scopeKey(node), label:(description(node) || (node.tagName === 'PRE' ? 'Code block' : node.tagName === 'TABLE' ? 'Table' : 'Visual details')).slice(0,120)})).filter(item => item.scopeKey.length <= 500);
    return {details, fingerprint:fingerprint(found),sections:sections.sort((a,b)=>a.ordinal-b.ordinal).slice(0,500),chapters:adapted && adapted.outline ? adapted.outline().chapters.slice(0,500) : [],chapter:found.chapter || null};
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
    if (blockOnly.has(id)) return null;
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
    const target = event.target.closest && event.target.closest('h1,h2,h3,h4,h5,h6,p,li,blockquote,pre,figure,svg,canvas,img,table,tr,figcaption,caption,td,dd,dt,[data-artifact-reader-block],[data-artifact-reader-summary]');
    if (!target || !allowed(target) || target.closest('a,button,form,nav,[contenteditable="true"]')) return;
    const found = scan(), ordinal = found.groups.findLastIndex(group => group.element === target || group.element.contains && group.element.contains(target));
    if (ordinal >= 0) { const detail = target.closest(detailSelector); post({ type: 'reader:target', detailAvailable:!!detail, scopeKey:detail ? scopeKey(detail) : '', ordinal, fingerprint: fingerprint(found), label: found.groups[ordinal].text.slice(0, 160) }); pick(found.groups[ordinal].element); }
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
    if (!['page', 'selection', 'here', 'section', 'view', 'detail'].includes(data.mode)) { post({ type: 'reader:error', requestId: data.requestId, message: 'Invalid reading mode.' }); return; }
    if (data.scopeKey !== undefined && (typeof data.scopeKey !== 'string' || data.scopeKey.length > 500)) return;
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
        const found = content(data.type === 'reader:jump' || data.type === 'reader:resume' && !data.scopeKey ? 'page' : data.mode, data.type === 'reader:resume', data.scopeKey);
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
      let next;
      try { next = scopedScan(activeScope?.mode || 'page', activeScope?.key).groups.map(g => g.text).join('\n'); } catch (_) { next = null; }
      if (next !== signature) { targetResize.disconnect(); clearPicked(); signature = next; selection = ''; selectionRange = null; playbackSelectionRange = null; wordSources.clear(); clearPassageHighlight(); highlight(null); snapshot.clear(); post({ type: 'reader:changed', version: 1, scopeKey:activeScope?.key || '' }); }
    }, 160);
  }).observe(document.documentElement, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['hidden', 'aria-hidden', 'class', 'style', 'data-artifact-readable', 'data-artifact-reader-region', 'data-artifact-reader-summary', 'data-artifact-reader-detail', 'data-artifact-reader-block', 'aria-describedby', 'aria-label', 'alt', 'role', 'open'] });
  post({ type: 'reader:ready', version: 1 });
})();
