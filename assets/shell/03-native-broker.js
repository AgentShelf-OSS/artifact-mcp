  // ADR-0018: when the Android app hosts the viewer, the shell relays consented native capabilities
  // (background location, car audio, notifications). The artifact frame never sees the host object.
  function createNativeBroker(options) {
    var host=null,ready=false,capabilities=[],helloSent=false,artifactListening=false,pendingPlayed=[],waiters=[];
    var idPattern=/^[A-Za-z0-9._:-]{1,64}(?![\s\S])/,artifact=String(options.artifactId||'');
    var consentPrefix='artifact-native:'+artifact+':';
    function send(type,fields){options.post(Object.assign({type:type},fields||{}));}
    // The app rejects unknown fields; hello carries only v and type (ADR-0018 protocol v1).
    function toHost(message){if(!host)return false;try{host.postMessage(JSON.stringify(message.type==='hello'?{v:1,type:'hello'}:Object.assign({v:1,artifactId:artifact},message)));return true;}catch(_){return false;}}
    function allowed(kind){try{return options.storage.getItem(consentPrefix+kind)==='allow';}catch(_){return false;}}
    function remember(kind){try{options.storage.setItem(consentPrefix+kind,'allow');}catch(_){}}
    function forget(kind){try{options.storage.removeItem(consentPrefix+kind);}catch(_){}}
    function consent(kind){if(allowed(kind))return Promise.resolve(true);return Promise.resolve().then(function(){return options.ask(kind);}).then(function(answer){if(answer==='always')remember(kind);return answer==='always'||answer==='once';},function(){return false;});}
    function str(value,max){return typeof value==='string'&&value.length>0&&value.length<=max;}
    function cleanItem(item,geo){
      if(!item||typeof item!=='object'||Array.isArray(item))return null;
      if(!str(item.id,64)||!idPattern.test(item.id)||!str(item.title,120))return null;
      var out={id:item.id,title:item.title};
      if(str(item.text,1500))out.text=item.text;
      else if(str(item.url,2048)){try{var u=new URL(item.url,options.origin);if(u.origin!==options.origin)return null;out.url=u.href;}catch(_){return null;}}
      else return null;
      if(item.voice!==undefined){if(!str(item.voice,64)||!/^[A-Za-z0-9_-]{1,64}$/.test(item.voice))return null;out.voice=item.voice;}
      if(geo){
        if(!Number.isFinite(item.lat)||!Number.isFinite(item.lon)||Math.abs(item.lat)>90||Math.abs(item.lon)>180)return null;
        var radius=Number.isFinite(item.radius_m)?Math.round(item.radius_m):3000;
        if(radius<500||radius>10000)return null;
        out.lat=item.lat;out.lon=item.lon;out.radius_m=radius;
      }
      return out;
    }
    function flushPlayed(){if(!artifactListening||!pendingPlayed.length)return;var ids=[];pendingPlayed.splice(0).forEach(function(p){send('audio:played',{id:p.id,at:p.at});ids.push(p.id);});toHost({type:'played.ack',ids:ids});}
    function onHost(raw){
      var m;try{m=typeof raw==='string'?JSON.parse(raw):raw;}catch(_){return;}
      if(!m||m.v!==1||typeof m.type!=='string')return;
      if(m.type==='ready'){ready=true;capabilities=Array.isArray(m.capabilities)?m.capabilities.filter(function(c){return typeof c==='string';}):[];chaptersSent='';sendChapters();waiters.splice(0).forEach(function(w){w();});return;}
      if(m.artifactId!==artifact)return;
      if(m.type==='geo.position'&&Number.isFinite(m.lat)&&Number.isFinite(m.lon))send('geo:position',{requestId:String(m.requestId||''),lat:m.lat,lon:m.lon,accuracy:Number.isFinite(m.accuracy)?m.accuracy:null,timestamp:Number.isFinite(m.timestamp)?m.timestamp:Date.now()});
      else if(m.type==='geo.error'){if(m.reason==='denied')forget('location');send('geo:error',{requestId:String(m.requestId||''),reason:['denied','unavailable','timeout'].indexOf(m.reason)>=0?m.reason:'unavailable'});}
      else if(m.type==='audio.state'&&str(m.id,64)&&['queued','playing','done','error'].indexOf(m.state)>=0)send('audio:state',{id:m.id,state:m.state});
      else if(m.type==='audio.played'&&str(m.id,64)){if(!pendingPlayed.some(function(p){return p.id===m.id;}))pendingPlayed.push({id:m.id,at:Number.isFinite(m.at)?m.at:Date.now()});flushPlayed();}
      else if(m.type==='media.command'&&typeof m.action==='string'){var run=mediaHandlers&&mediaHandlers[m.action];if(typeof run!=='function')return;
        // ADR-0022: forward and chapter come only from apps advertising media.progress.
        if((m.action==='forward'||m.action==='chapter')&&!has('media.progress'))return;
        if(m.action==='chapter'){if(!Number.isInteger(m.index)||m.index<0||m.index>499||!chapters||m.index>=chapters.length)return;try{run(m.index);}catch(_){}return;}
        try{run();}catch(_){}}
      else if(m.type==='error')send('native:error',{requestId:String(m.requestId||''),reason:typeof m.reason==='string'?m.reason.slice(0,40):'unavailable'});
    }
    function start(){
      host=options.host();if(!host||helloSent)return;helloSent=true;
      try{if(typeof host.addEventListener==='function')host.addEventListener('message',function(e){onHost(e&&e.data);});else host.onmessage=function(e){onHost(e&&e.data);};}catch(_){host=null;return;}
      toHost({type:'hello'});
    }
    function whenReady(){if(ready||!host)return Promise.resolve(ready);return new Promise(function(resolve){var done=false;waiters.push(function(){if(!done){done=true;resolve(true);}});options.setTimer(function(){if(!done){done=true;resolve(ready);}},1500);});}
    function has(cap){return ready&&capabilities.indexOf(cap)>=0;}
    var mediaHandlers=null,lastMirror='',lastShape='',lastMirrorAt=-Infinity,chapters=null,chaptersSent='';
    function now(){return typeof options.now==='function'?options.now():Date.now();}
    function seconds(value){return Number.isFinite(value)?Math.round(Math.max(0,Math.min(86400,value))*10)/10:null;}
    function mirrorMedia(state){
      if(!options.enabled||!has('media.mirror')||!state)return;
      var progress=has('media.progress'),cut=function(v){return typeof v==='string'?v.slice(0,200):'';};
      // Apps without media.progress know only playing and paused; buffering means "wants to play".
      var live=state.state==='buffering'?(progress?'buffering':'playing'):state.state==='playing'?'playing':'paused';
      var message=state.state==='none'?{type:'media.session',state:'none',title:'',artist:'',album:'',actions:[]}:
        {type:'media.session',state:live,title:cut(state.title)||'Listen',artist:cut(state.artist),album:cut(state.album),actions:['play','pause','next','previous','stop']};
      // Apps from before ADR-0021 reject unknown fields, so only hosts advertising media.rate get it.
      if(message.state!=='none'&&has('media.rate')){message.actions=message.actions.concat(['rewind','speed']);if(typeof state.rate==='string'&&/^\d(?:\.\d{1,2})?$/.test(state.rate))message.rate=state.rate;}
      // ADR-0022: progress, chapters and failure reasons only for hosts advertising media.progress.
      if(progress){
        if(message.state!=='none'){
          message.actions=message.actions.concat(chapters&&chapters.length?['forward','chapter']:['forward']);
          var position=seconds(state.position),duration=seconds(state.duration);
          if(position!==null)message.position=position;
          if(duration!==null)message.duration=duration;
          if(Number.isInteger(state.chapterIndex)&&state.chapterIndex>=0&&chapters&&state.chapterIndex<chapters.length)message.chapterIndex=state.chapterIndex;
        }
        if((message.state==='paused'||message.state==='none')&&typeof state.error==='string'&&state.error.trim())message.error=state.error.trim().slice(0,120);
      }
      mediaHandlers=state.state==='none'?null:(state.handlers||null);
      var key=JSON.stringify(message);if(key===lastMirror)return;
      // Position and duration alone are resent at most every 5 s; the app interpolates between.
      var shape=JSON.stringify(Object.assign({},message,{position:undefined,duration:undefined})),at=now();
      if(progress&&shape===lastShape&&at-lastMirrorAt<5000)return;
      lastMirror=key;lastShape=shape;lastMirrorAt=at;toHost(message);
    }
    function sendChapters(){
      if(!options.enabled||!chapters||!has('media.mirror')||!has('media.progress'))return;
      var key=JSON.stringify(chapters);if(key===chaptersSent)return;chaptersSent=key;toHost({type:'media.chapters',chapters:chapters.slice()});
    }
    function mirrorChapters(list){
      if(!Array.isArray(list))return;
      chapters=list.slice(0,500).map(function(title,index){var text=typeof title==='string'?title.trim().slice(0,200):'';return text||'Chapter '+(index+1);});
      sendChapters();
    }
    function fail(requestId,reason){send('native:error',{requestId:requestId,reason:reason});}
    function handleMessage(data){
      var type=data.type,requestId=typeof data.requestId==='string'&&idPattern.test(data.requestId)?data.requestId:'';
      if(type==='native:hello'){artifactListening=true;whenReady().then(function(){send('native:ready',{available:!!(options.enabled&&ready),capabilities:options.enabled&&ready?capabilities.slice():[]});flushPlayed();});return;}
      if(!options.enabled||!ready){if(type!=='audio:stop')fail(requestId,'unsupported');return;}
      if(type==='audio:stop'){toHost({type:'audio.stop'});return;}
      if(!requestId){fail('','bad_request');return;}
      if(type==='geo:watch'){if(!has('geo.watch')){fail(requestId,'unsupported');return;}
        var interval=Number.isFinite(data.intervalMs)?Math.max(15000,Math.min(600000,Math.round(data.intervalMs))):60000;
        consent('location').then(function(ok){if(!ok){send('geo:error',{requestId:requestId,reason:'declined'});return;}toHost({type:'geo.watch',requestId:requestId,intervalMs:interval,highAccuracy:data.highAccuracy===true});});return;}
      if(type==='geo:unwatch'){toHost({type:'geo.unwatch',requestId:requestId});return;}
      if(type==='audio:queue'){if(!has('audio.queue')){fail(requestId,'unsupported');return;}var item=cleanItem(data.item,false);if(!item){fail(requestId,'bad_request');return;}toHost({type:'audio.queue',requestId:requestId,item:item});return;}
      if(type==='audio:geotrigger'){if(!has('audio.geotrigger')){fail(requestId,'unsupported');return;}
        if(!Array.isArray(data.items)||data.items.length>50){fail(requestId,'bad_request');return;}
        var items=data.items.map(function(i){return cleanItem(i,true);});if(items.some(function(i){return !i;})){fail(requestId,'bad_request');return;}
        if(!items.length){toHost({type:'audio.geotrigger',requestId:requestId,items:[]});return;}
        consent('location').then(function(ok){if(!ok){fail(requestId,'declined');return;}toHost({type:'audio.geotrigger',requestId:requestId,items:items});});return;}
      if(type==='notify:show'){if(!has('notify')){fail(requestId,'unsupported');return;}if(!str(data.title,120)||!str(data.body,500)){fail(requestId,'bad_request');return;}
        consent('notify').then(function(ok){if(!ok){fail(requestId,'declined');return;}toHost({type:'notify.show',requestId:requestId,title:data.title,body:data.body});});return;}
    }
    return {start:start,mirrorMedia:mirrorMedia,mirrorChapters:mirrorChapters,hostHas:function(cap){return whenReady().then(function(){return has(cap);});},handle:function(event){
      if(event.source!==options.frame())return false;var data=event.data;
      if(!data||typeof data!=='object'||Array.isArray(data)||typeof data.type!=='string')return false;
      if(!/^(native:hello|geo:watch|geo:unwatch|audio:queue|audio:geotrigger|audio:stop|notify:show)$/.test(data.type))return false;
      handleMessage(data);return true;
    },receive:onHost};
  }
  // End native broker.
