// SPDX-License-Identifier: Apache-2.0
// Compact trusted gallery controls. Collection renderers share the portal's filters.
// One toolbar row: search, filters and sort, then a single View menu for folders, layout and
// preview size, then New folder when the folder shelf's own New folder tile is not on screen.
(function () {
  'use strict';
  const tools = document.querySelector('.collection-tools');
  const config = document.getElementById('collection-config');
  if (!tools || !config) return;
  const admin = config.dataset.viewerAdmin === '1';
  const actions = tools.querySelector('.toolbar-actions');
  const originalFilters = [...tools.querySelectorAll('[data-filter-view]')];
  const wrapper = document.createElement('div');
  wrapper.className = 'library-controls'; wrapper.id = 'library-controls';
  tools.before(wrapper); wrapper.append(tools);
  function invoke(method, value) { const api = window.ArtifactCollections; if (typeof api?.[method] === 'function') return api[method](value); }
  function el(tag, props, text) { const node = document.createElement(tag); Object.assign(node, props || {}); if (text != null) node.textContent = text; return node; }
  const sort = document.getElementById('sort');

  // View menu: one disclosure that holds every presentation choice.
  const view = el('div', { className: 'library-view', id: 'library-view' });
  const viewToggle = el('button', { type: 'button', id: 'library-view-toggle', className: 'library-view-toggle' });
  viewToggle.innerHTML = '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2.5" width="12" height="4" rx="1"/><rect x="2" y="9.5" width="5" height="4" rx="1"/><rect x="9" y="9.5" width="5" height="4" rx="1"/></svg><span>View</span>';
  viewToggle.setAttribute('aria-expanded', 'false'); viewToggle.setAttribute('aria-controls', 'library-view-menu');
  const menu = el('div', { id: 'library-view-menu', className: 'library-view-menu', hidden: true });
  menu.setAttribute('role', 'group'); menu.setAttribute('aria-label', 'View options');
  view.append(viewToggle, menu);

  const folderGroup = el('div', { className: 'library-view-group library-view-folders' });
  folderGroup.setAttribute('role', 'group'); folderGroup.setAttribute('aria-labelledby', 'library-view-folders-label');
  folderGroup.append(el('p', { className: 'library-view-heading', id: 'library-view-folders-label' }, 'Folders'));
  const folderChoices = [
    ['reel', 'Shelf', 'Folder tabs above your artifacts'],
    ['sheets', 'Contact sheets', 'One page of previews per folder'],
    ['ribbons', 'Ribbons', 'One scrolling row per folder'],
    ['all', 'Hide folders', 'Artifacts only'],
  ];
  for (const [value, label, hint] of folderChoices) {
    const button = el('button', { type: 'button', className: 'library-view-choice' });
    button.dataset.libraryView = value;
    button.append(el('span', { className: 'library-view-choice-label' }, label), el('small', null, hint));
    button.setAttribute('aria-pressed', String(value === 'reel'));
    button.addEventListener('click', () => { invoke('setView', value); sync(); });
    folderGroup.append(button);
  }

  function segmentRow(id, label) {
    const row = el('div', { className: 'library-view-row', id });
    row.setAttribute('role', 'group'); row.setAttribute('aria-labelledby', id + '-label');
    row.append(el('p', { className: 'library-view-heading', id: id + '-label' }, label));
    return row;
  }
  const layoutRow = segmentRow('library-view-layout', 'Layout');
  const layout = tools.querySelector('.layout-toggle');
  if (layout) { layout.removeAttribute('aria-label'); layoutRow.append(layout); }
  const sizeRow = segmentRow('library-view-size', 'Previews');
  const size = el('div', { className: 'layout-toggle library-size-toggle' });
  for (const [value, label] of [['compact', 'Compact'], ['large', 'Large']]) {
    const button = el('button', { type: 'button' }, label);
    button.dataset.previewSize = value; button.setAttribute('aria-pressed', String(value === 'compact'));
    button.addEventListener('click', () => { invoke('setDensity', value); sync(); });
    size.append(button);
  }
  sizeRow.append(size);
  menu.append(folderGroup, el('hr'), layoutRow, sizeRow);

  const create = el('button', { type: 'button', id: 'new-folder', className: 'library-new-folder' });
  create.setAttribute('aria-label', 'New folder');
  create.innerHTML = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3.5v9M3.5 8h9"/></svg><span class="folder-label">New folder</span><span class="folder-short" aria-hidden="true">Folder</span>';
  create.addEventListener('click', () => document.querySelector('#collection-surface [data-collection-create]')?.click());

  // Status filter: the legacy quick-filter buttons become one select.
  const statusLabel = el('label', { className: 'select-control library-status-control' });
  statusLabel.append(el('span', { className: 'sr-only' }, 'Show artifacts'));
  const status = el('select', { id: 'library-status' }); status.setAttribute('aria-label', 'Show artifacts'); statusLabel.append(status);
  for (const button of originalFilters) {
    const count = button.querySelector('span');
    const label = [...button.childNodes].filter(node => node.nodeType === Node.TEXT_NODE).map(node => node.textContent).join('').trim().replace(/\s+/g, ' ');
    status.append(el('option', { value: button.dataset.filterView }, count ? `${label} (${count.textContent.trim()})` : label));
  }
  if (status.options[0]) status.options[0].textContent = 'All artifacts';
  const uncollected = el('option', { value: 'uncollected' }, 'Uncollected'); status.append(uncollected);
  originalFilters.forEach(button => { button.hidden = true; });
  const group = tools.querySelector('.filter-group,.view-filters,.filter-choices'); if (group) group.hidden = true;
  actions.prepend(statusLabel);
  status.addEventListener('change', () => {
    const value = status.value; const legacy = value === 'uncollected' ? 'all' : value;
    originalFilters.find(button => button.dataset.filterView === legacy)?.click(); invoke('setStatus', value); sync();
  });
  if (!admin) document.getElementById('org-filter')?.closest('label')?.setAttribute('hidden', '');

  // Phones fold the filters behind one toggle; desktop shows them inline.
  const filters = el('button', { type: 'button', id: 'library-filters-toggle' });
  filters.innerHTML = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M2.5 4h11M4.5 8h7M6.5 12h3"/></svg><span>Filter &amp; sort</span>';
  filters.setAttribute('aria-expanded', 'false'); filters.setAttribute('aria-controls', 'library-filter-panel');
  actions.id = 'library-filter-panel';
  actions.after(filters);
  tools.append(view, create);
  filters.addEventListener('click', () => { const open = filters.getAttribute('aria-expanded') !== 'true'; filters.setAttribute('aria-expanded', String(open)); wrapper.dataset.filtersOpen = String(open); });

  function setMenu(open, focus) {
    menu.hidden = !open; viewToggle.setAttribute('aria-expanded', String(open)); view.dataset.open = String(open);
    if (!open && focus) viewToggle.focus();
  }
  viewToggle.addEventListener('click', () => setMenu(menu.hidden));
  document.addEventListener('click', event => { if (!menu.hidden && !view.contains(event.target)) setMenu(false); });
  document.addEventListener('keydown', event => { if (event.key === 'Escape' && !menu.hidden) { event.preventDefault(); event.stopImmediatePropagation(); setMenu(false, true); } }, true);
  view.addEventListener('focusout', event => { if (!menu.hidden && event.relatedTarget && !view.contains(event.relatedTarget)) setMenu(false); });

  const chips = el('div', { id: 'library-active-filters', className: 'library-active-filters', hidden: true });
  chips.setAttribute('aria-label', 'Active filters'); wrapper.after(chips);
  const intro = document.querySelector('.collection-head .intro-copy'); if (intro) intro.textContent = 'Browse, collect, and return to the latest revision.';
  const reset = document.querySelector('[data-reset-filters]'); if (reset) reset.textContent = 'Clear all'; reset?.addEventListener('click', () => { invoke('clearFilters'); status.value = 'all'; sync(); });
  function current() { return window.ArtifactCollections?.getState?.() || {}; }
  function portal() { return window.ArtifactPortal?.getLibraryState?.() || {}; }
  function removeFilter(field) {
    const node = { q: document.getElementById('q'), org: document.getElementById('org-filter'), category: document.getElementById('category-filter'), sort }[field];
    if (node) { node.value = field === 'q' ? '' : field === 'sort' ? 'recent' : 'all'; node.dispatchEvent(new Event(field === 'q' ? 'input' : 'change', { bubbles: true })); }
    if (field === 'status') { status.value = 'all'; status.dispatchEvent(new Event('change', { bubbles: true })); }
    if (field === 'collection') invoke('setScope', null); sync();
  }
  function sync() {
    const c = current(), p = portal(), prefs = c.preferences || {}; const activeView = c.view || prefs.view || 'reel';
    folderGroup.querySelectorAll('[data-library-view]').forEach(button => button.setAttribute('aria-pressed', String(button.dataset.libraryView === activeView)));
    const activeStatus = c.status || p.view || 'all'; if ([...status.options].some(option => option.value === activeStatus)) status.value = activeStatus;
    uncollected.textContent = 'Uncollected' + (Number.isFinite(c.uncollectedCount) ? ' (' + c.uncollectedCount + ')' : '');
    const large = prefs.previewSize === 'large';
    size.querySelectorAll('[data-preview-size]').forEach(button => button.setAttribute('aria-pressed', String((button.dataset.previewSize === 'large') === large)));
    const overview = (activeView === 'sheets' || activeView === 'ribbons') && !c.selected;
    layoutRow.hidden = overview;
    sizeRow.hidden = activeView === 'sheets' && !c.selected || prefs.artifactLayout === 'list' && activeView !== 'ribbons';
    // The shelf ends with its own New folder tile. The toolbar button covers every other view, and
    // a shelf too long to show that tile without scrolling.
    const shelf = document.querySelector('#collection-surface .collection-grid');
    create.hidden = activeView === 'reel' && !c.selected && !!shelf?.querySelector('[data-collection-create]') && shelf.scrollWidth <= shelf.clientWidth + 1;
    const entries = []; if (p.q) entries.push(['q', 'Search: ' + p.q]); if (p.org && p.org !== 'all' && admin) entries.push(['org', p.org]); if (p.category && p.category !== 'all') entries.push(['category', p.category]); if (activeStatus !== 'all') entries.push(['status', status.selectedOptions[0]?.textContent || activeStatus]); if (p.sort && p.sort !== 'recent') entries.push(['sort', sort?.selectedOptions[0]?.textContent || p.sort]); if (c.selected) { const folder = c.collections?.find(item => item.id === c.selected); entries.push(['collection', folder?.name || 'Folder']); }
    chips.replaceChildren(); for (const [field, text] of entries) { const button = el('button', { type: 'button' }, text + ' ×'); button.setAttribute('aria-label', 'Remove ' + text + ' filter'); button.addEventListener('click', () => removeFilter(field)); chips.append(button); } if (reset) { reset.hidden = entries.length < 2; chips.append(reset); } chips.hidden = entries.length === 0;
    const summary = document.querySelector('.collection-results .result-summary'); if (summary) summary.hidden = entries.length === 0;
  }
  document.addEventListener('collections:rendered', sync);
  let resizeFrame = 0; window.addEventListener('resize', () => { cancelAnimationFrame(resizeFrame); resizeFrame = requestAnimationFrame(sync); });
  tools.addEventListener('change', () => requestAnimationFrame(sync)); document.getElementById('q')?.addEventListener('input', () => requestAnimationFrame(sync));
  layout?.addEventListener('click', event => { const button = event.target.closest('[data-layout]'); if (button && event.isTrusted) invoke('setLayout', button.dataset.layout); });
  sort?.addEventListener('change', () => requestAnimationFrame(sync));
  window.CollectionToolbar = Object.freeze({ sync, wrapper });
  sync();
}());
