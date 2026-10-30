  // Reminder broker (ADR-0012). The artifact asks; the shell owns permission, subscription, and opt-in.
  // Adapters keep browser APIs injectable so tests can run the real broker.
  function createViewerReminderBroker(options){
    var keyPattern=/^[A-Za-z0-9._-]{1,64}$/,artifactPath='/'+encodeURIComponent(options.artifactId),readySent=false,config=null,configPromise=null,optedIn=false,busy=false,lastStatus='';
    var mutationHeaders={'content-type':'application/json','x-artifact-mutation':'1'};
    function send(type,fields){options.post(Object.assign({type:type},fields||{}));}
    function requestIdOf(data){return typeof data.requestId==='string'&&data.requestId.length<=80?data.requestId:undefined;}
    function withRequest(fields,requestId){if(requestId!==undefined)fields.requestId=requestId;return fields;}
    function permission(){var value=options.platform.permission();return value==='granted'||value==='denied'||value==='default'?value:'unsupported';}
    function serverEnabled(){return !!(options.enabled&&config&&config.enabled===true&&typeof config.vapid_public_key==='string'&&config.vapid_public_key);}
    function status(){var server=serverEnabled(),needsInstall=server&&options.platform.needsInstall();return {enabled:server&&options.platform.supported(),optedIn:server&&optedIn,permission:permission(),needsInstall:needsInstall};}
    function publish(force){var current=status(),signature=JSON.stringify(current);if(options.onStatus)options.onStatus(current,busy);if(signature===lastStatus&&!force)return current;lastStatus=signature;if(readySent)send('reminder:status',current);return current;}
    async function json(url,init){
      var response;
      try{response=await options.fetch(url,Object.assign({credentials:'same-origin',cache:'no-store'},init||{}));}catch(_){var offline=new Error('network');offline.reason='network';throw offline;}
      var body=response.status===204?{}:await response.json().catch(function(){return {};});
      if(!response.ok){var failure=new Error('reminder request failed');failure.status=response.status;failure.body=body||{};failure.reason=reason(failure);throw failure;}
      return body||{};
    }
    function reason(failure){
      var code=failure.body&&typeof failure.body.error==='string'?failure.body.error:'';
      if(code==='push_disabled')return 'disabled';
      if(['bad_time','bad_text','reminder_limit','bad_key','bad_scope','bad_endpoint','bad_keys'].indexOf(code)>=0)return code;
      if(failure.status===409)return 'reminder_limit';
      if(failure.status===401||failure.status===403||failure.status===404)return 'forbidden';
      if(failure.status===429)return 'rate_limited';
      if(failure.status===400)return 'bad_request';
      return 'network';
    }
    function loadConfig(){
      if(!options.enabled)return Promise.resolve(status());
      if(configPromise)return configPromise;
      configPromise=json('/push/config').then(function(body){config=body&&typeof body==='object'?body:{enabled:false};}).catch(function(){config={enabled:false};}).then(function(){
        if(!serverEnabled())return;
        return json(artifactPath+'/push').then(function(body){optedIn=body.opted_in===true;}).catch(function(){optedIn=false;});
      }).then(function(){return publish(false);});
      return configPromise;
    }
    function base64UrlBytes(value){var text=String(value).replace(/-/g,'+').replace(/_/g,'/');while(text.length%4)text+='=';var raw=options.platform.atob(text),bytes=new Uint8Array(raw.length);for(var i=0;i<raw.length;i++)bytes[i]=raw.charCodeAt(i);return bytes;}
    function sameKey(subscription,key){try{var current=subscription.options&&subscription.options.applicationServerKey;if(!current)return true;var a=new Uint8Array(current);if(a.length!==key.length)return false;for(var i=0;i<a.length;i++)if(a[i]!==key[i])return false;return true;}catch(_){return true;}}
    // Call from a click handler: requestPermission must run before any other await to keep the user gesture.
    async function turnOn(){
      if(busy)return status();
      if(!serverEnabled()||!options.platform.supported()){var unavailable=new Error('disabled');unavailable.reason=options.platform.needsInstall()?'needs_install':'disabled';throw unavailable;}
      busy=true;publish(false);
      try{
        var granted=await options.platform.requestPermission();
        if(granted!=='granted'){var blocked=new Error('permission');blocked.reason=granted==='denied'?'permission_denied':'permission_dismissed';throw blocked;}
        var registration=await options.platform.register('/sw.js');
        var key=base64UrlBytes(config.vapid_public_key),subscription=await registration.pushManager.getSubscription();
        if(subscription&&!sameKey(subscription,key)){await subscription.unsubscribe();subscription=null;}
        if(!subscription)subscription=await registration.pushManager.subscribe({userVisibleOnly:true,applicationServerKey:key});
        var detail=subscription.toJSON();
        await json('/push/subscriptions',{method:'PUT',headers:mutationHeaders,body:JSON.stringify({endpoint:detail.endpoint,keys:{p256dh:detail.keys&&detail.keys.p256dh,auth:detail.keys&&detail.keys.auth},label:options.platform.label()})});
        await json(artifactPath+'/push/optin',{method:'PUT',headers:mutationHeaders,body:'{}'});
        optedIn=true;
        return status();
      }catch(error){if(!error||!error.reason){var failed=new Error('subscribe');failed.reason='subscribe_failed';throw failed;}throw error;}
      finally{busy=false;publish(false);}
    }
    async function turnOff(){
      if(busy)return status();
      busy=true;publish(false);
      try{await json(artifactPath+'/push/optin',{method:'DELETE',headers:{'x-artifact-mutation':'1'}});optedIn=false;return status();}
      finally{busy=false;publish(false);}
    }
    function timeFields(data){
      var hasDelay=data.delaySeconds!==undefined,hasFire=data.fireAt!==undefined;
      if(hasDelay===hasFire)return null;
      if(hasDelay)return Number.isSafeInteger(data.delaySeconds)?{delay_seconds:data.delaySeconds}:null;
      return Number.isSafeInteger(data.fireAt)?{fire_at:data.fireAt}:null;
    }
    function set(data,key,scope,requestId){
      var time=timeFields(data);
      if(!time){send('reminder:error',withRequest({key:key,reason:'bad_time'},requestId));return;}
      if(typeof data.title!=='string'||(data.body!==undefined&&typeof data.body!=='string')){send('reminder:error',withRequest({key:key,reason:'bad_text'},requestId));return;}
      var body=Object.assign({},time,{title:data.title});if(data.body!==undefined)body.body=data.body;
      json(artifactPath+'/reminders/'+encodeURIComponent(key)+'?scope='+scope,{method:'PUT',headers:mutationHeaders,body:JSON.stringify(body)}).then(function(saved){
        send('reminder:saved',withRequest({key:key,scope:typeof saved.scope==='string'?saved.scope:scope,fireAt:Number.isSafeInteger(saved.fire_at)?saved.fire_at:null},requestId));
      }).catch(function(error){send('reminder:error',withRequest({key:key,reason:error&&error.reason||'network'},requestId));});
    }
    function clear(key,scope,requestId){
      json(artifactPath+'/reminders/'+encodeURIComponent(key)+'?scope='+scope,{method:'DELETE',headers:{'x-artifact-mutation':'1'}}).then(function(){
        send('reminder:cleared',withRequest({key:key,scope:scope},requestId));
      }).catch(function(error){send('reminder:error',withRequest({key:key,reason:error&&error.reason||'network'},requestId));});
    }
    function prompt(data){
      loadConfig().then(function(current){
        if(!serverEnabled()||!(current.enabled||current.needsInstall))return;
        if(current.permission==='denied'||(current.optedIn&&current.permission==='granted'))return;
        var text=typeof data.reason==='string'?Array.from(data.reason.replace(/[\u0000-\u001f\u007f]+/g,' ').trim()).slice(0,120).join(''):'';
        if(options.showPrompt)options.showPrompt(text,current);
      });
    }
    function handle(event){
      if(event.source!==options.frame())return false;
      var data=event.data;
      if(!data||typeof data!=='object'||Array.isArray(data)||typeof data.type!=='string'||data.type.indexOf('reminder:')!==0)return false;
      var type=data.type,requestId=requestIdOf(data),key=typeof data.key==='string'?data.key:undefined,scope=data.scope===undefined?'org':data.scope;
      if(type==='reminder:hello'){loadConfig().then(function(){var current=status();lastStatus=JSON.stringify(current);readySent=true;send('reminder:ready',current);});return true;}
      if(type==='reminder:prompt'){if(options.enabled)prompt(data);return true;}
      if(type!=='reminder:set'&&type!=='reminder:clear')return true;
      var errorKey=key===undefined?{}:{key:key};
      if(!options.enabled){send('reminder:error',withRequest(Object.assign(errorKey,{reason:'disabled'}),requestId));return true;}
      if(key===undefined||!keyPattern.test(key)){send('reminder:error',withRequest(Object.assign(errorKey,{reason:'bad_key'}),requestId));return true;}
      if(scope!=='org'&&scope!=='viewer'){send('reminder:error',withRequest({key:key,reason:'bad_scope'},requestId));return true;}
      if(type==='reminder:set')set(data,key,scope,requestId);else clear(key,scope,requestId);
      return true;
    }
    return {handle:handle,load:loadConfig,turnOn:turnOn,turnOff:turnOff,status:status,refresh:function(){return publish(false);}};
  }
  // End viewer reminder broker.
