  // Location stays behind a consent the viewer gives in the shell for each artifact, because the
  // browser's own permission covers the whole origin and therefore every artifact on it.
  function createViewerGeoBroker(options) {
    var consentKey='artifact-geo:'+options.artifactId,busy=false,asking=false,lastAt=-Infinity;
    var idPattern=/^[A-Za-z0-9._:-]{1,64}(?![\s\S])/;
    function send(type,fields){options.post(Object.assign({type:type},fields));}
    function remembered(){try{return options.storage.getItem(consentKey)==='allow';}catch(_){return false;}}
    function remember(){try{options.storage.setItem(consentKey,'allow');}catch(_){}}
    function forget(){try{options.storage.removeItem(consentKey);}catch(_){}}
    function ready(){send('geo:ready',{supported:!!options.geolocation,remembered:remembered()});}
    function locate(id,high){
      busy=true;lastAt=options.now();
      try{
        options.geolocation.getCurrentPosition(function(position){
          busy=false;var coords=position&&position.coords||{};
          if(!Number.isFinite(coords.latitude)||!Number.isFinite(coords.longitude)){send('geo:error',{requestId:id,reason:'unavailable'});return;}
          send('geo:position',{requestId:id,lat:coords.latitude,lon:coords.longitude,accuracy:Number.isFinite(coords.accuracy)?coords.accuracy:null,timestamp:Number.isFinite(position.timestamp)?position.timestamp:options.now()});
        },function(failure){
          busy=false;var code=failure&&failure.code;if(code===1)forget();
          send('geo:error',{requestId:id,reason:code===1?'denied':code===3?'timeout':'unavailable'});
        },{enableHighAccuracy:high===true,timeout:20000,maximumAge:60000});
      }catch(_){busy=false;send('geo:error',{requestId:id,reason:'unavailable'});}
    }
    function request(data){
      var id=data.requestId;
      if(typeof id!=='string'||!idPattern.test(id)){send('geo:error',{requestId:'',reason:'bad_request'});return;}
      if(!options.enabled||!options.geolocation){send('geo:error',{requestId:id,reason:'unsupported'});return;}
      if(busy||asking||options.now()-lastAt<2000){send('geo:error',{requestId:id,reason:'busy'});return;}
      if(remembered()){locate(id,data.highAccuracy);return;}
      asking=true;
      Promise.resolve().then(options.ask).then(function(answer){
        asking=false;if(answer==='always')remember();
        if(answer==='always'||answer==='once')locate(id,data.highAccuracy);
        else send('geo:error',{requestId:id,reason:'declined'});
      },function(){asking=false;send('geo:error',{requestId:id,reason:'declined'});});
    }
    return {handle:function(event){
      if(event.source!==options.frame())return false;
      var data=event.data;
      if(!data||typeof data!=='object'||Array.isArray(data)||typeof data.type!=='string'||!data.type.startsWith('geo:'))return false;
      if(data.type==='geo:hello')ready();
      else if(data.type==='geo:request')request(data);
      else if(data.type==='geo:forget'){forget();ready();}
      return true;
    }};
  }
  // End viewer geolocation broker.
