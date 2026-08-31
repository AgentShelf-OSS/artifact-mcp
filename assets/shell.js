
(function(){
  // See `assets/portal.js`: portal mutations use a non-simple header as a stateless CSRF signal.
  var nativeFetch=window.fetch;
  window.fetch=function portalFetch(input,init){var options=init||{},method=String(options.method||(input&&input.method)||'GET').toUpperCase();if(method==='POST'||method==='PUT'||method==='PATCH'||method==='DELETE'){var headers=new Headers(options.headers||(input&&input.headers));headers.set('x-artifact-mutation','1');options=Object.assign({},options,{headers:headers});}return nativeFetch.call(window,input,options);};
  var shellConfig=document.getElementById('shell-config').dataset;
  function configLiteral(name){return JSON.parse(shellConfig[name]);}
  var artifactId=configLiteral('artifactId'),prevId=configLiteral('prevId'),nextId=configLiteral('nextId'),bundleRawPrefix=configLiteral('bundleRawPrefix'),versionQuery=configLiteral('versionQuery');
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
    if(!wasOpen)lastInspectorFocus=source||document.activeElement;activeInspector=mode;inspector.classList.add('open');inspector.setAttribute('aria-hidden','false');
    if(inspectorTitle)inspectorTitle.textContent=inspectorTitles[mode]||'Artifact inspector';
    [].slice.call(document.querySelectorAll('[data-inspector-tab]')).forEach(function(tab){var selected=tab.getAttribute('data-inspector-tab')===mode;tab.setAttribute('aria-selected',selected?'true':'false');tab.tabIndex=selected?0:-1;});
    [].slice.call(document.querySelectorAll('.inspector-pane')).forEach(function(panel){var selected=panel.id==='inspector-'+mode;panel.hidden=!selected;panel.setAttribute('aria-hidden',selected?'false':'true');});
    [].slice.call(document.querySelectorAll('[data-inspector-open]')).forEach(function(trigger){trigger.setAttribute('aria-expanded',trigger.getAttribute('data-inspector-open')===mode?'true':'false');});
    if(mode==='share'&&typeof shareLoad==='function'&&!shareLoaded)shareLoad();if(mode==='history'&&typeof histLoad==='function'&&!histLoaded)histLoad();
    var target=document.querySelector('[data-inspector-tab="'+mode+'"]');setTimeout(function(){(target||inspectorClose||inspector).focus();},0);
  }
  function inspectorClosePanel(){if(!inspector||!inspector.classList.contains('open'))return;inspector.classList.remove('open');inspector.setAttribute('aria-hidden','true');activeInspector=null;[].slice.call(document.querySelectorAll('[data-inspector-open]')).forEach(function(trigger){trigger.setAttribute('aria-expanded','false');});if(lastInspectorFocus&&lastInspectorFocus.focus)lastInspectorFocus.focus();}
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

  // Positional comments are a postMessage-only boundary: this shell never inspects
  // the sandboxed iframe document. The bridge owns its document and reports pixels.
  var frame=document.getElementById('vframe'),overlay=document.getElementById('vanchor-overlay'),commentToggle=document.getElementById('vcomment-toggle'),commentMode=false,bridgeReady=false,currentPage=null,draftAnchor=null,bridgeTimer,fallbackDrag=null,isBundle=shellConfig.isBundle==='1';
  var composer=document.getElementById('vanchor-composer'),composerBody=document.getElementById('vanchor-body'),composerSave=document.getElementById('vanchor-save'),composerCopy=document.getElementById('vanchor-copy'),composerDismiss=document.getElementById('vanchor-dismiss'),composerSummary=document.getElementById('vanchor-summary'),composerStatus=document.getElementById('vanchor-status'),composerReturnFocus=null,draftSelection=null,composerSavedRow=null;
  var feedbackRows=JSON.parse(configLiteral('feedback')),pins=[],pinById={},feedbackItems={},feedbackThreads={};
  [].slice.call(fbList.querySelectorAll('.vfb-item[data-id]')).forEach(function(item){feedbackItems[item.getAttribute('data-id')]=item;});
  [].slice.call(fbList.querySelectorAll('.vfb-thread[data-thread-id]')).forEach(function(thread){feedbackThreads[thread.getAttribute('data-thread-id')]=thread;});
  function finiteFraction(value){return typeof value==='number'&&Number.isFinite(value)?Math.max(0,Math.min(1,value)):null;}
  function positiveFraction(value){var n=finiteFraction(value);return n!==null&&n>0?n:null;}
  function markerInitial(value){var text=String(value||'');try{var match=text.match(/[\p{L}\p{N}]/u);return match?match[0].toUpperCase():'?';}catch(_){var fallback=text.match(/[A-Za-z0-9]/);return fallback?fallback[0].toUpperCase():'?';}}
  function relativeAge(value){var when=Date.parse(value||'');if(!Number.isFinite(when))return 'recently';var delta=Math.max(0,Math.floor((Date.now()-when)/1000));if(delta<60)return 'just now';if(delta<3600)return Math.floor(delta/60)+'m ago';if(delta<86400)return Math.floor(delta/3600)+'h ago';return Math.floor(delta/86400)+'d ago';}
  function clamp(value,min,max){return Math.max(min,Math.min(max,value));}
  function composerPlacement(anchor,stageRect,composerRect){var gap=14,width=Math.max(240,composerRect.width||400),height=Math.max(180,composerRect.height||330),right=anchor.x+(anchor.w||0)+gap,left=anchor.x-width-gap,canRight=right+width<=stageRect.width-8,chosen=canRight?right:left;return {left:Math.round(clamp(chosen,8,Math.max(8,stageRect.width-width-8))),top:Math.round(clamp(anchor.y-(height/3),8,Math.max(8,stageRect.height-height-8))),side:canRight?'right':'left'};}
  function markerPreviewPlacement(anchor,stageRect,previewRect){var width=Math.max(120,previewRect.width||208),height=Math.max(48,previewRect.height||54),left=clamp(anchor.x-(width/2),8,Math.max(8,stageRect.width-width-8)),aboveTop=anchor.y-height-8,belowTop=anchor.y+(anchor.h||24)+8,aboveFits=aboveTop>=8,belowFits=belowTop+height<=stageRect.height-8,above=aboveFits||(!belowFits&&anchor.y>stageRect.height/2),top=clamp(above?aboveTop:belowTop,8,Math.max(8,stageRect.height-height-8));return {left:Math.round(left-anchor.x),top:Math.round(top-anchor.y),vertical:above?'above':'below'};}
  function pinFromRow(row){
    if(row.parent_id!=null)return null;var x=finiteFraction(row.anchor_x),y=finiteFraction(row.anchor_y);if(x===null||y===null)return null;
    var w=positiveFraction(row.anchor_w),h=positiveFraction(row.anchor_h),box=w!==null&&h!==null;
    if(box){w=Math.min(w,1-x);h=Math.min(h,1-y);if(w<=0||h<=0)box=false;}
    return {id:String(row.id),page:typeof row.anchor_page==='string'?row.anchor_page:null,path:typeof row.anchor_path==='string'?row.anchor_path.slice(0,512):null,x:x,y:y,w:box?w:null,h:box?h:null,approx:row.anchor_approx?1:0,stale:!!row.anchor_page_stale||Number(row.artifact_revision)!==(Number(shellConfig.revision)||1),author:authorLabel(row),body:String(row.body||''),createdAt:String(row.created_at||''),initial:markerInitial(authorLabel(row))};
  }
  feedbackRows.forEach(function(row){
    var pin=pinFromRow(row);if(pin){pins.push(pin);pinById[pin.id]=pin;}
  });
  function postToFrame(type,extra){try{if(frame&&frame.contentWindow)frame.contentWindow.postMessage(Object.assign({type:type},extra||{}),'*');}catch(_){}}
  function pinOnCurrentPage(pin){return !isBundle||pin.page===null||pin.page===currentPage;}
  function hideAllMarkers(){[].slice.call(overlay.querySelectorAll('.vanchor-marker')).forEach(function(marker){marker.hidden=true;});}
  function requestRepaint(){var pagePins=pins.filter(function(pin){return pinOnCurrentPage(pin)&&!pin.stale;});var anchors=pagePins.map(function(pin){return {id:pin.id,path:pin.path,x:pin.x,y:pin.y,w:pin.w,h:pin.h};});if(draftAnchor&&bridgeReady&&(!isBundle||draftAnchor.page===null||draftAnchor.page===currentPage))anchors.push({id:'__draft__',path:draftAnchor.path||null,x:draftAnchor.x,y:draftAnchor.y,w:draftAnchor.w||null,h:draftAnchor.h||null});postToFrame('anchor:repaint',{anchors:anchors});}
  function pinNumber(pin){return pins.indexOf(pin)+1;}
  function markerFor(pin){
    var marker=document.getElementById('vanchor-'+pin.id);if(marker)return marker;
    var box=pin.w!==null&&pin.h!==null,label=box?'Pinned section':'Pinned comment',excerpt=pin.body.slice(0,100);marker=document.createElement('button');marker.type='button';marker.id='vanchor-'+pin.id;marker.className='vanchor-marker'+(box?' vanchor-box':'')+(pin.stale?' stale':'');marker.textContent=pin.initial;if(box)marker.setAttribute('data-pin',pin.initial);marker.title=pin.stale?label+' · placed on an older revision':label;marker.setAttribute('aria-label','Open feedback from '+pin.author+': '+excerpt);var preview=document.createElement('span'),previewBody=document.createElement('span');preview.className='vanchor-preview';preview.appendChild(document.createTextNode(pin.author+' · '+relativeAge(pin.createdAt)));previewBody.textContent=excerpt;preview.appendChild(previewBody);marker.appendChild(preview);marker.addEventListener('focus',function(){marker.removeAttribute('data-preview-dismissed');});marker.addEventListener('pointerenter',function(){marker.removeAttribute('data-preview-dismissed');});
    marker.addEventListener('click',function(e){e.preventDefault();e.stopPropagation();fbOpen(true);var thread=feedbackThreads[pin.id],item=feedbackItems[pin.id];if(thread){if(item){item.classList.add('pin-focus');item.tabIndex=-1;}thread.scrollIntoView({block:'center'});setTimeout(function(){if(item){item.focus();item.classList.remove('pin-focus');}},180);}fbHint.textContent=label+(pin.stale?' · placed on an older revision.':'');});
    overlay.appendChild(marker);return marker;
  }
  function focusFeedback(id){
    fbOpen(true);var row=feedbackRows.find(function(entry){return String(entry.id)===id;}),item=feedbackItems[id],thread=row&&row.parent_id?feedbackThreads[row.parent_id]:feedbackThreads[id];
    if(item){item.classList.add('pin-focus');(thread||item).scrollIntoView({block:'center'});setTimeout(function(){item.classList.remove('pin-focus');},2200);}
    var pin=pinById[id];if(pin&&!pin.stale){var marker=markerFor(pin);marker.classList.add('pin-focus');setTimeout(function(){marker.classList.remove('pin-focus');},2200);if(isBundle&&pin.page){frame.src=bundleRawPrefix+pin.page.split('/').map(encodeURIComponent).join('/')+'?anchor=1'+versionQuery;}}else if(pin&&fbHint){fbHint.textContent='This anchor belongs to an older revision and is available in its feedback thread only.';}
  }
  var requestedFeedback=new URLSearchParams(window.location.search).get('feedback');
  if(requestedFeedback)setTimeout(function(){focusFeedback(requestedFeedback);},0);
  function positionLost(pin){
    var marker=document.getElementById('vanchor-'+pin.id);if(marker)marker.hidden=true;
    var item=feedbackItems[pin.id];if(item&&!item.querySelector('.vfb-anchor-state[data-lost]')){var note=document.createElement('span');note.className='vfb-anchor-state';note.setAttribute('data-lost','1');note.textContent='Position lost · shown in this thread';item.appendChild(note);}
  }
  function positionMarkerPreview(marker,x,y,height){var preview=marker.querySelector('.vanchor-preview'),rect=overlay.getBoundingClientRect();if(!preview||!rect.width)return;var placement=markerPreviewPlacement({x:x,y:y,h:height||0},rect,preview.getBoundingClientRect());preview.style.left=placement.left+'px';preview.style.top=placement.top+'px';preview.style.bottom='auto';preview.style.transform='translateY(.2rem)';marker.setAttribute('data-preview-side',placement.vertical);}
  function paintPosition(pin,x,y,width,height,lost){if(pin.stale||lost){positionLost(pin);return;}var marker=markerFor(pin),box=pin.w!==null&&pin.h!==null;marker.hidden=false;marker.style.left=Math.round(x)+'px';marker.style.top=Math.round(y)+'px';positionMarkerPreview(marker,x,y,box?height:24);if(box){marker.style.width=Math.max(1,Math.round(width))+'px';marker.style.height=Math.max(1,Math.round(height))+'px';}}
  function setCommentMode(next){commentMode=!!next;commentToggle.setAttribute('aria-pressed',commentMode?'true':'false');document.body.classList.toggle('vpinning',commentMode);if(!commentMode){overlay.classList.remove('fallback');postToFrame('anchor:pick-off');return;}closeShellMenus();if(inspector&&inspector.classList.contains('open'))inspectorClosePanel();if(bridgeReady){overlay.classList.remove('fallback');postToFrame('anchor:pick-on');requestRepaint();}else overlay.classList.add('fallback');}
  function openComposer(){if(!composer||!draftAnchor)return;composer.hidden=false;composer.classList.add('open');requestRepaint();setTimeout(function(){if(composerBody)composerBody.focus();},0);}
  function closeComposer(restore){if(!composer)return;composer.hidden=true;composer.classList.remove('open');if(restore&&composerReturnFocus&&composerReturnFocus.isConnected)composerReturnFocus.focus();composerReturnFocus=null;requestRepaint();}
  function positionComposer(){if(!composer||composer.hidden||!draftSelection||window.innerWidth<760)return;var stage=overlay.getBoundingClientRect(),box=composer.getBoundingClientRect(),placement=composerPlacement(draftSelection,stage,box);composer.style.left=placement.left+'px';composer.style.top=placement.top+'px';composer.style.right='auto';composer.style.bottom='auto';composer.setAttribute('data-side',placement.side);}
  function showDraftPosition(x,y,w,h,lost){var selection=overlay.querySelector('.vanchor-selection');if(lost||!draftAnchor){if(selection)selection.remove();return;}draftSelection={x:x,y:y,w:w,h:h};if(!selection){selection=document.createElement('div');selection.className='vanchor-selection';selection.id='vanchor-draft-selection';selection.setAttribute('aria-hidden','true');overlay.appendChild(selection);}selection.hidden=false;selection.style.left=Math.round(x)+'px';selection.style.top=Math.round(y)+'px';if(draftAnchor.w!==undefined&&draftAnchor.h!==undefined){selection.style.width=Math.max(1,Math.round(w))+'px';selection.style.height=Math.max(1,Math.round(h))+'px';}else{selection.classList.add('point');selection.style.width='14px';selection.style.height='14px';selection.style.transform='translate(-50%,-50%)';}positionComposer();}
  function startAnchoredComment(anchor){var x=finiteFraction(anchor&&anchor.x),y=finiteFraction(anchor&&anchor.y),width=positiveFraction(anchor&&anchor.w),height=positiveFraction(anchor&&anchor.h),box=width!==null&&height!==null;if(x===null||y===null)return;if(draftAnchor&&composerBody&&composerBody.value.trim()&&!window.confirm('Replace this anchored selection? This discards the current draft comment.'))return;if(box){width=Math.min(width,1-x);height=Math.min(height,1-y);if(width<=0||height<=0)return;}composerSavedRow=null;if(composerBody){composerBody.readOnly=false;composerBody.value='';}draftAnchor={version:Number(anchor.version)||2,kind:typeof anchor.kind==='string'?anchor.kind.slice(0,32):(box?'region':'point'),x:x,y:y,page:isBundle?(typeof anchor.page==='string'?anchor.page:currentPage):null,path:typeof anchor.path==='string'?anchor.path.slice(0,512):undefined,nodeId:typeof anchor.nodeId==='string'?anchor.nodeId.slice(0,128):undefined,quote:typeof anchor.quote==='string'?anchor.quote.slice(0,240):undefined,approx:anchor.approx?1:0};if(box){draftAnchor.w=width;draftAnchor.h=height;}setCommentMode(false);composerReturnFocus=commentToggle;composerSummary.textContent=draftAnchor.approx?(box?'Approximate section selected.':'Approximate pin selected.'):(box?'Pinned section selected.':'Pinned location selected.');composerStatus.textContent='';composerSave.textContent='Add comment';composerSave.disabled=false;openComposer();}
  if(commentToggle)commentToggle.addEventListener('click',function(){if(draftAnchor&&composer&&composer.hidden){openComposer();return;}setCommentMode(!commentMode);});
  document.addEventListener('keydown',function(e){if(e.key==='Escape'&&commentMode){e.preventDefault();setCommentMode(false);if(commentToggle&&commentToggle.isConnected)commentToggle.focus();}});
  function fallbackPoint(e){var rect=overlay.getBoundingClientRect();if(!rect.width||!rect.height)return null;return {x:(e.clientX-rect.left)/rect.width,y:(e.clientY-rect.top)/rect.height};}
  function clearFallbackSelection(){var selection=overlay.querySelector('.vanchor-selection');if(selection)selection.remove();}
  function drawFallbackSelection(a,b){var selection=overlay.querySelector('.vanchor-selection');if(!selection){selection=document.createElement('div');selection.className='vanchor-selection';overlay.appendChild(selection);}selection.style.left=Math.min(a.x,b.x)+'px';selection.style.top=Math.min(a.y,b.y)+'px';selection.style.width=Math.abs(a.x-b.x)+'px';selection.style.height=Math.abs(a.y-b.y)+'px';}
  overlay.addEventListener('pointerdown',function(e){if(!commentMode||bridgeReady||e.button!==0||e.target.closest('.vanchor-marker'))return;var point=fallbackPoint(e);if(!point)return;e.preventDefault();e.stopPropagation();fallbackDrag={id:e.pointerId,x:e.clientX,y:e.clientY,moved:false};try{overlay.setPointerCapture(e.pointerId);}catch(_){};});
  overlay.addEventListener('pointermove',function(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;e.preventDefault();e.stopPropagation();if(Math.abs(e.clientX-fallbackDrag.x)>4||Math.abs(e.clientY-fallbackDrag.y)>4){fallbackDrag.moved=true;drawFallbackSelection({x:fallbackDrag.x-overlay.getBoundingClientRect().left,y:fallbackDrag.y-overlay.getBoundingClientRect().top},{x:e.clientX-overlay.getBoundingClientRect().left,y:e.clientY-overlay.getBoundingClientRect().top});}});
  function finishFallbackDrag(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;var start=fallbackDrag;fallbackDrag=null;clearFallbackSelection();e.preventDefault();e.stopPropagation();var end=fallbackPoint(e);if(!end)return;if(start.moved){var rect=overlay.getBoundingClientRect(),sx=(start.x-rect.left)/rect.width,sy=(start.y-rect.top)/rect.height;startAnchoredComment({x:Math.min(sx,end.x),y:Math.min(sy,end.y),w:Math.abs(end.x-sx),h:Math.abs(end.y-sy),approx:1});}else startAnchoredComment({x:end.x,y:end.y,approx:1});}
  overlay.addEventListener('pointerup',finishFallbackDrag);overlay.addEventListener('pointercancel',function(e){if(fallbackDrag&&e.pointerId===fallbackDrag.id){fallbackDrag=null;clearFallbackSelection();}});
  if(frame)frame.addEventListener('load',function(){clearTimeout(bridgeTimer);bridgeReady=false;currentPage=null;hideAllMarkers();overlay.classList.remove('fallback');bridgeTimer=setTimeout(function(){if(!bridgeReady&&commentMode)overlay.classList.add('fallback');},800);});
  window.addEventListener('resize',function(){requestRepaint();positionComposer();});window.addEventListener('scroll',function(){requestRepaint();positionComposer();},true);
  window.addEventListener('beforeunload',function(event){if(draftAnchor&&!composerSavedRow&&composerBody&&composerBody.value.trim()){event.preventDefault();event.returnValue='You have an unsaved anchored comment.';return event.returnValue;}});
  var outboundPanel=null,outboundHost=null,outboundConfirm=null,outboundUrl=null;
  function parseOutboundHref(href){if(typeof href!=='string')return null;try{var url=new URL(href);return url.protocol==='http:'||url.protocol==='https:'?url:null;}catch(_){return null;}}
  function closeOutbound(){outboundUrl=null;if(!outboundPanel)return;outboundPanel.classList.remove('open');outboundPanel.setAttribute('aria-hidden','true');}
  function ensureOutboundPanel(){
    if(outboundPanel)return;
    outboundPanel=document.createElement('aside');outboundPanel.className='vinspector vmodal';outboundPanel.setAttribute('role','dialog');outboundPanel.setAttribute('aria-modal','true');outboundPanel.setAttribute('aria-label','Confirm external link');outboundPanel.setAttribute('aria-hidden','true');
    var head=document.createElement('div'),title=document.createElement('h2'),close=document.createElement('button'),content=document.createElement('div'),message=document.createElement('p'),actions=document.createElement('div'),cancel=document.createElement('button');
    head.className='vfb-head';title.textContent='Open external link?';close.type='button';close.className='vfb-close';close.textContent='×';close.setAttribute('aria-label','Close external link confirmation');head.appendChild(title);head.appendChild(close);
    content.className='vfb-list';message.textContent='You are being sent to ';outboundHost=document.createElement('strong');message.appendChild(outboundHost);content.appendChild(message);
    actions.className='vfb-actions';cancel.type='button';cancel.textContent='Cancel';outboundConfirm=document.createElement('button');outboundConfirm.type='button';outboundConfirm.className='vfb-send';outboundConfirm.textContent='Open link';actions.appendChild(cancel);actions.appendChild(outboundConfirm);
    outboundPanel.appendChild(head);outboundPanel.appendChild(content);outboundPanel.appendChild(actions);document.body.appendChild(outboundPanel);
    close.addEventListener('click',closeOutbound);cancel.addEventListener('click',closeOutbound);outboundConfirm.addEventListener('click',function(){var url=outboundUrl;closeOutbound();if(url)window.open(url.href,'_blank','noopener');});
  }
  function showOutbound(url){ensureOutboundPanel();outboundUrl=url;outboundHost.textContent=url.host;outboundPanel.classList.add('open');outboundPanel.setAttribute('aria-hidden','false');outboundConfirm.focus();}
  window.addEventListener('message',function(event){
    if(!frame||event.source!==frame.contentWindow)return;var data=event.data;if(!data||typeof data!=='object')return;
    if(data.type!=='anchor:ready'&&data.type!=='anchor:picked'&&data.type!=='anchor:positions'&&data.type!=='anchor:navigate')return;
    if(data.type==='anchor:navigate'){var url=parseOutboundHref(data.href);if(url)showOutbound(url);return;}
    if(data.type==='anchor:ready'){var nextPage=isBundle&&typeof data.page==='string'?data.page:null;if(draftAnchor&&composerBody&&composerBody.value.trim()&&draftAnchor.page!==nextPage&&!window.confirm('Move away from this selected anchor? Your draft comment will remain.')){if(isBundle&&draftAnchor.page)frame.src=bundleRawPrefix+draftAnchor.page.split('/').map(encodeURIComponent).join('/')+'?anchor=1'+versionQuery;return;}currentPage=nextPage;bridgeReady=true;hideAllMarkers();if(commentMode){overlay.classList.remove('fallback');postToFrame('anchor:pick-on');}requestRepaint();return;}
    if(data.type==='anchor:picked'){startAnchoredComment(data);return;}if(!Array.isArray(data.anchors))return;
    data.anchors.slice(0,200).forEach(function(pos){if(!pos||typeof pos!=='object'||typeof pos.id!=='string')return;if(pos.id==='__draft__'){if(pos.lost===true){showDraftPosition(0,0,0,0,true);return;}if(typeof pos.x!=='number'||typeof pos.y!=='number'||!Number.isFinite(pos.x)||!Number.isFinite(pos.y))return;if(draftAnchor&&draftAnchor.w!==undefined&&(!Number.isFinite(pos.w)||!Number.isFinite(pos.h)||pos.w<=0||pos.h<=0))return;showDraftPosition(pos.x,pos.y,pos.w||0,pos.h||0,false);return;}var pin=pinById[pos.id];if(!pin||pin.stale||!pinOnCurrentPage(pin))return;if(pos.lost===true){paintPosition(pin,0,0,0,0,true);return;}if(typeof pos.x!=='number'||typeof pos.y!=='number'||!Number.isFinite(pos.x)||!Number.isFinite(pos.y))return;if(pin.w!==null&&pin.h!==null){if(typeof pos.w!=='number'||typeof pos.h!=='number'||!Number.isFinite(pos.w)||!Number.isFinite(pos.h)||pos.w<=0||pos.h<=0)return;paintPosition(pin,pos.x,pos.y,pos.w,pos.h,false);}else paintPosition(pin,pos.x,pos.y,0,0,false);});
  });
  function promptField(label,value){return label+': '+(value==null||value===''?'(none)':String(value));}
  function buildAnchorPrompt(input){var source=input||{},anchor=source.anchor||{},draft=!!source.draft,artifact=String(source.artifactId||''),currentRevision=Number(source.currentRevision)||1,bundle=!!source.isBundle,revision=Number(anchor.artifact_revision||currentRevision)||1,bundlePage=anchor.anchor_page!=null?anchor.anchor_page:anchor.page,kind=anchor.anchor_kind||anchor.kind||(anchor.w!=null?'region':'point'),nodeId=anchor.anchor_node_id||anchor.nodeId,quote=anchor.anchor_quote||anchor.quote,version=Number(anchor.anchor_version||anchor.version)||((nodeId||quote||kind)?2:1),approx=anchor.anchor_approx||anchor.approx,stale=!!anchor.anchor_page_stale||(!draft&&revision!==currentRevision),filePath=bundle?(bundlePage?'/files/'+String(bundlePage):'/files/(bundle entry page)'):'';var read=stale?'Do not switch this stale feedback to the latest revision. Read the exact revision below.':'Read the canonical current revision below.';return ['Artifact MCP review handoff ('+(draft?'draft':'saved')+' anchored feedback)',promptField('Connector','Artifact MCP'),promptField('State',draft?'draft':'saved'),'',promptField('Artifact ID',artifact),promptField('Canonical artifact revision',revision),draft?'Feedback ID: (draft; not persisted yet)':promptField('Feedback ID',anchor.id),promptField('Bundle page',bundlePage),promptField('Bundle file path',filePath),promptField('Selector/path',anchor.anchor_path||anchor.path),promptField('Node ID',nodeId),promptField('Quote',quote),promptField('Anchor kind',kind),promptField('Anchor version',version),promptField('Normalized bounds','x='+((anchor.anchor_x!=null?anchor.anchor_x:anchor.x))+' y='+((anchor.anchor_y!=null?anchor.anchor_y:anchor.y))+' w='+((anchor.anchor_w!=null?anchor.anchor_w:anchor.w))+' h='+((anchor.anchor_h!=null?anchor.anchor_h:anchor.h))),promptField('Approximate',approx?'yes':'no'),promptField('Stale',stale?'yes':'no'),'',draft?promptField('Draft comment',source.body):promptField('Saved comment',source.body),'',draft?'1. Call Artifact MCP list_feedback for artifact '+artifact+' to compare this draft against active feedback.':'1. Call Artifact MCP list_feedback for artifact '+artifact+' and locate feedback '+anchor.id+'.','2. '+read,'3. Prefer resources/read with URI artifact://'+artifact+'/revisions/'+revision+filePath+'.','4. If resources/read is unavailable, use paged read_artifact. A single 65,536-byte read may be incomplete; request subsequent pages before drawing conclusions.'].join('\n');}
  function copyText(text,done,failed){if(navigator.clipboard&&navigator.clipboard.writeText){navigator.clipboard.writeText(text).then(done).catch(function(){fallbackCopy(text,done,failed);});}else fallbackCopy(text,done,failed);}
  function fallbackCopy(text,done,failed){var area=document.createElement('textarea');area.value=text;area.setAttribute('readonly','');area.style.position='fixed';area.style.opacity='0';document.body.appendChild(area);area.select();try{if(document.execCommand&&document.execCommand('copy')){area.remove();done();return;}}catch(_){}area.remove();failed();}
  function copyAnchorPrompt(row,draft,button){var anchor=row||draftAnchor||{},text=buildAnchorPrompt({anchor:anchor,draft:draft,body:draft?(composerBody&&composerBody.value):anchor.body,artifactId:artifactId,currentRevision:Number(shellConfig.revision),isBundle:isBundle});copyText(text,function(){if(button){var old=button.textContent;button.textContent='Copied';setTimeout(function(){button.textContent=old;},1400);}},function(){if(draft&&composerStatus){composerStatus.textContent='Could not copy the prompt. Your draft is still here; select and copy it manually.';composerStatus.classList.add('error');}else if(fbHint){fbHint.textContent='Could not copy prompt.';fbHint.classList.add('error');}});return text;}
  if(window.__artifactMcpTestHooks){window.__artifactMcpTestHooks.buildAnchorPrompt=buildAnchorPrompt;window.__artifactMcpTestHooks.composerPlacement=composerPlacement;window.__artifactMcpTestHooks.markerPreviewPlacement=markerPreviewPlacement;window.__artifactMcpTestHooks.relativeAge=relativeAge;}
  function updateCount(delta){var n=Math.max(0,(parseInt((fbCounts[0]||{}).textContent,10)||0)+delta);fbCounts.forEach(function(count){count.textContent=n;count.hidden=!n;});}
  function appendFeedback(row){
    var empty=fbList.querySelector('.vfb-empty');if(empty)empty.remove();var item,thread;
    if(row.parent_id){thread=feedbackThreads[row.parent_id];if(!thread)return;var replies=thread.querySelector('.vfb-replies');var holder=document.createElement('div');holder.innerHTML=itemHtml(row,true);item=holder.firstChild;replies.appendChild(item);}
    else{thread=document.createElement('section');thread.className='vfb-thread';thread.setAttribute('data-thread-id',row.id);thread.innerHTML=itemHtml(row,true)+'<div class="vfb-replies"></div>'+replyFormHtml(row.id);fbList.appendChild(thread);feedbackThreads[row.id]=thread;item=thread.querySelector('.vfb-item');}
    feedbackItems[row.id]=item;feedbackRows.push(row);
    var pin=pinFromRow(row);if(pin){pins.push(pin);pinById[pin.id]=pin;requestRepaint();}
    updateCount(1);fbList.scrollTop=fbList.scrollHeight;
  }
  function sendFeedback(text,parentId,anchor){return fetch('/'+artifactId+'/feedback',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({body:text,parent_id:parentId||undefined,anchor:parentId?undefined:(anchor||undefined),anchor_page:anchor&&anchor.page})}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not send feedback');return d;});});}
  if(composerDismiss)composerDismiss.addEventListener('click',function(){closeComposer(true);});
  if(composerCopy)composerCopy.addEventListener('click',function(){copyAnchorPrompt(composerSavedRow,!!(!composerSavedRow),composerCopy);});
  document.addEventListener('keydown',function(event){if(event.key!=='Escape')return;if(composer&&!composer.hidden){event.preventDefault();closeComposer(true);return;}var active=document.activeElement;if(active&&active.classList&&active.classList.contains('vanchor-marker')){event.preventDefault();active.setAttribute('data-preview-dismissed','1');active.focus();}});
  if(composerSave)composerSave.addEventListener('click',function(){var text=(composerBody&&composerBody.value||'').trim();if(!text||!draftAnchor||composerSavedRow){if(!composerSavedRow&&composerStatus){composerStatus.textContent='Write a comment before saving.';composerStatus.classList.add('error');}return;}composerSave.disabled=true;composerStatus.classList.remove('error');composerStatus.textContent='Saving…';sendFeedback(text,null,draftAnchor).then(function(row){var saved=Object.assign({},draftAnchor,row,{artifact_revision:Number(row.artifact_revision||shellConfig.revision)});composerSavedRow=saved;draftAnchor=null;showDraftPosition(0,0,0,0,true);appendFeedback(saved);composerSummary.textContent='Saved feedback · '+saved.id+' · v'+saved.artifact_revision;composerStatus.textContent='Saved. Copy prompt now includes the feedback ID and exact revision.';composerSave.textContent='Saved';composerSave.disabled=true;composerBody.readOnly=true;requestRepaint();}).catch(function(error){composerStatus.textContent=error.message||'Could not save anchored feedback.';composerStatus.classList.add('error');composerSave.disabled=false;});});
  if(fbForm)fbForm.addEventListener('submit',function(e){e.preventDefault();var text=(fbBody.value||'').trim();if(!text){fbHint.textContent='Write something first.';fbHint.classList.add('error');return;}var btn=fbForm.querySelector('.vfb-send');btn.disabled=true;fbHint.classList.remove('error');fbHint.textContent='Sending…';sendFeedback(text,null,draftAnchor).then(function(d){appendFeedback(d);fbBody.value='';draftAnchor=null;fbHint.textContent='Sent to the author.';}).catch(function(err){fbHint.textContent=err.message||'Could not send feedback.';fbHint.classList.add('error');}).finally(function(){btn.disabled=false;});});
  fbList.addEventListener('submit',function(e){var form=e.target.closest('.vfb-reply-form');if(!form)return;e.preventDefault();var input=form.querySelector('textarea'),text=(input.value||'').trim(),button=form.querySelector('button');if(!text)return;button.disabled=true;sendFeedback(text,form.getAttribute('data-parent-id')).then(function(d){appendFeedback(d);input.value='';}).catch(function(err){fbHint.textContent=err.message||'Could not send reply.';fbHint.classList.add('error');}).finally(function(){button.disabled=false;});});
  function forgetPin(id){var marker=document.getElementById('vanchor-'+id);if(marker)marker.remove();delete pinById[id];pins=pins.filter(function(pin){return pin.id!==id;});}
  fbList.addEventListener('click',function(e){var copy=e.target.closest('[data-copy-prompt]');if(copy){var copyId=copy.getAttribute('data-copy-prompt'),copyRow=feedbackRows.find(function(row){return String(row.id)===copyId;});if(copyRow)copyAnchorPrompt(copyRow,false,copy);return;}var button=e.target.closest('[data-feedback-action]');if(!button)return;var item=button.closest('.vfb-item[data-id]'),id=item&&item.getAttribute('data-id'),action=button.getAttribute('data-feedback-action');if(!id)return;button.disabled=true;var url='/'+artifactId+'/feedback/'+encodeURIComponent(id)+(action==='resolve'?'/resolve':'');fetch(url,{method:action==='resolve'?'POST':'DELETE'}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not update feedback');return d;});}).then(function(){if(action==='resolve'){if(!item.classList.contains('resolved')){item.classList.add('resolved');updateCount(-1);}var resolve=item.querySelector('.vfb-resolve');if(resolve)resolve.remove();var stamp=item.querySelector('.vfb-m span:last-child');if(stamp&&!stamp.querySelector('.vfb-res'))stamp.insertAdjacentHTML('beforeend',' &middot; <span class="vfb-res">Resolved</span>');return;}var thread=item.closest('.vfb-thread');var isTop=!!(thread&&item===feedbackItems[thread.getAttribute('data-thread-id')]);var items=isTop?[].slice.call(thread.querySelectorAll('.vfb-item[data-id]')):[item];items.forEach(function(node){var nodeId=node.getAttribute('data-id');if(!node.classList.contains('resolved'))updateCount(-1);forgetPin(nodeId);delete feedbackItems[nodeId];});if(isTop&&thread){delete feedbackThreads[thread.getAttribute('data-thread-id')];thread.remove();}else item.remove();if(!fbList.querySelector('.vfb-thread'))fbList.innerHTML='<div class="vfb-empty">No feedback yet. Leave the first note for the author.</div>';requestRepaint();}).catch(function(err){fbHint.textContent=err.message||'Could not update feedback.';fbHint.classList.add('error');button.disabled=false;});});

  // Viewer-safe discussion status loads independently so a transient status failure never
  // blocks the artifact shell. Only the server-rendered management capability gets controls.
  var discussion=document.getElementById('vdiscussion'),discussionState=document.getElementById('vdiscussion-state'),discussionCopy=document.getElementById('vdiscussion-copy'),discussionActions=document.getElementById('vdiscussion-actions'),discussionStatus=document.getElementById('vdiscussion-status'),canManageDiscussion=shellConfig.canManageDiscussion==='1';
  var discussionStates={
    local:{label:'Artifact MCP only',copy:'Discussion is kept in Artifact MCP.'},
    recovering:{label:'Recovering notification',copy:'Artifact MCP is looking for the original notification using the selected webhook and exact canonical URL.'},
    pending:{label:'Preparing thread',copy:'New activity is being prepared for outbound Discord threading.'},
    connected:{label:'Using organization default',copy:'New feedback follows the organization Discord threading policy.'},
    connecting:{label:'Connecting two-way sync',copy:'Artifact MCP is establishing the guarded Discord inbound connection.'},
    ready:{label:'Two-way Discord sync',copy:'Human replies in the mapped Discord thread are imported with Discord identity attribution.'},
    degraded:{label:'Two-way sync degraded',copy:'Artifact MCP feedback remains canonical while Discord inbound sync recovers.'},
    unavailable:{label:'Threading unavailable',copy:'Artifact MCP feedback remains available while Discord threading is unavailable.'},
    failed:{label:'Needs attention',copy:'Discord threading needs attention. Artifact MCP feedback remains canonical and available.'}
  };
  function discussionMessage(text,bad){if(!discussionStatus)return;discussionStatus.textContent=text||'';discussionStatus.classList.toggle('error',!!bad);}
  function discussionButton(action,label){var button=document.createElement('button');button.type='button';button.dataset.discussionAction=action;button.textContent=label;return button;}
  function renderDiscussion(value,focusAction){if(!discussion)return;var state=discussionStates[value&&value.state]||discussionStates.local,override=value&&value.overrideMode||'inherit';discussionState.textContent=override==='artifact_only'?'Artifact MCP only':override==='discord_two_way'?'Two-way Discord sync':state.label;discussionCopy.textContent=override==='artifact_only'?'This artifact is explicitly kept in Artifact MCP, even when organization threading is enabled.':override==='discord_two_way'?'Human Discord replies are imported as provider-attributed feedback. Artifact MCP remains canonical.':state.copy;if(value&&value.actionableError)discussionMessage(value.actionableError,true);if(!discussionActions)return;discussionActions.hidden=true;discussionActions.textContent='';if(!canManageDiscussion)return;if(override!=='inherit')discussionActions.appendChild(discussionButton('inherit','Use organization default'));if(override!=='artifact_only')discussionActions.appendChild(discussionButton('artifact_only','Keep discussion in Artifact MCP'));if(override!=='discord_two_way')discussionActions.appendChild(discussionButton('discord_two_way','Enable two-way Discord sync'));discussionActions.hidden=false;if(focusAction){var focusButton=discussionActions.querySelector('[data-discussion-action="'+focusAction+'"]')||discussionActions.querySelector('button');if(focusButton)focusButton.focus();}}
  function loadDiscussion(focusAction){if(!discussion)return;fetch('/'+artifactId+'/discussion/override').then(function(response){return response.json().then(function(body){return {ok:response.ok,body:body};});}).then(function(result){if(!result.ok)throw new Error(result.body&&result.body.error||'Could not load discussion status.');renderDiscussion(result.body,focusAction);if(!result.body.actionableError)discussionMessage('');}).catch(function(error){discussionState.textContent='Status unavailable';discussionCopy.textContent='Discussion status could not be loaded. Artifact content and feedback remain available.';if(discussionActions){discussionActions.hidden=true;discussionActions.textContent='';}discussionMessage(error.message||'Could not load discussion status.',true);});}
  if(discussionActions)discussionActions.addEventListener('click',function(event){var button=event.target.closest('[data-discussion-action]');if(!button)return;var action=button.dataset.discussionAction,all=[].slice.call(discussionActions.querySelectorAll('button')),working=action==='inherit'?'Restoring outbound organization default…':action==='artifact_only'?'Keeping discussion in Artifact MCP…':'Enabling guarded two-way sync…',done=action==='inherit'?'New comments will follow the organization outbound default.':action==='artifact_only'?'This artifact will keep discussion in Artifact MCP.':'Two-way Discord sync is enabled for this mapped thread.';all.forEach(function(item){item.disabled=true;});discussionMessage(working);fetch('/'+artifactId+'/discussion/override',{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify({override:action})}).then(function(response){return response.json().then(function(body){return {ok:response.ok,body:body};});}).then(function(result){if(!result.ok)throw new Error(result.body&&result.body.error||'Could not update discussion settings.');renderDiscussion(result.body,action);discussionMessage(done);}).catch(function(error){discussionMessage(error.message||'Could not update discussion settings.',true);all.forEach(function(item){item.disabled=false;});button.focus();});});
  loadDiscussion();

  // Category editor (top bar)
  var vcat=document.getElementById('vcat'),vcatEdit=document.getElementById('vcat-edit'),vcatInput=document.getElementById('vcat-input');
  function vcatShow(edit){vcat.hidden=edit;vcatEdit.hidden=!edit;if(edit){vcatInput.focus();vcatInput.select();}}
  if(vcat){vcat.addEventListener('click',function(){vcatShow(true);});}
  if(vcatEdit){
    vcatInput.addEventListener('keydown',function(e){if(e.key==='Escape'){e.preventDefault();vcatShow(false);}});
    vcatEdit.addEventListener('submit',function(e){
      e.preventDefault();
      var val=(vcatInput.value||'').trim();var save=vcatEdit.querySelector('.vcat-save');save.disabled=true;
      fetch('/'+artifactId+'/category',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({category:val})})
        .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Failed');return d;});})
        .then(function(d){vcat.textContent=d.category?d.category:'Add category';vcat.setAttribute('data-set',d.category?'1':'0');vcatInput.value=d.category||'';vcatShow(false);})
        .catch(function(){vcatShow(false);})
        .finally(function(){save.disabled=false;});
    });
  }

  // Version history drawer
  var histToggle=document.querySelector('[data-inspector-open="history"]'),histList=document.getElementById('vhist-list');
  var histLoaded=false,curTitle=configLiteral('title'),curBytes=Number(shellConfig.bytes)||0;
  function kb(b){return (Math.round((b||0)/102.4)/10)+' KB';}
  function histRow(r,isCurrent){
    var when=(r.created_at||'').replace('T',' ').slice(0,16);
    var view='/raw/'+artifactId+'/rev/'+r.revision+(isBundle?'/':'');
    return '<div class="vhist-item'+(isCurrent?' current':'')+'">'+
      '<div class="vh-m"><strong>v'+r.revision+'</strong>'+(isCurrent?'<span class="vh-cur">current</span>':'<span class="vh-when">'+fesc(when)+'</span>')+'</div>'+
      '<div class="vh-t">'+fesc(r.title)+'<span class="vh-size">'+kb(r.bytes)+'</span></div>'+
      '<div class="vh-actions">'+(isCurrent?'':'<a class="vh-view" href="'+view+'" target="_blank" rel="noopener">View</a><button class="vh-restore" type="button" data-rev="'+r.revision+'">Restore</button>')+'</div>'+
    '</div>';
  }
  function histLoad(){
    histList.innerHTML='<div class="vfb-empty">Loading…</div>';
    fetch('/'+artifactId+'/history').then(function(r){return r.json();}).then(function(d){
      histLoaded=true;
      var cur=d.current||1,revs=d.revisions||[];
      var html=histRow({revision:cur,title:curTitle,bytes:curBytes},true);
      if(!revs.length){html+='<div class="vfb-empty" style="border:0;margin-top:.4rem">No earlier versions yet. Each update adds one here.</div>';}
      revs.forEach(function(r){html+=histRow(r,false);});
      histList.innerHTML=html;
    }).catch(function(){histLoaded=false;histList.innerHTML='<div class="vfb-empty">Could not load history.</div>';});
  }
  function histOpen(open){if(open)inspectorOpen('history',histToggle);else inspectorClosePanel();}
  if(histList){histList.addEventListener('click',function(e){
    var b=e.target.closest('.vh-restore');if(!b)return;
    var rev=b.getAttribute('data-rev');
    if(!confirm('Restore v'+rev+'? It becomes a NEW revision at the same URL — nothing is lost.'))return;
    b.disabled=true;b.textContent='Restoring…';
    fetch('/'+artifactId+'/restore',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({revision:Number(rev)})})
      .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Restore failed');return d;});})
      .then(function(){location.reload();})
      .catch(function(err){b.disabled=false;b.textContent='Restore';alert(err.message||'Restore failed');});
  });}

  // Admin-only audience drawer. Its markup is absent for regular org viewers.
  var viewToggle=document.getElementById('vview-toggle');
  function viewOpen(open){if(!document.getElementById('inspector-audience'))return;if(open)inspectorOpen('audience',viewToggle);else inspectorClosePanel();}
})();
