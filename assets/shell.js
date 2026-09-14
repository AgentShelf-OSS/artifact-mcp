
(function(){
  // See `assets/portal.js`: portal mutations use a non-simple header as a stateless CSRF signal.
  var nativeFetch=window.fetch;
  window.fetch=function portalFetch(input,init){var options=init||{},method=String(options.method||(input&&input.method)||'GET').toUpperCase();if(method==='POST'||method==='PUT'||method==='PATCH'||method==='DELETE'){var headers=new Headers(options.headers||(input&&input.headers));headers.set('x-artifact-mutation','1');options=Object.assign({},options,{headers:headers});}return nativeFetch.call(window,input,options);};
  var shellConfig=document.getElementById('shell-config').dataset;
  function configLiteral(name){return JSON.parse(shellConfig[name]);}
  var artifactId=configLiteral('artifactId'),prevId=configLiteral('prevId'),nextId=configLiteral('nextId'),bundleRawPrefix=configLiteral('bundleRawPrefix'),versionQuery=configLiteral('versionQuery');
  // The broker owns all state I/O. Its adapters also let tests execute the real queue.
  function createViewerStateBroker(options) {
    var prefix='artifact-state:'+options.artifactId+':',viewerPrefix=prefix+'viewer:'+encodeURIComponent(String(options.viewerId||''))+':',lanes=new Map();
    var keyPattern=/^[A-Za-z0-9._-]{1,64}(?![\s\S])/;
    function send(type,fields){options.post(Object.assign({type:type},fields));}
    function scopeOf(data){return data&&data.scope===undefined?'org':data&&data.scope;}
    function validScope(scope){return scope==='org'||scope==='viewer';}
    function scopeFields(scope){return {scope:typeof scope==='string'?scope:'org'};}
    function error(key,reason,scope){send('state:error',Object.assign({key:typeof key==='string'?key:'',reason:reason},scopeFields(scope)));}
    function read(scope,key){try{var entry=JSON.parse(options.storage.getItem((scope==='viewer'?viewerPrefix:prefix)+key));return entry&&Number.isSafeInteger(entry.revision)&&entry.revision>=0&&Object.prototype.hasOwnProperty.call(entry,'value')?entry:null;}catch(_){return null;}}
    function write(scope,key,entry){var cache=(scope==='viewer'?viewerPrefix:prefix)+key;try{if(entry)options.storage.setItem(cache,JSON.stringify(entry));else options.storage.removeItem(cache);}catch(_){}}
    function lane(scope,key){var id=scope+':'+key;if(!lanes.has(id))lanes.set(id,{epoch:0,timer:null,busy:false,queued:null});return lanes.get(id);}
    function path(scope,key){var value='/'+encodeURIComponent(options.artifactId)+'/state'+(key===undefined?'':'/'+encodeURIComponent(key));return scope==='viewer'?value+'?scope=viewer':value;}
    async function request(scope,key,init){
      var response=await options.fetch(path(scope,key),init);
      var body=response.status===204?{}:await response.json().catch(function(){return {};});
      if(!response.ok){var failure=new Error('state request failed');failure.status=response.status;failure.body=body;throw failure;}
      return body;
    }
    function validValue(body){return body&&Object.prototype.hasOwnProperty.call(body,'value')&&Number.isSafeInteger(body.revision)&&body.revision>=0;}
    function reason(failure){if(failure.status===403||failure.status===401)return 'forbidden';if(failure.status===413||failure.body&&failure.body.error==='too_many_keys')return 'too_large';if(failure.status===400)return failure.body&&failure.body.error==='bad_scope'?'bad_scope':'bad_key';return 'network';}
    function emitValue(scope,key,entry,conflict){var fields={key:key,value:entry.value,revision:entry.revision};Object.assign(fields,scopeFields(scope));if(conflict)fields.conflict=true;send('state:value',fields);}
    function remember(scope,key,value,revision){var entry={value:value,revision:revision},pending=lane(scope,key).queued;if(pending&&!pending.remove)entry.draft={value:pending.value,ifRevision:pending.ifRevision};write(scope,key,entry);}
    function schedule(scope,key,job){
      var current=lane(scope,key);current.epoch++;options.clearTimer(current.timer);current.timer=null;current.queued=job;
      if(job.remove){drain(scope,key);return;}
      var entry=read(scope,key)||{value:null,revision:0};entry.draft={value:job.value,ifRevision:job.ifRevision};write(scope,key,entry);
      current.timer=options.setTimer(function(){current.timer=null;drain(scope,key);},500);
    }
    async function drain(scope,key){
      var current=lane(scope,key);
      if(current.busy||current.timer!==null||!current.queued)return;
      var job=current.queued;current.queued=null;current.busy=true;current.epoch++;
      try {
        var body=await request(scope,key,job.remove?{method:'DELETE'}:{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify({value:job.value,if_revision:job.ifRevision})});
        if(job.remove){
          var pending=current.queued;
          write(scope,key,pending&&!pending.remove?{value:null,revision:0,draft:{value:pending.value,ifRevision:pending.ifRevision}}:null);
          send('state:saved',Object.assign({key:key,revision:0},scopeFields(scope)));
        }else{
          if(!Number.isSafeInteger(body.revision)||body.revision<1)throw new Error('invalid acknowledgement');
          remember(scope,key,job.value,body.revision);send('state:saved',Object.assign({key:key,revision:body.revision},scopeFields(scope)));
        }
      }catch(failure){
        if(failure.status===409&&failure.body&&failure.body.error==='conflict'&&validValue(failure.body)){
          remember(scope,key,failure.body.value,failure.body.revision);emitValue(scope,key,failure.body,true);error(key,'conflict',scope);
        }else{
          // A failed set remains a local draft until a retry or an explicit delete.
          if(!job.remove&&failure.status&&failure.status!==429&&failure.status<500&&!current.queued){var cached=read(scope,key);if(cached){delete cached.draft;write(scope,key,cached);}}
          error(key,reason(failure),scope);
        }
      }finally{current.busy=false;current.epoch++;drain(scope,key);}
    }
    function retryDraft(scope,key,entry){var current=lane(scope,key);if(entry&&entry.draft&&!current.busy&&!current.queued)schedule(scope,key,{value:entry.draft.value,ifRevision:entry.draft.ifRevision});}
    async function get(scope,key){
      var cached=read(scope,key),current=lane(scope,key);
      if(cached){emitValue(scope,key,{value:cached.draft?cached.draft.value:cached.value,revision:cached.revision});retryDraft(scope,key,cached);}
      var epoch=current.epoch;
      try{
        var body;
        try{body=await request(scope,key);}catch(failure){if(failure.status!==404)throw failure;body={value:null,revision:0};}
        if(!validValue(body))throw new Error('invalid state value');
        if(current.epoch!==epoch||current.busy)return;
        var latest=read(scope,key),entry={value:body.value,revision:body.revision};if(latest&&latest.draft)entry.draft=latest.draft;
        write(scope,key,body.revision===0&&!entry.draft?null:entry);
        if(!cached||cached.revision!==body.revision)emitValue(scope,key,body);
      }catch(failure){error(key,reason(failure),scope);}
    }
    async function hello(){
      var scopes=['org','viewer'],results=await Promise.allSettled(scopes.map(function(scope){return request(scope);}));
      function keys(result){var rows=result.status==='fulfilled'&&Array.isArray(result.value.keys)?result.value.keys:[];return rows.filter(function(row){return row&&keyPattern.test(row.key)&&Number.isSafeInteger(row.revision);}).map(function(row){return {key:row.key,revision:row.revision};});}
      send('state:ready',{enabled:true,scope:'org',scopes:scopes,viewer:{id:String(options.viewerId||''),name:String(options.viewerName||'')},keys:keys(results[0]),viewerKeys:keys(results[1])});
      results.forEach(function(result,index){if(result.status==='rejected')error('',reason(result.reason),scopes[index]);});
    }
    return {handle:function(event){
      if(event.source!==options.frame())return false;
      var data=event.data;
      if(!data||typeof data!=='object'||Array.isArray(data)||typeof data.type!=='string'||!data.type.startsWith('state:'))return false;
      var type=data.type,key=data.key,scope=scopeOf(data);
      if(!validScope(scope)){error(key,'bad_scope',scope);return true;}
      if(!options.enabled){if(type==='state:hello')send('state:ready',{enabled:false,scope:'org',keys:[]});else error(key,'disabled',scope);return true;}
      if(type==='state:hello'){hello();return true;}
      if(type!=='state:get'&&type!=='state:set'&&type!=='state:delete')return true;
      if(typeof key!=='string'||!keyPattern.test(key)){error(key,'bad_key',scope);return true;}
      if(type==='state:get'){get(scope,key);return true;}
      if(type==='state:delete'){schedule(scope,key,{remove:true});return true;}
      if(Object.prototype.hasOwnProperty.call(data,'ifRevision')&&(!Number.isSafeInteger(data.ifRevision)||data.ifRevision<0)){error(key,'bad_key',scope);return true;}
      var serialized;
      try{serialized=JSON.stringify(data.value);if(serialized===undefined)throw new Error('missing value');}catch(_){error(key,'bad_key',scope);return true;}
      if(new TextEncoder().encode(serialized).length>256*1024){error(key,'too_large',scope);return true;}
      schedule(scope,key,{value:JSON.parse(serialized),ifRevision:data.ifRevision});return true;
    }};
  }
  // End viewer state broker.
  var stateBroker=createViewerStateBroker({enabled:shellConfig.stateEnabled==='1',artifactId:artifactId,viewerId:configLiteral('viewerId'),viewerName:configLiteral('viewerName'),storage:{getItem:function(key){return localStorage.getItem(key);},setItem:function(key,value){localStorage.setItem(key,value);},removeItem:function(key){localStorage.removeItem(key);}},fetch:function(url,init){return window.fetch(url,init);},post:function(message){postToFrame(message.type,message);},frame:function(){return frame&&frame.contentWindow;},setTimer:function(callback,ms){return window.setTimeout(callback,ms);},clearTimer:function(id){window.clearTimeout(id);}});
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
    if(mode==='share'&&typeof shareLoad==='function'&&!shareLoaded)shareLoad();if(mode==='history'&&typeof histLoad==='function'&&!histLoaded)histLoad();
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
  // Speech is brokered here because the artifact iframe has an opaque origin and no network capability.
  var ttsActive=new Map(),ttsMax=2,ttsVoices=[],ttsRequested=false;
  function ttsError(id,error){postToFrame('tts:error',{requestId:String(id||''),error:error});}
  function ttsHello(){ttsRequested=true;fetch('/'+encodeURIComponent(artifactId)+'/speech/voices').then(function(r){return r.json().then(function(body){return {ok:r.ok,body:body};});}).then(function(result){ttsVoices=result.ok&&result.body&&Array.isArray(result.body.voices)?result.body.voices:[];postToFrame('tts:ready',{enabled:!!(result.ok&&result.body&&result.body.enabled),voices:ttsVoices,maxChars:result.body&&result.body.maxChars||1500});}).catch(function(){postToFrame('tts:ready',{enabled:false,voices:[],maxChars:1500});});}
  function ttsRequest(data){var id=typeof data.requestId==='string'?data.requestId:'',text=typeof data.text==='string'?data.text:'',voice=typeof data.voice==='string'?data.voice:'';if(!id||id.length>80){ttsError(id,'bad_request');return;}if(ttsActive.has(id)){ttsError(id,'duplicate_request');return;}if(!text.trim()||text.length>1500){ttsError(id,'bad_text');return;}if(!ttsVoices.some(function(item){return item&&item.id===voice;})){ttsError(id,'bad_voice');return;}if(ttsActive.size>=ttsMax){ttsError(id,'busy');return;}var controller=new AbortController();ttsActive.set(id,controller);fetch('/'+encodeURIComponent(artifactId)+'/speech',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({text:text,voice:voice}),signal:controller.signal}).then(function(response){if(!response.ok){var error=new Error(response.status===429?'busy':'unavailable');error.code=error.message;throw error;}return response.arrayBuffer();}).then(function(audio){if(ttsActive.get(id)!==controller)return;try{frame.contentWindow.postMessage({type:'tts:audio',requestId:id,audio:audio,mime:'audio/wav'},'*',[audio]);}catch(_){ttsError(id,'unavailable');}}).catch(function(error){if(error&&error.name==='AbortError')return;if(ttsActive.get(id)===controller)ttsError(id,error&&error.code==='busy'?'busy':'unavailable');}).finally(function(){if(ttsActive.get(id)===controller)ttsActive.delete(id);});}
  function ttsCancel(data){var id=typeof data.requestId==='string'?data.requestId:'',controller=id.length<=80?ttsActive.get(id):null;if(controller){ttsActive.delete(id);controller.abort();}}
  function ttsReset(){ttsActive.forEach(function(controller){controller.abort();});ttsActive.clear();ttsVoices=[];if(ttsRequested)ttsHello();}
  if(frame)frame.addEventListener('load',ttsReset);

  // The trusted shell owns audio; the sandbox supplies bounded text snapshots.
  function createNativeReader() {
    if (!frame || !document.getElementById('vshare-toggle')) return;
    const box = document.createElement('section');
    box.className = 'vreader'; box.hidden = true;
    box.setAttribute('aria-label', 'Read aloud');
    box.innerHTML = '<button id="vreader-toggle" type="button" class="vreader-toggle" aria-expanded="false" aria-controls="vreader-mini vreader-panel">Listen</button><div id="vreader-panel" class="vreader-panel" hidden><div class="vreader-head"><div><span class="vreader-kicker">Audio reader</span><strong>Read aloud</strong></div><button id="vreader-minimize" type="button" class="vreader-control vreader-minimize" aria-label="Minimize audio player">Minimize</button><span id="vreader-status" role="status" aria-live="polite">Ready</span></div><div class="vreader-actions" role="group" aria-label="Playback controls"><button id="vreader-play" type="button" class="vreader-control vreader-play">Play</button><button id="vreader-prev" type="button" class="vreader-control vreader-skip" aria-label="Previous block">Previous</button><button id="vreader-next" type="button" class="vreader-control vreader-skip" aria-label="Next block">Next</button><button id="vreader-stop" type="button" class="vreader-control vreader-stop">Stop</button></div><div class="vreader-navigator"><label for="vreader-outline">Jump to</label><div class="vreader-navigator-row"><select id="vreader-outline" disabled><option value="">Loading sections…</option></select><button id="vreader-jump" type="button" class="vreader-control" aria-label="Start reading from selected section or chapter" disabled>Read</button></div><p class="vreader-pick-hint">Click text in the artifact to choose where to read.</p><div id="vreader-target" class="vreader-target" hidden><span id="vreader-target-label"></span><button id="vreader-target-read" type="button" class="vreader-control">Read from here</button><button id="vreader-target-details" type="button" class="vreader-control" hidden>Read details</button><button id="vreader-target-dismiss" type="button" class="vreader-control vreader-dismiss" aria-label="Dismiss reading suggestion">×</button></div></div><div class="vreader-settings"><label>Read <select id="vreader-mode"><option value="page">Document / chapter</option><option value="view">Current view</option><option value="detail" hidden>Selected details</option><option value="selection">Selection</option><option value="here">From here</option><option value="section">This section</option></select></label><label>Voice <select id="vreader-voice"></select></label><label>Speed <select id="vreader-rate"><option value="0.8">0.8×</option><option value="1" selected>1×</option><option value="1.25">1.25×</option><option value="1.5">1.5×</option><option value="2">2×</option></select></label></div><div id="vreader-style-panel" class="vreader-style-panel" hidden><div class="vreader-style-row"><label for="vreader-style">Reading style<select id="vreader-style"><option value="calm">Calm audiobook</option><option value="neutral">Neutral</option><option value="expressive">Expressive</option><option value="default">Model default</option><option value="custom">Custom instructions</option></select></label></div><label id="vreader-custom-label" class="vreader-custom" hidden>Delivery instructions<textarea id="vreader-custom" rows="3" maxlength="500" placeholder="Describe the pacing, tone, and emphasis you want."></textarea></label><p id="vreader-style-hint" class="vreader-style-hint">Steady pacing, restrained emotion, gentle emphasis.</p></div><div class="vreader-utility-row"><button id="vreader-rewind" type="button" class="vreader-control" aria-label="Rewind 15 seconds">↶ 15 seconds</button><button id="vreader-resume" type="button" class="vreader-control" hidden>Resume saved place</button></div><label class="vreader-sleep">Sleep timer<select id="vreader-sleep"><option value="off">Off</option><option value="15">15 minutes</option><option value="30">30 minutes</option><option value="60">60 minutes</option><option value="section">End of section</option><option value="chapter" hidden disabled>End of chapter</option></select><span id="vreader-sleep-hint"></span></label><div class="vreader-preview-row"><button id="vreader-preview" type="button" class="vreader-control">Preview block</button></div><audio id="vreader-audio" preload="auto"></audio></div><div id="vreader-mini" class="vreader-mini" hidden role="region" aria-label="Compact audio player"><div id="vreader-mini-target" class="vreader-target" hidden><span id="vreader-mini-target-label"></span><button id="vreader-mini-target-read" type="button" class="vreader-control">Read from here</button><button id="vreader-mini-target-dismiss" type="button" class="vreader-control vreader-dismiss" aria-label="Dismiss reading suggestion">×</button></div><div class="vreader-mini-row"><div class="vreader-mini-info"><span id="vreader-mini-meta" class="vreader-kicker">Listen</span><span id="vreader-mini-status" role="status" aria-live="polite">Ready</span></div><button id="vreader-mini-rewind" type="button" class="vreader-control" aria-label="Rewind 15 seconds">↶ 15</button><button id="vreader-mini-play" type="button" class="vreader-control vreader-play">Play</button><button id="vreader-mini-next" type="button" class="vreader-control" aria-label="Next block">Next</button><button id="vreader-expand" type="button" class="vreader-control" aria-label="Expand audio player" title="Voice, speed, and reading settings">Expand</button><button id="vreader-mini-close" type="button" class="vreader-control vreader-dismiss" aria-label="Stop and close audio player">×</button></div></div>';
    box.querySelector('#vreader-mini').insertAdjacentHTML('beforeend', '<button id="vreader-mini-resume" type="button" class="vreader-mini-resume" hidden>Resume saved place</button>');
    // Tabler Icons player-play and player-pause, MIT, https://github.com/tabler/tabler-icons.
    box.insertAdjacentHTML('beforeend', '<button id="vreader-inline-read" class="vreader-inline-read" type="button" aria-label="Read from here" title="Read from here" hidden><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M7 4v16l13 -8l-13 -8" /></svg><span>Read from here</span></button>');
    box.querySelector('.vreader-utility-row').insertAdjacentHTML('beforeend', '<button id="vreader-replay" type="button" class="vreader-control" disabled title="Replay the current sentence when word timings are available">Replay sentence</button>');
    // Keep playback together, followed by reading position and voice preferences.
    const readerPanel = box.querySelector('.vreader-panel');
    const playbackGroup = document.createElement('div'); playbackGroup.className = 'vreader-playback';
    readerPanel.prepend(playbackGroup);
    for (const selector of ['.vreader-head','.vreader-actions','.vreader-utility-row']) playbackGroup.appendChild(box.querySelector(selector));
    const readingGroup = box.querySelector('.vreader-navigator');
    const scopeLabel = box.querySelector('#vreader-mode').parentElement;
    scopeLabel.className = 'vreader-scope'; readingGroup.prepend(scopeLabel);
    const listeningSettings = box.querySelector('.vreader-settings');
    listeningSettings.setAttribute('role','group'); listeningSettings.setAttribute('aria-label','Voice and speed');
    const finishRow = document.createElement('div'); finishRow.className = 'vreader-finish-row';
    finishRow.append(box.querySelector('.vreader-sleep'),box.querySelector('.vreader-preview-row'));
    readerPanel.insertBefore(finishRow,box.querySelector('audio'));
    const share = document.getElementById('vshare-toggle'); share.parentNode.insertBefore(box, share);
    const get = name => box.querySelector('#vreader-' + name), audio = get('audio');
    const readerMeasurements = [];
    window.artifactReaderDiagnostics = () => readerMeasurements.map(item => ({...item}));
    let previousAudioEnd = null;
    let ready = false, enabled = false, epoch = 0, serial = 0, extraction = '', extractionTimer;
    let queue = [], position = 0, wants = false, loading = false, currentUrl = null, loaded = -1, chapter = null;
    let controller = new AbortController(), cache = new Map(), voice = '', mode = 'page';
    let audioContext = null, stream = null;
    const streamRequests = new Map();
    let playbackConfig = null;
    let contentFingerprint = '', readerScopeKey = '', restartScopeKey = '', queueKey = '', scopeStart = 0, scopeEnd = 0, restoring = null, pendingSeek = 0;
    const durations = new Map();
    let sleepDeadline = 0, sleepBoundary = null;
    let compact = false, outlineRequest = '', outlineTimer, outlineFingerprint = '', detailTargets = [], clickedTarget = null, targetGeometry = null;
    function readerVisible() { return !get('panel').hidden || compact; }
    function pickMode() { send('reader:pick-mode', {enabled:enabled && ready && readerVisible() && !commentMode}); }
    function refreshOutline() {
      if (!enabled || !ready || !readerVisible()) return;
      clearTimeout(outlineTimer); outlineRequest = 'outline-' + (++serial);
      get('outline').disabled = true; get('jump').disabled = true;
      send('reader:outline', {requestId:outlineRequest});
      outlineTimer = setTimeout(() => {
        if (!outlineRequest) return;
        outlineRequest = ''; get('outline').replaceChildren(new Option('Sections unavailable', ''));
      }, 5000);
    }
    function clearTarget() {
      clickedTarget = null; targetGeometry = null;
      get('target').hidden = get('mini-target').hidden = get('inline-read').hidden = get('target-details').hidden = true;
      send('reader:clear-target');
    }
    function placeTargetAction() {
      const action = get('inline-read'); action.hidden = true;
      if (!clickedTarget || !targetGeometry || !readerVisible() || commentMode) return;
      const bounds = frame.getBoundingClientRect(), g = targetGeometry;
      const sx = bounds.width / g.viewport.width, sy = bounds.height / g.viewport.height;
      const left = bounds.left + g.rect.left * sx, right = bounds.left + g.rect.right * sx;
      const top = bounds.top + g.rect.top * sy, bottom = bounds.top + g.rect.bottom * sy;
      const minY = Math.max(bounds.top, document.querySelector('.vbar').getBoundingClientRect().bottom) + 6;
      const maxY = Math.min(bounds.bottom, innerHeight) - 50;
      if (bottom <= minY || top >= maxY + 44 || right <= bounds.left || left >= bounds.right || maxY < minY) return;
      const y = Math.max(minY, Math.min(top, maxY)), above = Math.max(minY, Math.min(top - 50, maxY));
      const panel = (compact ? get('mini') : get('panel')).getBoundingClientRect();
      const candidates = [[right+4,y],[left>=bounds.left+38 ? Math.max(bounds.left,left-44) : -100,y],[Math.min(right-44,innerWidth-50),above],[Math.max(left,6),above]];
      const candidate = candidates.find(([x,cy]) => x>=Math.max(0,bounds.left) && x+44<=Math.min(innerWidth-6,bounds.right-6) && !(x<panel.right && x+44>panel.left && cy<panel.bottom && cy+44>panel.top));
      if (!candidate) return;
      const [x, cy] = candidate;
      action.querySelector('span').style.left = x < innerWidth / 2 ? '0' : 'auto';
      action.querySelector('span').style.right = x < innerWidth / 2 ? 'auto' : '0';
      action.style.left = x + 'px'; action.style.top = cy + 'px'; action.hidden = false;
    }
    function setPlayerView(view) {
      compact = view === 'compact'; get('panel').hidden = view !== 'full'; get('mini').hidden = !compact;
      get('toggle').setAttribute('aria-expanded', String(view !== 'closed'));
      if (view === 'closed') clearTarget();
      placePanel(); pickMode(); placeTargetAction();
      if (view !== 'closed') refreshOutline();
    }
    function jumpTo(fields) {
      if (!enabled || !ready || (supportsStyle() && get('style').value === 'custom' && !instructions())) return;
      stop('Opening reading position…'); outlineRequest = ''; clearTimeout(outlineTimer); get('outline').disabled = true;
      mode = Number.isInteger(fields.chapterIndex) ? 'page' : 'here'; get('mode').value = mode;
      sleepBoundary = null; clearTarget(); wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null;
      if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial); send('reader:jump', Object.assign({requestId:extraction,mode}, fields));
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not open that position. Choose it again.'); }, 5000); paint();
    }

    function checkpointKey() { if (isBundle && !currentPage) return ''; return 'artifact-reader-place:' + JSON.stringify([artifactId, configLiteral('viewerId'), isBundle ? currentPage : '']); }
    function validCheckpoint(value) {
      return value && (value.version === 1 || value.version === 2) && (value.chapterIndex === undefined || Number.isInteger(value.chapterIndex) && value.chapterIndex >= 0) && typeof value.fingerprint === 'string' && value.fingerprint.length <= 100 &&
        ['page','view','selection','here','section','detail'].includes(value.mode) && Number.isInteger(value.ordinal) && value.ordinal >= 0 &&
        Number.isInteger(value.chunk) && value.chunk >= 0 && Number.isFinite(value.offset) && value.offset >= 0 && value.offset < 3600 &&
        Number.isInteger(value.start) && Number.isInteger(value.end) && value.start <= value.ordinal && value.end >= value.ordinal &&
        typeof value.voice === 'string' && Array.from(get('rate').options).some(o => o.value === value.rate) && (value.version === 1 || typeof value.scopeKey === 'string' && value.scopeKey.length <= 500);
    }
    function savedPlace() {
      try {
        const stored = JSON.parse(localStorage.getItem(checkpointKey()) || 'null');
        let value = stored;
        if (stored && stored.version === 2 && stored.current) {
          value = stored.current;
          if (readerScopeKey && stored.scopes) {
            const scoped = stored.scopes[mode + '\u0000' + readerScopeKey];
            if (scoped) value = scoped;
          }
        }
        if (validCheckpoint(value)) {
          if (!Array.from(get('voice').options).some(o => o.value === value.voice)) {
            const fallback = Array.from(get('voice').options).find(o => o.value === 'pocket_alba');
            if (!fallback) return null;
            // Different voices can use different chunks and timings. Keep the paragraph.
            value.voice = fallback.value; value.chunk = 0; value.offset = 0;
          }
          return value;
        }
      } catch (_) {} return null;
    }
    function refreshSaved() { get('resume').hidden = !savedPlace(); get('resume').disabled = !ready || !enabled || !!extraction; get('mini-resume').hidden = get('resume').hidden || !!queue.length || !!extraction; get('mini-resume').disabled = get('resume').disabled; }
    function mediaOffset() {
      if (!stream) return loaded === position ? audio.currentTime || 0 : pendingSeek;
      let offset = stream.baseOffset;
      for (const f of stream.frames) offset += f.played ? f.buffer.duration : Math.min(f.buffer.duration, f.offset + (f.source ? Math.max(0, audioContext.currentTime - f.start - (stream.latency || 0)) * f.rate : 0));
      return offset;
    }
    function savePlace() {
      const entry = queue[position];
      if (!entry || !queueKey || !contentFingerprint || (playbackConfig?.mode || mode) === 'selection' || previewEnd !== null || previewRequested || restoring || loaded !== position) return;
      try {
        const checkpoint = {version:2, fingerprint:contentFingerprint, ordinal:entry.ordinal, chunk:entry.chunk, offset:mediaOffset(), start:scopeStart, end:scopeEnd, chapterIndex:chapter?.index, voice:playbackConfig?.voice || voice, rate:get('rate').value, mode:playbackConfig?.mode || mode, scopeKey:readerScopeKey, style:playbackConfig?.style || get('style').value, custom:playbackConfig?.custom ?? get('custom').value};
        let stored = null; try { stored = JSON.parse(localStorage.getItem(queueKey) || 'null'); } catch (_) {}
        if (!stored || stored.version !== 2 || !stored.scopes || typeof stored.scopes !== 'object') stored = {version:2, current:null, scopes:{}};
        const scopeId = checkpoint.mode + '\u0000' + checkpoint.scopeKey;
        stored.current = checkpoint; stored.scopes[scopeId] = checkpoint;
        const keys = Object.keys(stored.scopes); while (keys.length > 12) delete stored.scopes[keys.shift()];
        localStorage.setItem(queueKey, JSON.stringify(stored));
      } catch (_) {}
      refreshSaved();
    }
    function cancelSleep() { sleepDeadline = 0; sleepBoundary = null; get('sleep').value = 'off'; get('sleep-hint').textContent = ''; }
    function armSleep() {
      const choice = get('sleep').value;
      if (/^\d+$/.test(choice) && !sleepDeadline) sleepDeadline = Date.now() + Number(choice) * 60000;
      if (choice === 'section' && sleepBoundary === null && queue[position]) sleepBoundary = queue[position].sectionEndOrdinal;
    }
    function mayContinue(index) { return !(get('sleep').value === 'section' && sleepBoundary !== null && queue[index]?.ordinal > sleepBoundary); }
    function checkSleep() {
      if (sleepDeadline && Date.now() >= sleepDeadline) { stop('Sleep timer ended. Your place is saved.'); cancelSleep(); return true; }
      get('sleep-hint').textContent = sleepDeadline ? Math.ceil((sleepDeadline - Date.now()) / 60000) + ' min remaining' : '';
      return false;
    }
    setInterval(() => { checkSleep(); savePlace(); }, 2000);
    window.addEventListener('pagehide', () => stop('Paused', true));
    document.addEventListener('visibilitychange', () => { checkSleep(); savePlace(); });

    const readingStyles = {
      calm: 'Read as a calm audiobook narrator. Use consistent pacing, restrained emotion, and natural sentence endings. Avoid exaggerated emphasis and dramatic pitch changes.',
      neutral: 'Read clearly with an even, neutral delivery, consistent pacing, and natural pauses. Keep emotional expression minimal.',
      expressive: 'Read expressively with natural emotional variation appropriate to the text, clear phrasing, and varied emphasis.'
    };
    let previewEnd = null, previewStart = 0, previewRequested = false;
    function supportsStyle() { return voice === 'qwen_ryan' || voice === 'qwen_aiden'; }
    function instructions() { return supportsStyle() ? (get('style').value === 'custom' ? get('custom').value.trim() : readingStyles[get('style').value] || '') : ''; }
    function speechBody(text, chosenVoice) {
      const body = { text, voice: chosenVoice };
      if (supportsStyle() && instructions()) body.instructions = instructions();
      return JSON.stringify(body);
    }
    function updateStyle() {
      get('style-panel').hidden = !supportsStyle();
      get('custom-label').hidden = get('style').value !== 'custom';
      get('style-hint').textContent = ({ calm: 'Steady pacing, restrained emotion, gentle emphasis.', neutral: 'Clear, even delivery with minimal emotion.', expressive: 'More variation in tone and emphasis.', default: 'Uses the model’s own delivery, without style instructions.', custom: 'Up to 500 characters. Style guides delivery; results can vary.' })[get('style').value];
      placePanel(); paint();
    }
    try {
      const saved = JSON.parse(localStorage.getItem('artifact-reader-style') || 'null');
      if (saved && ['calm','neutral','expressive','default','custom'].includes(saved.style)) get('style').value = saved.style;
      if (saved && typeof saved.custom === 'string') get('custom').value = saved.custom.slice(0,500);
    } catch (_) {}
    let savedVoice = '';
    try {
      const saved = JSON.parse(localStorage.getItem('artifact-reader-preferences') || 'null');
      if (saved && typeof saved.voice === 'string') savedVoice = saved.voice;
      if (saved && Array.from(get('rate').options).some(option => option.value === saved.rate)) get('rate').value = saved.rate;
    } catch (_) {}
    function savePreferences() {
      try { localStorage.setItem('artifact-reader-preferences', JSON.stringify({ voice, rate: get('rate').value })); } catch (_) {}
    }
    function styleChanged() {
      stop('Style changed. Play or preview this paragraph.', true);
      try { localStorage.setItem('artifact-reader-style', JSON.stringify({ style: get('style').value, custom: get('custom').value })); } catch (_) {}
      updateStyle();
    }
    function preparePreview() {
      previewStart = position;
      const block = queue[position]?.block;
      while (position > 0 && queue[position - 1].block === block) position--;
      previewEnd = position;
      while (previewEnd < queue.length && queue[previewEnd].block === block) previewEnd++;
    }
    const send = (type, fields) => postToFrame(type, fields);
    function status(text) { get('status').textContent = text; get('mini-status').textContent = text; get('mini-status').title = text; }
    function readingMessage() {
      const entry = queue[position];
      const label = chapter?.label || entry?.sectionLabel || (entry ? 'block ' + (entry.ordinal + 1) : '');
      return (wants ? 'Reading' : 'Paused') + (label ? ' · ' + label : '');
    }
    function paint() { get('replay').disabled = replaySentenceStart() === null; get('rewind').disabled = loaded < 0; refreshSaved(); box.classList.toggle('is-playing', wants); box.classList.toggle('is-loading', loading); get('play').textContent = wants ? 'Pause' : loaded >= 0 || loading ? 'Resume' : 'Play'; get('play').setAttribute('aria-label', wants ? 'Pause reading' : loaded >= 0 || loading ? 'Resume reading' : 'Start reading'); get('play').disabled = !enabled || !ready || (!wants && supportsStyle() && get('style').value === 'custom' && !instructions()); get('preview').disabled = !enabled || !ready || (supportsStyle() && get('style').value === 'custom' && !instructions()); get('stop').disabled = !queue.length && !extraction;
      const icon = wants
        ? '<path d="M6 6a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1l0 -12" /><path d="M14 6a1 1 0 0 1 1 -1h2a1 1 0 0 1 1 1v12a1 1 0 0 1 -1 1h-2a1 1 0 0 1 -1 -1l0 -12" />'
        : '<path d="M7 4v16l13 -8l-13 -8" />';
      get('mini-play').innerHTML = '<svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' + icon + '</svg>';
      const voiceName = get('voice').selectedOptions[0]?.textContent.split(' · ')[0] || 'Listen';
      const scopeName = {page:'Document',view:'Current view',detail:'Details',section:'Section',selection:'Selection',here:'From here'}[mode] || 'Listen';
      get('mini-meta').textContent = scopeName + ' · ' + voiceName + ' · ' + get('rate').value + '×';
      get('mini-play').title = get('play').getAttribute('aria-label');
      get('mini-play').disabled = get('play').disabled;
      get('mini-play').setAttribute('aria-label', get('play').getAttribute('aria-label'));
      get('mini-rewind').disabled = get('rewind').disabled;
      get('mini-next').disabled = !queue.length || !!extraction;
      const canJump = enabled && ready && !commentMode && !extraction && !(supportsStyle() && get('style').value === 'custom' && !instructions());
      get('jump').disabled = !canJump || !get('outline').value || get('outline').disabled;
      get('target-read').disabled = get('mini-target-read').disabled = get('inline-read').disabled = !canJump;
      placeTargetAction();
    }
    function replaySentenceStart() {
      // Reuse only the current stream's decoded audio. No new audio cache,
      // storage writes, network request, or synthesis is needed for replay.
      if (!stream?.wordMapping || !stream.words.length || typeof Intl.Segmenter !== 'function') return null;
      const entry = queue[position];
      if (!entry || loaded !== position) return null;
      const time = mediaOffset();
      const word = stream.words.findLast(w => w.start <= time);
      if (!word) return null;
      const sentence = Array.from(new Intl.Segmenter('en', {granularity:'sentence'}).segment(entry.text))
        .find(s => word.textStart >= s.index && word.textStart < s.index + s.segment.length);
      if (!sentence) return null;
      const first = stream.words.find(w => w.textStart >= sentence.index);
      // A stream resumed partway through a sentence may not retain its start.
      return first && first.start >= stream.baseOffset ? first.start : null;
    }
    function clearStream() {
      if (!stream) return;
      cancelAnimationFrame(stream.highlightFrame);
      if (stream.reader) stream.reader.cancel().catch(() => {});
      stream.sources.forEach(source => { try { source.onended = null; source.stop(); } catch (_) {} });
      stream.pitch?.disconnect(); if (stream.silence) { stream.silence.stop(); stream.silence.disconnect(); }
      stream.sources.clear(); if (stream.finish) stream.finish.resolve(); stream = null; loaded = -1;
      box.removeAttribute('data-stream-state'); box.removeAttribute('data-played-samples'); box.removeAttribute('data-buffered-samples');
    }
    let pitchModule = null, pitchReady = false;
    function ensurePitchModule(context) {
      if (!context.audioWorklet || !window.AudioWorkletNode) return Promise.resolve(false);
      if (!pitchModule) pitchModule = context.audioWorklet.addModule('/reader-audio/pitch-v1.js')
        .then(() => pitchReady = true).catch(() => false);
      return pitchModule;
    }
    function ensureAudioContext() {
      if (!window.AudioContext && !window.webkitAudioContext) return null;
      if (!audioContext) { try { audioContext = new (window.AudioContext || window.webkitAudioContext)(); } catch (_) { return null; } }
      return audioContext;
    }
    function stop(message, preserve) {
      savePlace(); pendingSeek = 0; restoring = null;
      epoch++; wants = false; loading = false; extraction = ''; previewEnd = null; previewRequested = false; clearTimeout(extractionTimer);
      controller.abort(); controller = new AbortController(); cache.clear();
      streamRequests.forEach(work => { work.then(response => response.body?.cancel()).catch(() => {}); });
      streamRequests.clear();
      clearStream();
      audio.onended = null; audio.pause(); audio.removeAttribute('src'); audio.load(); loaded = -1;
      if (currentUrl) URL.revokeObjectURL(currentUrl); currentUrl = null;
      if (!preserve) { previousAudioEnd = null; queue = []; position = 0; chapter = null; durations.clear(); contentFingerprint = ''; readerScopeKey = ''; queueKey = ''; const option = get('sleep').querySelector('[value=chapter]'); option.hidden = option.disabled = true; }
      send('reader:highlight', { id: null }); status(message || 'Ready'); paint();
    }
    function pronunciationPlan(text, hints) {
      const segments = []; let spoken = '', cursor = 0;
      if (!Array.isArray(hints) || hints.length > 128) hints = [];
      const valid = hints.every(h => h && Number.isInteger(h.start) && Number.isInteger(h.end) && h.start >= cursor && h.end > h.start && h.end <= text.length && h.end-h.start <= 200 && typeof h.text === 'string' && h.text.trim().length > 0 && h.text.length <= 200 && !/[\u0000-\u001f]/.test(h.text) && (cursor = h.end));
      if (!valid) hints = [];
      cursor = 0;
      function add(value, start, end, replacement) {
        if (!value) return;
        segments.push({start:spoken.length,end:spoken.length+value.length,sourceStart:start,sourceEnd:end,replacement}); spoken += value;
      }
      for (const hint of hints) {
        add(text.slice(cursor,hint.start),cursor,hint.start,false);
        add(hint.text.replace(/\s+/g,' ').trim(),hint.start,hint.end,true); cursor = hint.end;
      }
      add(text.slice(cursor),cursor,text.length,false);
      return {text:spoken,segments};
    }
    function pronunciationRange(plan, start, end) {
      const first = plan.segments.find(s=>s.start<=start && start<s.end);
      const last = plan.segments.find(s=>s.start<end && end<=s.end);
      if (!first || !last) return null;
      return {start:first.replacement ? first.sourceStart : first.sourceStart+start-first.start,
        end:last.replacement ? last.sourceEnd : last.sourceStart+end-last.start};
    }
    function splitLongText(text, limit) {
      const result = [], chars = Array.from(text); let start = 0;
      while (start < chars.length) {
        let end = Math.min(start + limit, chars.length);
        if (end < chars.length) {
          for (let i = end; i > start + Math.floor(limit / 2); i--) {
            if (/\s/.test(chars[i - 1])) { end = i; break; }
          }
        }
        const value = chars.slice(start, end).join('').trim(); if (value) result.push(value); start = end;
      }
      return result;
    }
    function chunks(text) {
      const limit = /^moss_/.test(voice) ? 300 : 600;
      if (!/^(pocket_|raven_)/.test(voice) || typeof Intl.Segmenter !== 'function') return splitLongText(text, limit);
      // Keep complete sentences together. Only split within a sentence when it
      // exceeds the worker chunk limit, including text without punctuation.
      const result = []; let pending = '';
      for (const { segment } of new Intl.Segmenter('en', { granularity: 'sentence' }).segment(text)) {
        const sentence = segment.trim();
        if (!sentence) continue;
        if (pending && Array.from(pending + ' ' + sentence).length > limit) { result.push(pending); pending = ''; }
        if (Array.from(sentence).length > limit) {
          const parts = splitLongText(sentence, limit); result.push(...parts.slice(0, -1)); pending = parts[parts.length - 1] || '';
        } else pending = pending ? pending + ' ' + sentence : sentence;
      }
      if (pending) result.push(pending);
      return result;
    }
    function delay(ms, signal) {
      return new Promise((resolve, reject) => {
        if (signal.aborted) return reject(new DOMException('Cancelled', 'AbortError'));
        const cancel = () => { clearTimeout(timer); reject(new DOMException('Cancelled', 'AbortError')); };
        const timer = setTimeout(() => { signal.removeEventListener('abort', cancel); resolve(); }, ms);
        signal.addEventListener('abort', cancel, { once: true });
      });
    }
    function synth(index) {
      if (cache.has(index)) return cache.get(index);
      const signal = controller.signal, entry = queue[index], chosenVoice = voice;
      const work = (async () => {
        for (let attempt = 0; attempt < 5; attempt++) {
          const response = await fetch('/' + encodeURIComponent(artifactId) + '/speech', { method: 'POST', headers: { 'content-type': 'application/json' }, body: speechBody(entry.text, chosenVoice), signal });
          if (response.status === 429 && attempt < 4) { await delay(1500, signal); continue; }
          if (!response.ok) throw new Error(response.status === 429 ? 'Speech is busy. Press Play to retry.' : 'Speech is unavailable. Press Play to retry.');
          if (!(response.headers.get('content-type') || '').startsWith('audio/wav')) throw new Error('Invalid audio response.');
          return response.arrayBuffer();
        }
      })();
      cache.set(index, work); work.catch(() => { if (cache.get(index) === work) cache.delete(index); });
      return work;
    }
    // RAVEN trial uses complete paragraph WAVs while its live-stream buzzing is investigated.
    function streamingVoice() { return /^(qwen_|pocket_)/.test(voice); }
    function fetchStream(index) {
      if (streamRequests.has(index)) return streamRequests.get(index);
      const signal = controller.signal, entry = queue[index], chosenVoice = voice;
      const work = (async () => {
        for (let attempt = 0; attempt < 10; attempt++) {
          const timed = /^pocket_/.test(chosenVoice);
          const options = { method: 'POST', headers: { 'content-type': 'application/json' }, body: speechBody(entry.text, chosenVoice), signal };
          let response = await fetch('/' + encodeURIComponent(artifactId) + (timed ? '/speech/stream-timed' : '/speech/stream'), options);
          if (timed && [404, 415].includes(response.status)) {
            await response.body?.cancel();
            response = await fetch('/' + encodeURIComponent(artifactId) + '/speech/stream', options);
          }
          if (response.status !== 429 || attempt === 9) return response;
          await response.body?.cancel(); await delay(500, signal);
        }
      })();
      // Keep at most the current response and one upcoming response. Leaving the
      // upcoming body unread preserves browser/network backpressure until playback.
      streamRequests.set(index, work);
      work.catch(() => {}); // A prefetch failure is reported when this paragraph plays.
      return work;
    }
    async function streamCurrent(index) {
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
      const state = stream = { words:[], wordCursor:0, wordMapping:true, highlightedWord:-1, highlightFrame:0, baseOffset:seek, received:0, token, index, reader, sources: new Set(), frames: [], pending: new Uint8Array(0), done: false, started: false, bytes: 0, total: 0, played: 0, buffered: 0, cursor: context.currentTime + startup, rate: Number(get('rate').value), finished: null, finish: null };
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
        state.pitch = null; state.silence = null; state.latency = rate === 1 ? 0 : 0.2;
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
        node.connect(context.destination);
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
        source.connect(state.pitch || context.destination); frame.source = source; frame.rate = rate; frame.start = start; state.sources.add(source);
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
        if (token === epoch && mayContinue(index + 1) && index + 1 < queue.length && (previewEnd === null || index + 1 < previewEnd)) fetchStream(index + 1);
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
      try { await audio.play(); }
      catch (_) { wants = false; status('Press Play to start audio.'); paint(); }
    }
    async function loadCurrent(retry = 0) {
      if (loading) return;
      if (checkSleep()) return; armSleep();
      const token = epoch, index = position;
      if (previewEnd !== null && index >= previewEnd) {
        const restart = previewStart; stop('Preview finished. Play to continue reading.', true); position = restart; return;
      }
      if (!mayContinue(index) || (index >= queue.length && ['section','chapter'].includes(get('sleep').value))) { stop('Sleep timer ended. Your place is saved.'); cancelSleep(); return; }
      if (index >= queue.length) {
        if (chapter && chapter.hasNext && ['page','here'].includes(mode)) { requestContent(true); return; }
        const finishedKey = queueKey; stop('Finished'); try { if (finishedKey) localStorage.removeItem(finishedKey); } catch (_) {} refreshSaved(); return;
      }
      playbackConfig = {voice, mode, style:get('style').value, custom:get('custom').value};
      loading = true; send('reader:highlight', {id:queue[index].id}); status('Preparing audio…'); paint();
      try {
        if (streamingVoice() && ensureAudioContext()) {
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
        // Only begin prefetch after the current synthesis completes.
        if (token === epoch && mayContinue(index + 1) && index + 1 < queue.length && (previewEnd === null || index + 1 < previewEnd)) synth(index + 1).catch(() => {});
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
        loading = false; wants = false; status(error.message || 'Speech is unavailable. Press Play to resume.'); paint();
      }
    }
    function requestContent(nextChapter, preview = false) {
      const scopeKey = !nextChapter && ['view','detail','section'].includes(mode) ? restartScopeKey : '';
      const intended = wants; stop(nextChapter ? 'Opening next chapter…' : 'Reading page…'); wants = intended; previewRequested = preview;
      extraction = 'read-' + (++serial);
      send(nextChapter ? 'reader:next' : 'reader:extract', { requestId: extraction, mode: nextChapter ? 'page' : mode, scopeKey });
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not read this page. Press Play to retry.'); }, 5000); paint();
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
      if (!target || typeof target.scopeKey !== 'string' || !target.scopeKey || target.scopeKey.length > 500) return;
      stop('Opening details…'); mode = 'detail'; get('mode').value = mode; wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null; if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial);
      send('reader:extract', {requestId:extraction, mode:'detail', scopeKey:target.scopeKey});
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not read these details.'); }, 5000); paint();
    }
    get('target-details').onclick = () => { if (clickedTarget?.detailAvailable) readDetails(clickedTarget); };
    get('target-dismiss').onclick = get('mini-target-dismiss').onclick = clearTarget;
    if (commentToggle) new MutationObserver(() => { pickMode(); if (commentMode) clearTarget(); paint(); }).observe(commentToggle, {attributes:true,attributeFilter:['aria-pressed']});
    new ResizeObserver(() => { placePanel(); placeTargetAction(); }).observe(document.querySelector('.vbar'));
    window.addEventListener('resize', placeTargetAction);
    document.addEventListener('keydown', event => { if (event.key === 'Escape' && !get('panel').hidden) { setPlayerView(queue.length || extraction ? 'compact' : 'closed'); get('toggle').focus(); } });
    get('play').onclick = () => {
      if (checkSleep()) return; armSleep();
      if (wants) { savePlace(); wants = false; audio.pause(); if (audioContext && audioContext.state === 'running' && streamingVoice()) audioContext.suspend().catch(() => {}); if (stream) { stream.boxState = 'paused'; box.dataset.streamState = 'paused'; } status(readingMessage()); paint(); return; }
      wants = true; if (loaded >= 0 || stream) status(readingMessage()); paint();
      if (streamingVoice()) { const context = ensureAudioContext(); if (context && context.state === 'suspended') context.resume().catch(() => {}); }
      if (stream && audioContext && audioContext.state === 'suspended') audioContext.resume().catch(() => {});
      if (stream) { stream.boxState = 'playing'; box.dataset.streamState = 'playing'; return; }
      if (extraction || loading) return;
      if (!queue.length) requestContent(false); else if (loaded === position) playLoaded(); else loadCurrent();
    };
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
    get('resume').onclick = () => {
      const saved = savedPlace(); if (!saved) return;
      stop('Restoring saved place…'); restoring = saved;
      voice = saved.voice; get('voice').value = voice; get('rate').value = saved.rate; mode = saved.mode; get('mode').value = mode;
      if (['calm','neutral','expressive','default','custom'].includes(saved.style)) get('style').value = saved.style;
      if (typeof saved.custom === 'string') get('custom').value = saved.custom.slice(0,500);
      updateStyle(); wants = true;
      const context = streamingVoice() ? ensureAudioContext() : null; if (context?.state === 'suspended') context.resume().catch(() => {});
      extraction = 'read-' + (++serial); const resume = {requestId:extraction, mode, fingerprint:saved.fingerprint, chapterIndex:saved.chapterIndex};
      if (['view','detail','section'].includes(mode) && saved.scopeKey) resume.scopeKey = saved.scopeKey;
      send('reader:resume', resume);
      extractionTimer = setTimeout(() => { if (extraction) stop('Could not restore this place. Start reading again.'); }, 5000); paint();
    };
    function skip(direction) {
      if (streamingVoice()) { const context = ensureAudioContext(); if (context && context.state === 'suspended') context.resume().catch(() => {}); }
      if (!queue.length) { wants = true; requestContent(false); return; }
      const block = queue[position] && queue[position].block;
      let target = position;
      if (direction > 0) { while (target < queue.length && queue[target].block === block) target++; }
      else { target = Math.max(0, position - 1); while (target > 0 && queue[target - 1].block === queue[target].block) target--; }
      stop('Preparing audio…', true); position = target; wants = true; loadCurrent();
    }
    get('prev').onclick = () => skip(-1); get('next').onclick = () => skip(1);
    get('mode').onchange = () => { stop(); restartScopeKey = ''; mode = get('mode').value; };
    get('voice').onchange = () => { voice = get('voice').value; savePreferences(); stop('Voice changed'); updateStyle(); };
    get('style').onchange = styleChanged;
    get('custom').oninput = styleChanged;
    get('preview').onclick = () => {
      const context = streamingVoice() ? ensureAudioContext() : null; if (context && context.state === 'suspended') context.resume().catch(() => {});
      if (queue.length && position < queue.length) {
        stop('Preparing preview…', true); preparePreview(); wants = true; loadCurrent();
      } else { wants = true; requestContent(false, true); }
    };
    get('rate').onchange = () => { savePreferences(); const value = Number(get('rate').value); audio.playbackRate = value; if (stream && !pitchReady && value !== 1) {
        const offset = mediaOffset(), playing = wants; stop('Preparing audio…', true); pendingSeek = offset; wants = playing; loadCurrent();
      } else if (stream && stream.changeRate) stream.changeRate(value); paint(); };
    audio.onerror = () => { if (audio.getAttribute('src')) stop('Could not play audio. Press Play to retry.'); };
    frame.addEventListener('load', () => { stop(); restartScopeKey = ''; clearTarget(); outlineRequest = ''; clearTimeout(outlineTimer); get('outline').replaceChildren(new Option('Loading sections…','')); get('outline').disabled = true; ready = false; send('reader:hello'); paint(); });
    window.addEventListener('message', event => {
      if (event.source !== frame.contentWindow || !event.data || typeof event.data !== 'object') return;
      const data = event.data;
      if (data.type === 'anchor:ready') setTimeout(() => { if (queue.length && !queueKey) queueKey = checkpointKey(); refreshSaved(); }, 0);
      if (data.type === 'reader:ready') { ready = true; pickMode(); refreshOutline(); paint(); return; }
      if (data.type === 'reader:changed') {
        const changedScope = typeof data.scopeKey === 'string' ? data.scopeKey : '';
        if ((!changedScope && !readerScopeKey) || (changedScope && changedScope === readerScopeKey)) { stop('Page changed. Press Play to read it.'); clearTarget(); refreshOutline(); }
        return;
      }
      if (data.type === 'reader:error' && outlineRequest && data.requestId === outlineRequest) { clearTimeout(outlineTimer); outlineRequest = ''; get('outline').replaceChildren(new Option('Sections unavailable','')); get('outline').disabled = true; paint(); return; }
      if (data.type === 'reader:target-position') {
        const rect = data.rect, viewport = data.viewport;
        if (!rect) { targetGeometry = null; get('inline-read').hidden = true; return; }
        if (!clickedTarget || !viewport || ![rect.left,rect.right,rect.top,rect.bottom,viewport.width,viewport.height].every(n=>Number.isFinite(n) && Math.abs(n)<10000000) || viewport.width<=0 || viewport.height<=0 || rect.right<rect.left || rect.bottom<rect.top) return;
        targetGeometry = {rect,viewport}; placeTargetAction(); return;
      }
      if (data.type === 'reader:target') {
        if (!readerVisible() || commentMode || !Number.isInteger(data.ordinal) || data.ordinal < 0 || data.ordinal >= 5000 || typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || typeof data.label !== 'string' || typeof data.scopeKey !== 'string' || data.scopeKey.length > 500) return;
        targetGeometry = null; clickedTarget = {ordinal:data.ordinal,fingerprint:data.fingerprint,scopeKey:data.scopeKey,detailAvailable:data.detailAvailable === true};
        get('target-label').textContent = get('mini-target-label').textContent = data.label.slice(0,160);
        get('target').hidden = get('mini-target').hidden = false; get('target-details').hidden = !clickedTarget.detailAvailable; paint(); return;
      }
      if (data.type === 'reader:outline' && outlineRequest && data.requestId === outlineRequest) {
        clearTimeout(outlineTimer); outlineRequest = '';
        const valid = (items, key) => Array.isArray(items) && items.length <= 500 && items.every(item => item && Number.isInteger(item[key]) && item[key] >= 0 && typeof item.label === 'string' && item.label.length <= 120);
        const validDetails = items => Array.isArray(items) && items.length <= 100 && items.every(item => item && typeof item.label === 'string' && item.label.length <= 120 && typeof item.scopeKey === 'string' && item.scopeKey.length <= 500);
        if (typeof data.fingerprint !== 'string' || data.fingerprint.length > 100 || !valid(data.sections,'ordinal') || !valid(data.chapters,'index') || !validDetails(data.details || [])) return;
        outlineFingerprint = data.fingerprint;
        detailTargets = data.details || [];
        const select = get('outline'); select.replaceChildren(new Option('Choose a section…',''));
        for (const [items, prefix, label, key] of [[data.chapters,'chapter','Chapters','index'],[data.sections,'section',data.chapters.length ? 'On this page' : 'Sections','ordinal']]) {
          if (!items.length) continue;
          const group = document.createElement('optgroup'); group.label = label;
          for (const item of items) group.appendChild(new Option(item.label, prefix + ':' + item[key]));
          select.appendChild(group);
        }
        if (detailTargets.length) {
          const group = document.createElement('optgroup'); group.label = 'Details';
          detailTargets.forEach((item, index) => group.appendChild(new Option(item.label, 'detail:' + index)));
          select.appendChild(group);
        }
        select.disabled = select.options.length < 2;
        if (select.disabled) select.options[0].textContent = 'No readable sections';
        paint(); return;
      }
      if ((data.type !== 'reader:content' && data.type !== 'reader:error') || !extraction || data.requestId !== extraction) return;
      extraction = ''; clearTimeout(extractionTimer);
      if (data.type === 'reader:error') { stop(typeof data.message === 'string' ? data.message.slice(0,200) : 'Could not read this page.'); clearTarget(); refreshOutline(); return; }
      let total = 0;
      if (!Array.isArray(data.blocks) || data.blocks.length > 5000 || data.blocks.some(b => !b || typeof b.id !== 'string' || b.id.length > 80 || typeof b.text !== 'string' || (total += b.text.length) > 500000)) { stop('This page returned too much text.'); return; }
      const saved = restoring; restoring = null;
      queue = data.blocks.flatMap((b, index) => {
        let cursor = 0;
        const pronunciation = pronunciationPlan(b.text,b.pronunciations);
        return chunks(pronunciation.text).map((text, chunk) => {
          const textOffset = pronunciation.text.indexOf(text, cursor);
          cursor = textOffset < 0 ? pronunciation.text.length : textOffset + text.length;
          const source = pronunciationRange(pronunciation,textOffset,textOffset+text.length);
          const sourceOffset = source?.start || 0, sourceText = source ? b.text.slice(source.start,source.end) : '';
          return {id:b.id, text, textOffset, pronunciation, sourceOffset, sourceText, block:index, ordinal:Number.isInteger(b.ordinal) ? b.ordinal : index, chunk, sectionLabel:typeof b.sectionLabel === 'string' ? b.sectionLabel.slice(0,120) : '', sectionEndOrdinal:Number.isInteger(b.sectionEndOrdinal) ? b.sectionEndOrdinal : data.blocks.length - 1};
        });
      });
      if (queue.reduce((size,entry)=>size+entry.text.length,0) > 500000) { stop('This page returned too much spoken text.'); return; }
      if (typeof data.scopeKey !== 'string' || data.scopeKey.length > 500) { stop('This reading scope is invalid.'); return; }
      readerScopeKey = restartScopeKey = data.scopeKey; contentFingerprint = typeof data.fingerprint === 'string' && data.fingerprint.length <= 100 ? data.fingerprint : ''; queueKey = checkpointKey();
      if (saved) {
        if (contentFingerprint !== saved.fingerprint || saved.version === 2 && saved.scopeKey !== readerScopeKey) { stop('Content changed or this place belongs to a different view. Start reading again.'); return; }
        queue = queue.filter(entry => entry.ordinal >= saved.start && entry.ordinal <= saved.end);
        position = queue.findIndex(entry => entry.ordinal === saved.ordinal && entry.chunk === saved.chunk);
        if (position < 0) { stop('Saved place is no longer available. Start reading again.'); return; }
        pendingSeek = saved.offset;
      } else position = 0;
      scopeStart = queue[0]?.ordinal || 0; scopeEnd = queue[queue.length - 1]?.ordinal || 0;
      chapter = data.chapter && typeof data.chapter.label === 'string' ? { index: Number.isInteger(data.chapter.index) ? data.chapter.index : undefined, label: data.chapter.label.slice(0,120), hasNext: data.chapter.hasNext === true } : null;
      const chapterOption = get('sleep').querySelector('[value=chapter]'); chapterOption.hidden = chapterOption.disabled = !chapter;
      if (!chapter && get('sleep').value === 'chapter') cancelSleep();
      if (!queue.length) { stop(mode === 'selection' ? 'Select text in the artifact first.' : 'No readable text found.'); return; }
      if (previewRequested) { previewRequested = false; preparePreview(); }
      status(data.truncated ? 'Long page shortened to the reading limit.' : chapter ? chapter.label + (['page','here'].includes(mode) ? ' · continues across chapters' : ' · this section') : 'Ready'); paint();
      clearTarget(); refreshOutline(); if (wants) loadCurrent();
    });
    fetch('/' + encodeURIComponent(artifactId) + '/speech/voices').then(r => r.ok ? r.json() : null).then(data => {
      if (!data || !data.enabled || !Array.isArray(data.voices) || !data.voices.length) return;
      const groups = new Map();
      for (const item of data.voices) { const label = item.provider || (/pocket/i.test(item.id + ' ' + item.name) ? 'Pocket TTS' : /^raven_/.test(item.id) ? 'RAVEN trial' : /qwen/i.test(item.id + ' ' + item.name) ? 'Qwen3-TTS' : /^moss_/.test(item.id) ? 'MOSS-TTS-Nano' : 'Kokoro'); if (!groups.has(label)) groups.set(label, document.createElement('optgroup')); const option = document.createElement('option'); option.value = item.id; option.textContent = item.name.replace(/ · (Kokoro|Pocket|RAVEN|Qwen|MOSS(?:-TTS Nano)?)(?: TTS)?/i, ''); groups.get(label).label = label; groups.get(label).appendChild(option); }
      groups.forEach(group => get('voice').appendChild(group));
      if (Array.from(get('voice').options).some(option => option.value === savedVoice)) get('voice').value = savedVoice;
      voice = get('voice').value; enabled = true; box.hidden = false; updateStyle(); send('reader:hello'); paint();
    }).catch(() => {});
    paint();
  }
  createNativeReader();
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
  if(frame)frame.addEventListener('load',function(){clearTimeout(bridgeTimer);bridgeReady=false;currentPage=null;hideAllMarkers();overlay.classList.remove('fallback');bridgeTimer=setTimeout(function(){if(!bridgeReady&&commentMode)overlay.classList.add('fallback');},800);});
  window.addEventListener('resize',function(){requestRepaint();positionComposer();});window.addEventListener('scroll',function(){requestRepaint();positionComposer();},true);
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
  window.addEventListener('message',function(event){
    if(!frame||event.source!==frame.contentWindow)return;var data=event.data;if(!data||typeof data!=='object')return;
    if(data.type==='tts:hello'){ttsHello();return;}
    if(data.type==='tts:request'){ttsRequest(data);return;}
    if(data.type==='tts:cancel'){ttsCancel(data);return;}
    if(stateBroker.handle(event))return;
    if(data.type!=='anchor:ready'&&data.type!=='anchor:picked'&&data.type!=='anchor:positions'&&data.type!=='anchor:navigate')return;
    if(data.type==='anchor:navigate'){var url=parseOutboundHref(data.href);if(url)showOutbound(url);return;}
    if(data.type==='anchor:ready'){var nextPage=isBundle&&typeof data.page==='string'?data.page:null;if(draftAnchor&&composerBody&&composerBody.value.trim()&&draftAnchor.page!==nextPage&&!window.confirm('Move away from this selected anchor? Your draft comment will remain.')){if(isBundle&&draftAnchor.page)frame.src=bundleRawPrefix+draftAnchor.page.split('/').map(encodeURIComponent).join('/')+'?anchor=1&reader=1'+versionQuery;return;}currentPage=nextPage;bridgeReady=true;hideAllMarkers();if(commentMode){overlay.classList.remove('fallback');postToFrame('anchor:pick-on');}requestRepaint();return;}
    if(data.type==='anchor:picked'){startAnchoredComment(data);return;}if(!Array.isArray(data.anchors))return;
    data.anchors.slice(0,200).forEach(function(pos){if(!pos||typeof pos!=='object'||typeof pos.id!=='string')return;if(pos.id==='__draft__'){if(pos.lost===true){showDraftPosition(0,0,0,0,true);return;}if(typeof pos.x!=='number'||typeof pos.y!=='number'||!Number.isFinite(pos.x)||!Number.isFinite(pos.y))return;if(draftAnchor&&draftAnchor.w!==undefined&&(!Number.isFinite(pos.w)||!Number.isFinite(pos.h)||pos.w<=0||pos.h<=0))return;showDraftPosition(pos.x,pos.y,pos.w||0,pos.h||0,false);return;}var pin=pinById[pos.id];if(!pin||pin.stale||!pinOnCurrentPage(pin))return;if(pos.lost===true){paintPosition(pin,0,0,0,0,true);return;}if(typeof pos.x!=='number'||typeof pos.y!=='number'||!Number.isFinite(pos.x)||!Number.isFinite(pos.y))return;if(pin.w!==null&&pin.h!==null){if(typeof pos.w!=='number'||typeof pos.h!=='number'||!Number.isFinite(pos.w)||!Number.isFinite(pos.h)||pos.w<=0||pos.h<=0)return;paintPosition(pin,pos.x,pos.y,pos.w,pos.h,false);}else paintPosition(pin,pos.x,pos.y,0,0,false);});
  });
  function promptField(label,value){return label+': '+(value==null||value===''?'(none)':String(value));}
  function buildAnchorPrompt(input){var source=input||{},anchor=source.anchor||{},draft=!!source.draft,artifact=String(source.artifactId||''),currentRevision=Number(source.currentRevision)||1,bundle=!!source.isBundle,revision=Number(anchor.artifact_revision||currentRevision)||1,bundlePage=anchor.anchor_page!=null?anchor.anchor_page:anchor.page,kind=anchor.anchor_kind||anchor.kind||(anchor.w!=null?'region':'point'),nodeId=anchor.anchor_node_id||anchor.nodeId,quote=anchor.anchor_quote||anchor.quote,version=Number(anchor.anchor_version||anchor.version)||((nodeId||quote||kind)?2:1),approx=anchor.anchor_approx||anchor.approx,stale=!!anchor.anchor_page_stale||(!draft&&revision!==currentRevision),filePath=bundle?(bundlePage?'/files/'+String(bundlePage):'/files/(bundle entry page)'):'';var read=stale?'Do not switch this stale feedback to the latest revision. Read the exact revision below.':'Read the canonical current revision below.';return ['Artifact MCP review handoff ('+(draft?'draft':'saved')+' anchored feedback)',promptField('Connector','Artifact MCP'),promptField('State',draft?'draft':'saved'),'',promptField('Artifact ID',artifact),promptField('Canonical artifact revision',revision),draft?'Feedback ID: (draft; not persisted yet)':promptField('Feedback ID',anchor.id),promptField('Bundle page',bundlePage),promptField('Bundle file path',filePath),promptField('Selector/path',anchor.anchor_path||anchor.path),promptField('Node ID',nodeId),promptField('Quote',quote),promptField('Anchor kind',kind),promptField('Anchor version',version),promptField('Normalized bounds','x='+((anchor.anchor_x!=null?anchor.anchor_x:anchor.x))+' y='+((anchor.anchor_y!=null?anchor.anchor_y:anchor.y))+' w='+((anchor.anchor_w!=null?anchor.anchor_w:anchor.w))+' h='+((anchor.anchor_h!=null?anchor.anchor_h:anchor.h))),promptField('Approximate',approx?'yes':'no'),promptField('Stale',stale?'yes':'no'),'',draft?promptField('Draft comment',source.body):promptField('Saved comment',source.body),'',draft?'1. Call Artifact MCP list_feedback for artifact '+artifact+' to compare this draft against active feedback.':'1. Call Artifact MCP list_feedback for artifact '+artifact+' and locate feedback '+anchor.id+'.','2. '+read,'3. Prefer resources/read with URI artifact://'+artifact+'/revisions/'+revision+filePath+'.','4. If resources/read is unavailable, use paged read_artifact. A single 65,536-byte read may be incomplete; request subsequent pages before drawing conclusions.'].join('\n');}
  function copyText(text,done,failed){if(navigator.clipboard&&navigator.clipboard.writeText){navigator.clipboard.writeText(text).then(done).catch(function(){fallbackCopy(text,done,failed);});}else fallbackCopy(text,done,failed);}
  function fallbackCopy(text,done,failed){var area=document.createElement('textarea');area.value=text;area.setAttribute('readonly','');area.style.position='fixed';area.style.opacity='0';document.body.appendChild(area);area.select();try{if(document.execCommand&&document.execCommand('copy')){area.remove();done();return;}}catch(_){}area.remove();failed();}
  function copyAnchorPrompt(row,draft,button){var anchor=row||draftAnchor||{},text=buildAnchorPrompt({anchor:anchor,draft:draft,body:draft?(composerBody&&composerBody.value):anchor.body,artifactId:artifactId,currentRevision:Number(shellConfig.revision),isBundle:isBundle});copyText(text,function(){if(button){var old=button.textContent;button.textContent='Copied';setTimeout(function(){button.textContent=old;},1400);}},function(){if(draft&&composerStatus){composerStatus.textContent='Could not copy the prompt. Your draft is still here; select and copy it manually.';composerStatus.classList.add('error');}else if(fbHint){fbHint.textContent='Could not copy prompt.';fbHint.classList.add('error');}});return text;}
  if(window.__artifactMcpTestHooks){window.__artifactMcpTestHooks.buildAnchorPrompt=buildAnchorPrompt;window.__artifactMcpTestHooks.composerPlacement=composerPlacement;window.__artifactMcpTestHooks.markerPreviewPlacement=markerPreviewPlacement;window.__artifactMcpTestHooks.relativeAge=relativeAge;window.__artifactMcpTestHooks.draftAnchorFromSelection=draftAnchorFromSelection;window.__artifactMcpTestHooks.feedbackPayload=feedbackPayload;}
  function updateCount(delta){var n=Math.max(0,(parseInt((fbCounts[0]||{}).textContent,10)||0)+delta);fbCounts.forEach(function(count){count.textContent=n;count.hidden=!n;});}
  function appendFeedback(row){
    var empty=fbList.querySelector('.vfb-empty');if(empty)empty.remove();var item,thread;
    if(row.parent_id){thread=feedbackThreads[row.parent_id];if(!thread)return;var replies=thread.querySelector('.vfb-replies');var holder=document.createElement('div');holder.innerHTML=itemHtml(row,true);item=holder.firstChild;replies.appendChild(item);}
    else{thread=document.createElement('section');thread.className='vfb-thread';thread.setAttribute('data-thread-id',row.id);thread.innerHTML=itemHtml(row,true)+'<div class="vfb-replies"></div>'+replyFormHtml(row.id);fbList.appendChild(thread);feedbackThreads[row.id]=thread;item=thread.querySelector('.vfb-item');}
    feedbackItems[row.id]=item;feedbackRows.push(row);
    var pin=pinFromRow(row);if(pin){pins.push(pin);pinById[pin.id]=pin;requestRepaint();}
    updateCount(1);fbList.scrollTop=fbList.scrollHeight;
  }
  function feedbackPayload(text,parentId,anchor){return {body:text,parent_id:parentId||undefined,anchor:parentId?undefined:(anchor||undefined),anchor_page:anchor&&anchor.page};}
  function sendFeedback(text,parentId,anchor){return fetch('/'+artifactId+'/feedback',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(feedbackPayload(text,parentId,anchor))}).then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Could not send feedback');return d;});});}
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
