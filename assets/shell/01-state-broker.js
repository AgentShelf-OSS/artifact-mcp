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
