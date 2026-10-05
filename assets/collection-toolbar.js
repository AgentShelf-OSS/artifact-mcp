// SPDX-License-Identifier: Apache-2.0
// Compact trusted gallery controls. Collection renderers share the portal's filters.
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
  const row = document.createElement('section'); row.className = 'library-presentation';
  row.setAttribute('aria-label', 'Library presentation'); wrapper.append(row);
  const nav = document.createElement('nav'); nav.className = 'library-viewchoices';
  nav.setAttribute('aria-label', 'Library view'); row.append(nav);
  const choices = {all:'All artifacts',reel:'Reel shelf',sheets:'Contact sheets',ribbons:'Gallery ribbons'};
  function invoke(method, value) { const api = window.ArtifactCollections; if (typeof api?.[method] === 'function') return api[method](value); }
  for (const [view, label] of Object.entries(choices)) {
    const button = document.createElement('button'); button.type = 'button';
    button.dataset.libraryView = view; button.textContent = label;
    button.setAttribute('aria-pressed', String(view === 'reel'));
    button.addEventListener('click', () => { invoke('setView', view); sync(); }); nav.append(button);
  }
  const controls = document.createElement('div'); controls.className = 'library-display-options'; row.append(controls);
  const displayToggle = document.createElement('button'); displayToggle.type = 'button'; displayToggle.id = 'library-display-toggle';
  displayToggle.textContent = 'Display ⌄'; displayToggle.setAttribute('aria-expanded','false'); displayToggle.setAttribute('aria-controls','library-display-panel');
  controls.append(displayToggle);
  const display = document.createElement('div'); display.id='library-display-panel'; display.className='library-display-panel'; controls.append(display);
  const sort = document.getElementById('sort'); if (sort) display.append(sort.closest('label'));
  const size = document.createElement('button'); size.type='button'; size.id='preview-size'; size.textContent='Size: compact'; size.setAttribute('aria-label','Toggle preview size'); size.setAttribute('aria-pressed','false');
  size.addEventListener('click',()=>{invoke('setDensity',size.getAttribute('aria-pressed')==='true'?'compact':'large');sync();}); display.append(size);
  const layout = tools.querySelector('.layout-toggle'); if (layout) { display.append(layout); layout.setAttribute('aria-label','Artifact layout'); }
  const create=document.createElement('button');create.type='button';create.id='new-folder';create.className='library-new-folder';create.textContent='+ New folder';
  create.addEventListener('click',()=>document.querySelector('#collection-surface [data-collection-create]')?.click());row.append(create);
  const statusLabel=document.createElement('label');statusLabel.className='select-control library-status-control';
  const sr=document.createElement('span');sr.className='sr-only';sr.textContent='Show artifacts';statusLabel.append(sr);
  const status=document.createElement('select');status.id='library-status';status.setAttribute('aria-label','Show artifacts');statusLabel.append(status);
  for (const button of originalFilters) { const option=document.createElement('option');option.value=button.dataset.filterView;option.textContent=button.textContent.trim().replace(/\s+/g,' ');status.append(option); }
  if(status.options[0])status.options[0].textContent='All artifacts';
  const uncollected=document.createElement('option');uncollected.value='uncollected';uncollected.textContent='Uncollected';status.append(uncollected);
  originalFilters.forEach(button=>button.hidden=true);
  const group=tools.querySelector('.filter-group,.view-filters,.filter-choices');if(group)group.hidden=true;
  actions.prepend(statusLabel);
  status.addEventListener('change',()=>{
    const value=status.value;const legacy=value==='uncollected'?'all':value;
    originalFilters.find(button=>button.dataset.filterView===legacy)?.click();invoke('setStatus',value);sync();
  });
  if(!admin)document.getElementById('org-filter')?.closest('label')?.setAttribute('hidden','');
  const filters=document.createElement('button');filters.type='button';filters.id='library-filters-toggle';filters.textContent='Filters ⌄';
  filters.setAttribute('aria-expanded','false');filters.setAttribute('aria-controls','library-filter-panel');tools.append(filters);actions.id='library-filter-panel';
  filters.addEventListener('click',()=>{const open=filters.getAttribute('aria-expanded')!=='true';filters.setAttribute('aria-expanded',String(open));wrapper.dataset.filtersOpen=String(open);});
  displayToggle.addEventListener('click',()=>{const open=displayToggle.getAttribute('aria-expanded')!=='true';displayToggle.setAttribute('aria-expanded',String(open));controls.dataset.open=String(open);});
  function closeDisplay(focus=false){controls.dataset.open='false';displayToggle.setAttribute('aria-expanded','false');if(focus)displayToggle.focus();}
  document.addEventListener('click',event=>{if(!controls.contains(event.target))closeDisplay();});
  document.addEventListener('keydown',event=>{if(event.key==='Escape'&&controls.dataset.open==='true'){event.preventDefault();event.stopImmediatePropagation();closeDisplay(true);}},true);
  const chips=document.createElement('div');chips.id='library-active-filters';chips.className='library-active-filters';chips.setAttribute('aria-label','Active filters');chips.hidden=true;wrapper.after(chips);
  const intro=document.querySelector('.collection-head .intro-copy');if(intro)intro.textContent='Browse, collect, and return to the latest revision.';
  const reset=document.querySelector('[data-reset-filters]');reset?.addEventListener('click',()=>{invoke('clearFilters');status.value='all';sync();});
  function current(){return window.ArtifactCollections?.getState?.()||{};}
  function portal(){return window.ArtifactPortal?.getLibraryState?.()||{};}
  function removeFilter(field){
    const node={q:document.getElementById('q'),org:document.getElementById('org-filter'),category:document.getElementById('category-filter'),sort}[field];
    if(node){node.value=field==='q'?'':field==='sort'?'recent':'all';node.dispatchEvent(new Event(field==='q'?'input':'change',{bubbles:true}));}
    if(field==='status'){status.value='all';status.dispatchEvent(new Event('change',{bubbles:true}));}
    if(field==='collection')invoke('setScope',null);sync();
  }
  function sync(){
    const c=current(), p=portal(), prefs=c.preferences||{};const activeView=c.view||prefs.view||'reel';
    nav.querySelectorAll('button').forEach(button=>button.setAttribute('aria-pressed',String(button.dataset.libraryView===activeView)));
    const activeStatus=c.status||p.view||'all';if([...status.options].some(option=>option.value===activeStatus))status.value=activeStatus;
    uncollected.textContent='Uncollected'+(Number.isFinite(c.uncollectedCount)?' ('+c.uncollectedCount+')':'');
    size.textContent='Size: '+(prefs.previewSize==='large'?'large':'compact');size.setAttribute('aria-pressed',String(prefs.previewSize==='large'));
    const overview=(activeView==='sheets'||activeView==='ribbons')&&!c.selected; if(layout)layout.hidden=overview;size.hidden=activeView==='sheets'&&!c.selected||prefs.artifactLayout==='list'&&activeView!=='ribbons';
    const entries=[];if(p.q)entries.push(['q','Search: '+p.q]);if(p.org&&p.org!=='all'&&admin)entries.push(['org',p.org]);if(p.category&&p.category!=='all')entries.push(['category',p.category]);if(activeStatus!=='all')entries.push(['status',status.selectedOptions[0]?.textContent||activeStatus]);if(p.sort&&p.sort!=='recent')entries.push(['sort',sort?.selectedOptions[0]?.textContent||p.sort]);if(c.selected){const folder=c.collections?.find(item=>item.id===c.selected);entries.push(['collection',folder?.name||'Folder']);}
    chips.replaceChildren();for(const [field,text] of entries){const button=document.createElement('button');button.type='button';button.textContent=text+' ×';button.setAttribute('aria-label','Remove '+text+' filter');button.addEventListener('click',()=>removeFilter(field));chips.append(button);}chips.hidden=entries.length===0;
    const summary=document.querySelector('.collection-results .result-summary');if(summary)summary.hidden=entries.length===0;
    if(reset)reset.hidden=entries.length===0;
  }
  document.addEventListener('collections:rendered',sync);
  tools.addEventListener('change',()=>requestAnimationFrame(sync));document.getElementById('q')?.addEventListener('input',()=>requestAnimationFrame(sync));
  layout?.addEventListener('click',event=>{const button=event.target.closest('[data-layout]');if(button && event.isTrusted)invoke('setLayout',button.dataset.layout);});
  sort?.addEventListener('change',()=>requestAnimationFrame(sync));
  window.CollectionToolbar=Object.freeze({sync,wrapper});sync();
}());
