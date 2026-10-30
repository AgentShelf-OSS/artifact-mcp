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
  var castBridge=null,nativeReader=null;
  var outerRevisionQuery=new URLSearchParams(window.location.search);
  var historicalView=outerRevisionQuery.has('v')||outerRevisionQuery.has('revision');
  var reminderBell=document.getElementById('vpush-toggle'),reminderBanner=document.getElementById('vpush-banner'),reminderReason=document.getElementById('vpush-reason'),reminderEnable=document.getElementById('vpush-enable'),reminderDismiss=document.getElementById('vpush-dismiss'),reminderPromptDismissed=false;
  var reminderPlatform={
    supported:function(){return !!(typeof navigator!=='undefined'&&navigator.serviceWorker&&typeof window.PushManager!=='undefined'&&typeof window.Notification!=='undefined');},
    permission:function(){return typeof window.Notification!=='undefined'?window.Notification.permission:'unsupported';},
    requestPermission:function(){var result=window.Notification.requestPermission();return result&&typeof result.then==='function'?result:Promise.resolve(window.Notification.permission);},
    register:function(url){return navigator.serviceWorker.register(url,{scope:'/'}).then(function(){return navigator.serviceWorker.ready;});},
    needsInstall:function(){if(typeof navigator==='undefined')return false;var ua=String(navigator.userAgent||''),ios=/iPad|iPhone|iPod/.test(ua)||(navigator.platform==='MacIntel'&&navigator.maxTouchPoints>1),standalone=navigator.standalone===true;try{standalone=standalone||!!(window.matchMedia&&window.matchMedia('(display-mode: standalone)').matches);}catch(_){}return ios&&!standalone;},
    label:function(){var ua=typeof navigator!=='undefined'?String(navigator.userAgent||''):'';if(/iPhone|iPod/.test(ua))return 'iPhone';if(/iPad/.test(ua)||(navigator.platform==='MacIntel'&&navigator.maxTouchPoints>1))return 'iPad';if(/Android/.test(ua))return 'Android';return 'Desktop';},
    atob:function(value){return window.atob(value);}
  };
  function reminderBellState(current){if(current.needsInstall&&!current.enabled)return 'install';if(current.permission==='denied')return 'blocked';if(current.optedIn&&current.permission==='granted')return 'on';return 'off';}
  var reminderLabels={off:'Notify me',on:'Notifications on',blocked:'Blocked in browser settings',install:'Add to Home Screen to get notifications'};
  function paintReminderBell(current,busy){
    if(!reminderBell)return;
    var visible=current.enabled||current.needsInstall;reminderBell.hidden=!visible;if(!visible){hideReminderPrompt(false);return;}
    var state=reminderBellState(current),label=reminderLabels[state],text=reminderBell.querySelector('.vpush-label');
    reminderBell.dataset.state=state;reminderBell.setAttribute('aria-label',label);reminderBell.setAttribute('title',label);reminderBell.setAttribute('aria-pressed',state==='on'?'true':'false');reminderBell.setAttribute('aria-busy',busy?'true':'false');if(text)text.textContent=label;
    if(state==='on'||state==='blocked')hideReminderPrompt(false);
  }
  function showReminderPrompt(text,current,force){
    if(!reminderBanner||(reminderPromptDismissed&&!force))return;
    var install=reminderBellState(current)==='install',hint=reminderBanner.querySelector('.vpush-hint');
    if(reminderReason)reminderReason.textContent=text||'This artifact can send you reminders.';
    if(hint){hint.textContent=install?reminderMessages.needs_install:'';hint.hidden=!install;}
    if(reminderEnable)reminderEnable.hidden=install;
    reminderBanner.hidden=false;
  }
  function hideReminderPrompt(restoreFocus){if(!reminderBanner||reminderBanner.hidden)return;var hadFocus=reminderBanner.contains&&reminderBanner.contains(document.activeElement);reminderBanner.hidden=true;if((restoreFocus||hadFocus)&&reminderBell&&!reminderBell.hidden)reminderBell.focus();}
  var reminderMessages={needs_install:'On iPhone or iPad, tap Share, then Add to Home Screen. Open the app from your Home Screen to turn on notifications.',permission_denied:'Notifications are blocked. Allow them in your browser settings.',permission_dismissed:'Notifications were not allowed.',disabled:'Notifications are not available here.',forbidden:'You cannot turn on notifications for this artifact.',rate_limited:'Too many requests. Try again shortly.',network:'Could not reach the server. Try again.'};
  function reminderToggle(){
    if(!reminderBroker)return;
    var state=reminderBellState(reminderBroker.status());
    if(state==='install'){showReminderPrompt('',reminderBroker.status(),true);if(reminderDismiss)reminderDismiss.focus();return;}
    if(state==='blocked'){announce(reminderMessages.permission_denied,true);return;}
    if(state==='on'){reminderBroker.turnOff().then(function(){announce('Notifications off for this artifact');}).catch(function(error){announce(reminderMessages[error&&error.reason]||'Could not turn off notifications',true);});return;}
    reminderBroker.turnOn().then(function(){hideReminderPrompt(true);announce('Notifications on for this artifact');}).catch(function(error){announce(reminderMessages[error&&error.reason]||'Could not turn on notifications',true);});
  }
  // Reminders exist only in the authenticated current-revision viewer shell, never in shares or history.
  var reminderBroker=createViewerReminderBroker({enabled:shellConfig.stateEnabled==='1'&&!historicalView&&!!configLiteral('viewerEmail'),artifactId:artifactId,fetch:function(url,init){return window.fetch(url,init);},post:function(message){postToFrame(message.type,message);},frame:function(){return frame&&frame.contentWindow;},platform:reminderPlatform,onStatus:paintReminderBell,showPrompt:showReminderPrompt});
  if(reminderBell)reminderBell.addEventListener('click',reminderToggle);
  if(reminderEnable)reminderEnable.addEventListener('click',reminderToggle);
  if(reminderDismiss)reminderDismiss.addEventListener('click',function(){reminderPromptDismissed=true;hideReminderPrompt(true);});
  if(reminderBanner)reminderBanner.addEventListener('keydown',function(e){if(e.key==='Escape'){reminderPromptDismissed=true;hideReminderPrompt(true);}});
  if(reminderBell)reminderBroker.load();
  window.addEventListener('focus',function(){if(reminderBroker)reminderBroker.refresh();});
  if(typeof window.createArtifactCastBridge==='function'&&shellConfig.castEnabled==='1'&&!historicalView&&!isBundle&&shellConfig.stateEnabled==='1'&&!!configLiteral('viewerEmail')){
    castBridge=window.createArtifactCastBridge({frame:frame,getCurrentFrame:function(){return frame&&frame.contentWindow;},post:function(message){try{if(frame&&frame.contentWindow)frame.contentWindow.postMessage(message,'*');}catch(_){}},artifactId:artifactId,revision:Number(shellConfig.revision),serverEnabled:shellConfig.castEnabled==='1',authenticated:!!configLiteral('viewerEmail'),stateEnabled:shellConfig.stateEnabled==='1',onCaptureActive:function(active){ttsCapturePaused=active===true;if(ttsCapturePaused){ttsActive.forEach(function(controller){controller.abort();});ttsActive.clear();}if(nativeReader)nativeReader.setCaptureActive(active);}});
    if(frame)frame.addEventListener('load',function(){castBridge.onFrameLoad();});
    window.addEventListener('pagehide',function(){castBridge.destroy();},{once:true});
  }
