  function pinOnCurrentPage(pin){return !isBundle||pin.page===null||pin.page===currentPage;}
  function hideAllMarkers(){[].slice.call(overlay.querySelectorAll('.vanchor-marker')).forEach(function(marker){marker.hidden=true;});}
  var repaintFrame=0,repaintDirty=false,anchorPayload=null,viewportFrame=0;
  function invalidateAnchorPayload(){anchorPayload=null;}
  function repaintNow(){
    repaintFrame=0;if(!repaintDirty)return;repaintDirty=false;
    if(!anchorPayload){
      var pagePins=pins.filter(function(pin){return pinOnCurrentPage(pin)&&!pin.stale;});
      anchorPayload=pagePins.map(function(pin){return {id:pin.id,path:pin.path,x:pin.x,y:pin.y,w:pin.w,h:pin.h};});
    }
    var anchors=anchorPayload.slice();
    if(draftAnchor&&bridgeReady&&(!isBundle||draftAnchor.page===null||draftAnchor.page===currentPage))anchors.push({id:'__draft__',path:draftAnchor.path||null,x:draftAnchor.x,y:draftAnchor.y,w:draftAnchor.w||null,h:draftAnchor.h||null});
    postToFrame('anchor:repaint',{anchors:anchors});
  }
  function requestRepaint(){
    repaintDirty=true;if(repaintFrame)return;
    repaintFrame=typeof window.requestAnimationFrame==='function'?window.requestAnimationFrame(repaintNow):setTimeout(repaintNow,0);
  }
  function scheduleViewportUpdate(){
    if(viewportFrame)return;
    viewportFrame=typeof window.requestAnimationFrame==='function'?window.requestAnimationFrame(function(){viewportFrame=0;requestRepaint();positionComposer();}):setTimeout(function(){viewportFrame=0;requestRepaint();positionComposer();},0);
  }
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
    var pin=pinById[id];if(pin&&!pin.stale){var marker=markerFor(pin);marker.classList.add('pin-focus');setTimeout(function(){marker.classList.remove('pin-focus');},2200);if(isBundle&&pin.page){frame.src=bundleRawPrefix+pin.page.split('/').map(encodeURIComponent).join('/')+'?anchor=1&reader=1'+versionQuery;}}else if(pin&&fbHint){fbHint.textContent='This anchor belongs to an older revision and is available in its feedback thread only.';}
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
  function draftAnchorFromSelection(anchor,x,y,width,height,box){var kind=anchor&&anchor.kind;return Object.assign({version:2,kind:kind==='element'||kind==='region'?kind:(box?'region':'element'),x:x,y:y,page:isBundle?(typeof anchor.page==='string'?anchor.page:currentPage):null,path:typeof anchor.path==='string'?anchor.path.slice(0,512):'',nodeId:typeof anchor.nodeId==='string'?anchor.nodeId.slice(0,128):undefined,quote:typeof anchor.quote==='string'?anchor.quote.slice(0,240):undefined,approx:typeof anchor.approx==='boolean'?anchor.approx:!!anchor.approx},box?{w:width,h:height}:{});}
  function startAnchoredComment(anchor){var x=finiteFraction(anchor&&anchor.x),y=finiteFraction(anchor&&anchor.y),width=positiveFraction(anchor&&anchor.w),height=positiveFraction(anchor&&anchor.h),box=width!==null&&height!==null;if(x===null||y===null)return;if(draftAnchor&&composerBody&&composerBody.value.trim()&&!window.confirm('Replace this anchored selection? This discards the current draft comment.'))return;if(box){width=Math.min(width,1-x);height=Math.min(height,1-y);if(width<=0||height<=0)return;}composerSavedRow=null;if(composerBody){composerBody.readOnly=false;composerBody.value='';}draftAnchor=draftAnchorFromSelection(anchor,x,y,width,height,box);setCommentMode(false);composerReturnFocus=commentToggle;composerSummary.textContent=draftAnchor.approx?(box?'Approximate section selected.':'Approximate pin selected.'):(box?'Pinned section selected.':'Pinned location selected.');composerStatus.textContent='';composerSave.textContent='Add comment';composerSave.disabled=false;openComposer();}
  if(commentToggle)commentToggle.addEventListener('click',function(){if(draftAnchor&&composer&&composer.hidden){openComposer();return;}setCommentMode(!commentMode);});
  document.addEventListener('keydown',function(e){if(e.key==='Escape'&&commentMode){e.preventDefault();setCommentMode(false);if(commentToggle&&commentToggle.isConnected)commentToggle.focus();}});
  function fallbackPoint(e){var rect=overlay.getBoundingClientRect();if(!rect.width||!rect.height)return null;return {x:(e.clientX-rect.left)/rect.width,y:(e.clientY-rect.top)/rect.height};}
  function clearFallbackSelection(){var selection=overlay.querySelector('.vanchor-selection');if(selection)selection.remove();}
  function drawFallbackSelection(a,b){var selection=overlay.querySelector('.vanchor-selection');if(!selection){selection=document.createElement('div');selection.className='vanchor-selection';overlay.appendChild(selection);}selection.style.left=Math.min(a.x,b.x)+'px';selection.style.top=Math.min(a.y,b.y)+'px';selection.style.width=Math.abs(a.x-b.x)+'px';selection.style.height=Math.abs(a.y-b.y)+'px';}
  overlay.addEventListener('pointerdown',function(e){if(!commentMode||bridgeReady||e.button!==0||e.target.closest('.vanchor-marker'))return;var point=fallbackPoint(e);if(!point)return;e.preventDefault();e.stopPropagation();fallbackDrag={id:e.pointerId,x:e.clientX,y:e.clientY,moved:false};try{overlay.setPointerCapture(e.pointerId);}catch(_){};});
  overlay.addEventListener('pointermove',function(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;e.preventDefault();e.stopPropagation();if(Math.abs(e.clientX-fallbackDrag.x)>4||Math.abs(e.clientY-fallbackDrag.y)>4){fallbackDrag.moved=true;drawFallbackSelection({x:fallbackDrag.x-overlay.getBoundingClientRect().left,y:fallbackDrag.y-overlay.getBoundingClientRect().top},{x:e.clientX-overlay.getBoundingClientRect().left,y:e.clientY-overlay.getBoundingClientRect().top});}});
  function finishFallbackDrag(e){if(!fallbackDrag||e.pointerId!==fallbackDrag.id)return;var start=fallbackDrag;fallbackDrag=null;clearFallbackSelection();e.preventDefault();e.stopPropagation();var end=fallbackPoint(e);if(!end)return;if(start.moved){var rect=overlay.getBoundingClientRect(),sx=(start.x-rect.left)/rect.width,sy=(start.y-rect.top)/rect.height;startAnchoredComment({kind:'region',path:'',x:Math.min(sx,end.x),y:Math.min(sy,end.y),w:Math.abs(end.x-sx),h:Math.abs(end.y-sy),approx:true});}else startAnchoredComment({kind:'element',path:'',x:end.x,y:end.y,approx:true});}
  overlay.addEventListener('pointerup',finishFallbackDrag);overlay.addEventListener('pointercancel',function(e){if(fallbackDrag&&e.pointerId===fallbackDrag.id){fallbackDrag=null;clearFallbackSelection();}});
  if(frame)frame.addEventListener('load',function(){clearTimeout(bridgeTimer);bridgeReady=false;currentPage=null;hideAllMarkers();overlay.classList.remove('fallback');postToFrame('anchor:hello');bridgeTimer=setTimeout(function(){if(!bridgeReady&&commentMode)overlay.classList.add('fallback');},800);});
  window.addEventListener('resize',scheduleViewportUpdate);window.addEventListener('scroll',scheduleViewportUpdate,true);
  window.addEventListener('beforeunload',function(event){if(draftAnchor&&!composerSavedRow&&composerBody&&composerBody.value.trim()){event.preventDefault();event.returnValue='You have an unsaved anchored comment.';return event.returnValue;}});
  var outboundPanel=null,outboundHost=null,outboundConfirm=null,outboundUrl=null;
  function parseOutboundHref(href){if(typeof href!=='string')return null;try{var url=new URL(href);return url.protocol==='http:'||url.protocol==='https:'?url:null;}catch(_){return null;}}
  function closeOutbound(){outboundUrl=null;if(!outboundPanel)return;outboundPanel.classList.remove('open');outboundPanel.setAttribute('aria-hidden','true');outboundPanel.setAttribute('inert','');}
  function ensureOutboundPanel(){
    if(outboundPanel)return;
    outboundPanel=document.createElement('aside');outboundPanel.className='vinspector vmodal';outboundPanel.setAttribute('role','dialog');outboundPanel.setAttribute('aria-modal','true');outboundPanel.setAttribute('aria-label','Confirm external link');outboundPanel.setAttribute('aria-hidden','true');outboundPanel.setAttribute('inert','');
    var head=document.createElement('div'),title=document.createElement('h2'),close=document.createElement('button'),content=document.createElement('div'),message=document.createElement('p'),actions=document.createElement('div'),cancel=document.createElement('button');
    head.className='vfb-head';title.textContent='Open external link?';close.type='button';close.className='vfb-close';close.textContent='×';close.setAttribute('aria-label','Close external link confirmation');head.appendChild(title);head.appendChild(close);
    content.className='vfb-list';message.textContent='You are being sent to ';outboundHost=document.createElement('strong');message.appendChild(outboundHost);content.appendChild(message);
    actions.className='vfb-actions';cancel.type='button';cancel.textContent='Cancel';outboundConfirm=document.createElement('button');outboundConfirm.type='button';outboundConfirm.className='vfb-send';outboundConfirm.textContent='Open link';actions.appendChild(cancel);actions.appendChild(outboundConfirm);
    outboundPanel.appendChild(head);outboundPanel.appendChild(content);outboundPanel.appendChild(actions);document.body.appendChild(outboundPanel);
    close.addEventListener('click',closeOutbound);cancel.addEventListener('click',closeOutbound);outboundConfirm.addEventListener('click',function(){var url=outboundUrl;closeOutbound();if(url)window.open(url.href,'_blank','noopener');});
  }
  function showOutbound(url){ensureOutboundPanel();outboundUrl=url;outboundHost.textContent=url.host;outboundPanel.removeAttribute('inert');outboundPanel.classList.add('open');outboundPanel.setAttribute('aria-hidden','false');outboundConfirm.focus();}
  var geoPanel=null,geoResolve=null;
  function settleGeo(answer){var done=geoResolve;geoResolve=null;if(geoPanel){geoPanel.classList.remove('open');geoPanel.setAttribute('aria-hidden','true');geoPanel.setAttribute('inert','');}if(done)done(answer);}
  function askGeoConsent(question,detail){
    if(!geoPanel){
      geoPanel=document.createElement('aside');geoPanel.className='vinspector vmodal';geoPanel.setAttribute('role','dialog');geoPanel.setAttribute('aria-modal','true');geoPanel.setAttribute('aria-label','Share your location');geoPanel.setAttribute('aria-hidden','true');geoPanel.setAttribute('inert','');
      var head=document.createElement('div'),title=document.createElement('h2'),close=document.createElement('button'),content=document.createElement('div'),message=document.createElement('p'),actions=document.createElement('div'),deny=document.createElement('button'),once=document.createElement('button'),always=document.createElement('button');
      head.className='vfb-head';title.className='vgeo-title';close.type='button';close.className='vfb-close';close.textContent='×';close.setAttribute('aria-label','Do not share location');head.appendChild(title);head.appendChild(close);
      content.className='vfb-list';message.className='vgeo-detail';content.appendChild(message);
      actions.className='vfb-actions';deny.type='button';deny.textContent='Don’t allow';once.type='button';once.textContent='Allow once';always.type='button';always.className='vfb-send';always.textContent='Always allow here';
      actions.appendChild(deny);actions.appendChild(once);actions.appendChild(always);geoPanel.appendChild(head);geoPanel.appendChild(content);geoPanel.appendChild(actions);document.body.appendChild(geoPanel);
      close.addEventListener('click',function(){settleGeo('deny');});deny.addEventListener('click',function(){settleGeo('deny');});once.addEventListener('click',function(){settleGeo('once');});always.addEventListener('click',function(){settleGeo('always');});
      geoPanel.addEventListener('keydown',function(event){if(event.key==='Escape')settleGeo('deny');});
    }
    geoPanel.querySelector('.vgeo-title').textContent=question||'Share your location?';geoPanel.querySelector('.vgeo-detail').textContent=detail||'This artifact asks for your current location. Only this artifact receives it.';geoPanel.setAttribute('aria-label',question||'Share your location');
    return new Promise(function(resolve){geoResolve=resolve;geoPanel.removeAttribute('inert');geoPanel.classList.add('open');geoPanel.setAttribute('aria-hidden','false');var primary=geoPanel.querySelector('.vfb-send');if(primary)primary.focus();});
  }
