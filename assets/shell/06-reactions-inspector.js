  var theme=document.getElementById('vtheme');
  if(theme) theme.addEventListener('click',function(){
    var current=document.documentElement.dataset.theme;
    var dark=window.matchMedia&&window.matchMedia('(prefers-color-scheme: dark)').matches;
    var next=current==='dark'?'light':current==='light'?'dark':dark?'light':'dark';
    document.documentElement.dataset.theme=next;
    try{localStorage.setItem('artifact-theme',next);}catch(e){}
  });

  document.addEventListener('keydown',function(e){
    if(e.defaultPrevented||e.altKey||e.ctrlKey||e.metaKey) return;
    if(e.target.closest&&e.target.closest('input,textarea,select,[contenteditable],[role="menu"],dialog,[role="dialog"]')) return;
    if(e.key==='ArrowLeft'&&prevId){location.href='/'+prevId;}
    if(e.key==='ArrowRight'&&nextId){location.href='/'+nextId;}
  });

  var R={favorite:Number(shellConfig.favorite)||0,vote:Number(shellConfig.vote)||0};
  var buttons=[].slice.call(document.querySelectorAll('.vreact'));
  var status=document.getElementById('reaction-status');
  var statusTimer;
  function announce(message,isError){
    clearTimeout(statusTimer);status.textContent=message;status.classList.toggle('error',!!isError);status.classList.add('show');
    statusTimer=setTimeout(function(){status.classList.remove('show');},1800);
  }
  function paintR(){
    [].slice.call(document.querySelectorAll('.vreact.fav')).forEach(function(f){f.setAttribute('aria-pressed',R.favorite?'true':'false');f.setAttribute('aria-label',R.favorite?'Remove from favorites':'Save to favorites');});
    [].slice.call(document.querySelectorAll('.vreact.up')).forEach(function(u){u.setAttribute('aria-checked',R.vote>0?'true':'false');});
    [].slice.call(document.querySelectorAll('.vreact.down')).forEach(function(d){d.setAttribute('aria-checked',R.vote<0?'true':'false');});
  }
  buttons.forEach(function(b){
    b.addEventListener('click',function(){
      var act=b.dataset.act,body={};
      if(act==='fav')body.favorite=R.favorite?0:1;
      else if(act==='up')body.vote=R.vote>0?0:1;
      else body.vote=R.vote<0?0:-1;
      buttons.forEach(function(x){x.disabled=true;});
      fetch('/'+artifactId+'/react',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)})
        .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Request failed');return d;});})
        .then(function(d){
          if(d&&typeof d.favorite!=='undefined'){R.favorite=d.favorite;R.vote=d.vote;paintR();announce(act==='fav'?(R.favorite?'Saved to favorites':'Removed from favorites'):'Feedback saved',false);}
        })
        .catch(function(){announce('Could not save feedback',true);})
        .finally(function(){buttons.forEach(function(x){x.disabled=false;});});
    });
  });

  // One controller owns every inspector mode. The artifact frame remains outside this
  // focus boundary and is never inspected directly by the shell.
  function fesc(s){return String(s==null?'':s).replace(/[&<>"']/g,function(c){return {'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c];});}
  var viewerEmail=configLiteral('viewerEmail'),viewerIsAdmin=shellConfig.viewerIsAdmin==='1';
  var inspector=document.getElementById('vinspector'),inspectorClose=document.getElementById('vinspector-close'),inspectorTitle=document.getElementById('inspector-title'),lastInspectorFocus=null,activeInspector=null;
  var inspectorTitles={feedback:'Feedback',details:'Details',share:'Share',history:'Version history',audience:'Audience'};
  function inspectorOpen(mode,source){
    if(!inspector||!document.getElementById('inspector-'+mode))return;
    var wasOpen=inspector.classList.contains('open');
    closeShellMenus();
    if(!wasOpen)lastInspectorFocus=source||document.activeElement;activeInspector=mode;inspector.removeAttribute('inert');inspector.classList.add('open');inspector.setAttribute('aria-hidden','false');
    if(inspectorTitle)inspectorTitle.textContent=inspectorTitles[mode]||'Artifact inspector';
    [].slice.call(document.querySelectorAll('[data-inspector-tab]')).forEach(function(tab){var selected=tab.getAttribute('data-inspector-tab')===mode;tab.setAttribute('aria-selected',selected?'true':'false');tab.tabIndex=selected?0:-1;});
    [].slice.call(document.querySelectorAll('.inspector-pane')).forEach(function(panel){var selected=panel.id==='inspector-'+mode;panel.hidden=!selected;panel.setAttribute('aria-hidden',selected?'false':'true');});
    [].slice.call(document.querySelectorAll('[data-inspector-open]')).forEach(function(trigger){trigger.setAttribute('aria-expanded',trigger.getAttribute('data-inspector-open')===mode?'true':'false');});
    if(mode==='share'&&typeof shareLoad==='function'&&!shareLoaded)shareLoad();if(mode==='history'&&typeof histLoad==='function'&&!histLoaded)histLoad();if(mode==='details'&&typeof loadDiscussion==='function')loadDiscussion();
    var target=document.querySelector('[data-inspector-tab="'+mode+'"]');setTimeout(function(){(target||inspectorClose||inspector).focus();},0);
  }
  function inspectorClosePanel(){if(!inspector||!inspector.classList.contains('open'))return;inspector.classList.remove('open');inspector.setAttribute('aria-hidden','true');inspector.setAttribute('inert','');activeInspector=null;[].slice.call(document.querySelectorAll('[data-inspector-open]')).forEach(function(trigger){trigger.setAttribute('aria-expanded','false');});if(lastInspectorFocus&&lastInspectorFocus.focus)lastInspectorFocus.focus();}
  function inspectorSource(trigger){return trigger&&trigger.closest&&trigger.closest('#vtitle-menu')?titleToggle:trigger&&trigger.closest&&trigger.closest('#vmore-menu')?moreToggle:trigger;}
  [].slice.call(document.querySelectorAll('[data-inspector-open]')).forEach(function(trigger){trigger.addEventListener('click',function(){var mode=trigger.getAttribute('data-inspector-open'),source=inspectorSource(trigger);closeShellMenus();if(inspector&&inspector.classList.contains('open')&&activeInspector===mode)inspectorClosePanel();else inspectorOpen(mode,source);});});
  [].slice.call(document.querySelectorAll('[data-inspector-tab]')).forEach(function(tab){tab.addEventListener('click',function(){inspectorOpen(tab.getAttribute('data-inspector-tab'),tab);});tab.addEventListener('keydown',function(e){if(e.key!=='ArrowLeft'&&e.key!=='ArrowRight')return;var tabs=[].slice.call(document.querySelectorAll('[data-inspector-tab]')),next=(tabs.indexOf(tab)+(e.key==='ArrowRight'?1:tabs.length-1))%tabs.length;e.preventDefault();tabs[next].focus();inspectorOpen(tabs[next].getAttribute('data-inspector-tab'),tabs[next]);});});
  if(inspectorClose)inspectorClose.addEventListener('click',inspectorClosePanel);
  document.addEventListener('keydown',function(e){if(e.key==='Escape'&&inspector&&inspector.classList.contains('open')){e.preventDefault();inspectorClosePanel();return;}if(e.key==='Tab'&&inspector&&inspector.classList.contains('open')){var items=[].slice.call(inspector.querySelectorAll('button:not([disabled]),a[href],input:not([disabled]),select:not([disabled]),textarea:not([disabled]),[tabindex="0"]')).filter(function(item){return !item.closest('[hidden]');});if(!items.length)return;var first=items[0],last=items[items.length-1];if(e.shiftKey&&document.activeElement===first){e.preventDefault();last.focus();}else if(!e.shiftKey&&document.activeElement===last){e.preventDefault();first.focus();}}});
  var titleToggle=document.getElementById('vtitle-toggle'),titleMenu=document.getElementById('vtitle-menu'),moreToggle=document.getElementById('vmore-toggle'),moreMenu=document.getElementById('vmore-menu');
  function closeMenu(menu,trigger,restore){if(!menu||menu.hidden)return;menu.hidden=true;if(trigger)trigger.setAttribute('aria-expanded','false');if(restore&&trigger&&trigger.isConnected)trigger.focus();}
  function closeTitle(restore){closeMenu(titleMenu,titleToggle,restore);}
  function closeMore(restore){closeMenu(moreMenu,moreToggle,restore);}
  function closeShellMenus(restore){closeTitle(restore);closeMore(restore);}
  function menuItems(menu){return menu?[].slice.call(menu.querySelectorAll('[role^="menuitem"]')).filter(function(item){return !item.disabled&&!item.hidden; }):[];}
  function focusMenuItem(menu,where){var items=menuItems(menu);if(!items.length)return;var index=where==='last'?items.length-1:where==='first'?0:Math.max(0,items.indexOf(document.activeElement)+where);(items[index]||items[0]).focus();}
  function toggleMenu(menu,trigger,otherMenu,otherTrigger){if(!menu)return;var open=menu.hidden;closeMenu(otherMenu,otherTrigger,false);if(open&&inspector&&inspector.classList.contains('open'))inspectorClosePanel();menu.hidden=!open;trigger.setAttribute('aria-expanded',open?'true':'false');}
  function bindMenu(trigger,menu,otherMenu,otherTrigger){if(!trigger||!menu)return;trigger.addEventListener('click',function(){toggleMenu(menu,trigger,otherMenu,otherTrigger);});trigger.addEventListener('keydown',function(e){if(e.key==='ArrowDown'||e.key==='Home'){e.preventDefault();if(menu.hidden)toggleMenu(menu,trigger,otherMenu,otherTrigger);focusMenuItem(menu,'first');}else if(e.key==='ArrowUp'||e.key==='End'){e.preventDefault();if(menu.hidden)toggleMenu(menu,trigger,otherMenu,otherTrigger);focusMenuItem(menu,'last');}});menu.addEventListener('keydown',function(e){if(e.key==='Escape'){e.preventDefault();closeMenu(menu,trigger,true);return;}if(e.key==='ArrowDown'){e.preventDefault();focusMenuItem(menu,1);}else if(e.key==='ArrowUp'){e.preventDefault();focusMenuItem(menu,-1);}else if(e.key==='Home'){e.preventDefault();focusMenuItem(menu,'first');}else if(e.key==='End'){e.preventDefault();focusMenuItem(menu,'last');}});}
  bindMenu(titleToggle,titleMenu,moreMenu,moreToggle);bindMenu(moreToggle,moreMenu,titleMenu,titleToggle);
  document.addEventListener('pointerdown',function(e){if(titleMenu&&!titleMenu.hidden&&!e.target.closest('#vtitle-menu,#vtitle-toggle'))closeTitle();if(moreMenu&&!moreMenu.hidden&&!e.target.closest('#vmore-menu,#vmore-toggle'))closeMore();});
  var deleteTrigger=document.getElementById('vdelete-trigger'),deleteDialog=document.getElementById('delete-dialog'),deleteConfirm=document.getElementById('delete-confirm'),deleteError=document.getElementById('delete-error');
  if(deleteTrigger&&deleteDialog){
    deleteTrigger.addEventListener('click',function(){
      closeMore();deleteError.textContent='';deleteDialog.showModal();
      var cancel=deleteDialog.querySelector('.delete-cancel');setTimeout(function(){if(cancel)cancel.focus();},0);
    });
    deleteDialog.addEventListener('close',function(){
      deleteError.textContent='';deleteConfirm.disabled=false;deleteConfirm.textContent='Delete artifact';
      if(deleteTrigger.isConnected)deleteTrigger.focus();
    });
  }
  if(deleteConfirm&&deleteDialog){
    deleteConfirm.addEventListener('click',function(){
      deleteConfirm.disabled=true;deleteConfirm.textContent='Deleting…';deleteError.textContent='';
      fetch('/'+artifactId,{method:'DELETE',headers:{accept:'application/json'}})
        .then(function(r){return r.json().catch(function(){return {};}).then(function(d){if(!r.ok)throw new Error(d.error||'Could not delete artifact');return d;});})
        .then(function(){location.href='/?deleted=1';})
        .catch(function(error){deleteConfirm.disabled=false;deleteConfirm.textContent='Delete artifact';deleteError.textContent=error.message||'Could not delete artifact';deleteConfirm.focus();});
    });
  }
  var shareArtifactId=artifactId;
  var shareToggle=document.getElementById('vshare-toggle'),shareList=document.getElementById('vshare-list'),shareForm=document.getElementById('vshare-form'),shareExpiry=document.getElementById('vshare-expiry'),shareDate=document.getElementById('vshare-date'),shareResult=document.getElementById('vshare-result'),shareLoaded=false;
  function copyShareUrl(url,button){function done(){button.textContent='Copied';setTimeout(function(){button.textContent='Copy';},1200);}if(navigator.clipboard&&navigator.clipboard.writeText){navigator.clipboard.writeText(url).then(done).catch(fallback);return;}fallback();function fallback(){var input=document.createElement('textarea');input.value=url;input.setAttribute('readonly','');input.style.position='fixed';input.style.opacity='0';document.body.appendChild(input);input.select();try{if(document.execCommand('copy'))done();}catch(_){}input.remove();}}
  function shareRow(row){var expiry=row.expires_at?'Expires '+fesc(String(row.expires_at).replace('T',' ').slice(0,16)):'No expiration',created=row.created_at?'Created '+fesc(String(row.created_at).replace('T',' ').slice(0,16)):'Created recently';return '<div class="vfb-item vshare-row" data-token="'+fesc(row.token)+'"><div class="vfb-m"><span>'+expiry+'</span><span>'+created+'</span></div><div class="vshare-result"><a href="/s/'+fesc(row.token)+'" target="_blank" rel="noopener">/s/'+fesc(row.token)+'</a><button class="vshare-copy" type="button" data-share-copy="'+fesc(row.token)+'">Copy</button></div><button class="vshare-revoke" type="button" data-share-revoke="'+fesc(row.token)+'">Revoke</button></div>';}
  function shareLoad(){shareList.innerHTML='<div class="vfb-empty">Loading active links…</div>';fetch('/'+shareArtifactId+'/shares').then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not load links');return d;});}).then(function(d){shareLoaded=true;var rows=Array.isArray(d.shares)?d.shares:[];shareList.innerHTML=rows.length?rows.map(shareRow).join(''):'<div class="vfb-empty">No active public links.</div>';}).catch(function(){shareLoaded=false;shareList.innerHTML='<div class="vfb-empty">Could not load share links.</div>';});}
  function shareOpen(open){if(open)inspectorOpen('share',shareToggle);else inspectorClosePanel();}
  if(shareExpiry)shareExpiry.addEventListener('change',function(){shareDate.hidden=shareExpiry.value!=='date';if(shareExpiry.value==='date')shareDate.focus();});
  if(shareForm)shareForm.addEventListener('submit',function(e){e.preventDefault();var expires=shareExpiry.value==='date'?shareDate.value:shareExpiry.value;if(!expires){shareResult.textContent='Choose a future date.';return;}var button=shareForm.querySelector('button[type="submit"]');button.disabled=true;shareResult.textContent='Creating…';fetch('/'+shareArtifactId+'/share',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({expires:expires})}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not create link');return d;});}).then(function(d){var url=String(d.url||'');shareResult.innerHTML='<a href="'+fesc(url)+'" target="_blank" rel="noopener">'+fesc(url)+'</a><button class="vshare-copy" type="button" data-share-url="'+fesc(url)+'">Copy</button>';shareLoaded=false;shareLoad();}).catch(function(err){shareResult.textContent=err.message||'Could not create link';}).finally(function(){button.disabled=false;});});
  if(shareResult)shareResult.addEventListener('click',function(e){var copy=e.target.closest('[data-share-url]');if(copy)copyShareUrl(copy.getAttribute('data-share-url'),copy);});
  if(shareList)shareList.addEventListener('click',function(e){var copy=e.target.closest('[data-share-copy],[data-share-url]');if(copy){var url=copy.getAttribute('data-share-url')||location.origin+'/s/'+copy.getAttribute('data-share-copy');copyShareUrl(url,copy);return;}var revoke=e.target.closest('[data-share-revoke]');if(!revoke)return;var token=revoke.getAttribute('data-share-revoke');revoke.disabled=true;fetch('/'+shareArtifactId+'/shares/'+encodeURIComponent(token),{method:'DELETE'}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not revoke link');return d;});}).then(function(){var row=revoke.closest('[data-token]');if(row)row.remove();if(!shareList.querySelector('[data-token]'))shareList.innerHTML='<div class="vfb-empty">No active public links.</div>';}).catch(function(){revoke.disabled=false;});});
  var fbToggle=document.getElementById('vfb-toggle'),fbList=document.getElementById('vfb-list'),fbForm=document.getElementById('vfb-form'),fbBody=document.getElementById('vfb-body'),fbHint=document.getElementById('vfb-hint'),fbCounts=[].slice.call(document.querySelectorAll('.vfb-count'));
  function fbOpen(open){if(open){inspectorOpen('feedback',fbToggle);setTimeout(function(){if(fbBody)fbBody.focus();},180);}else inspectorClosePanel();}
  function discordAuthor(row){return row&&(row.author_source==='discord'||row.author&&row.author.source==='discord');}
  function authorLabel(row){return discordAuthor(row)?String(row.external_author_display||row.author&&row.author.external_author_display||'Discord user')+' · Discord':row.viewer_email;}
  function canManage(row){return viewerIsAdmin||(!discordAuthor(row)&&row.viewer_email===viewerEmail);}
  function itemHtml(row,justNow){var resolved=!!row.resolved_at,anchored=row.anchor_x!=null&&row.anchor_y!=null,box=row.anchor_w!=null&&row.anchor_h!=null,manage=canManage(row);return '<div class="vfb-item '+(resolved?'resolved':'')+'" data-id="'+fesc(row.id)+'"><div class="vfb-m"><span>'+fesc(authorLabel(row))+'</span><span>'+fesc(justNow?'Just now':'')+(resolved?' &middot; <span class="vfb-res">Resolved</span>':'')+'</span></div><div class="vfb-b">'+fesc(row.body)+'</div>'+(anchored?'<span class="vfb-anchor-state">'+(box?'Pinned section':'Pinned comment')+'</span><button class="vfb-copy-prompt" type="button" data-copy-prompt="'+fesc(row.id)+'">Copy prompt</button>':'')+(manage?'<div class="vfb-manage"><button class="vfb-delete" type="button" data-feedback-action="delete">Delete</button>'+(resolved?'':'<button class="vfb-resolve" type="button" data-feedback-action="resolve">Resolve</button>')+'</div>':'')+'</div>';}
  function replyFormHtml(parentId){return '<form class="vfb-reply-form" data-parent-id="'+fesc(parentId)+'"><textarea maxlength="4000" aria-label="Reply to feedback" placeholder="Reply to this thread…"></textarea><button type="submit">Reply</button></form>';}

