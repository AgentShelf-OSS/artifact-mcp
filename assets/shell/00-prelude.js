
(function(){
  // Gain (dB) that brings each Pocket preset to about -19 LUFS integrated, measured on one
  // shared passage (October 2026) and capped at +12 dB; the limiter catches peaks. Unknown voices use 0 dB.
  const VOICE_GAIN_DB=Object.freeze({pocket_alba:0,pocket_marius:8.5,pocket_javert:7,pocket_jean:0,pocket_cosette:11.5,pocket_eponine:1.5,pocket_fantine:1,pocket_azelma:1.5,pocket_anna:1,pocket_bill_boerst:8,pocket_caro_davy:7,pocket_charles:3.5,pocket_eve:2,pocket_george:1,pocket_jane:1.5,pocket_mary:1.5,pocket_michael:2.5,pocket_paul:0.5,pocket_peter_yearsley:10.5,pocket_stuart_bell:12,pocket_vera:2.5});
  var stateBoot=window.__artifactMcpStateBoot,pendingStateHello=stateBoot?stateBoot.take():null;
  // See `assets/portal.js`: portal mutations use a non-simple header as a stateless CSRF signal.
  var nativeFetch=window.fetch;
  window.fetch=function portalFetch(input,init){var options=init||{},method=String(options.method||(input&&input.method)||'GET').toUpperCase();if(method==='POST'||method==='PUT'||method==='PATCH'||method==='DELETE'){var headers=new Headers(options.headers||(input&&input.headers));headers.set('x-artifact-mutation','1');options=Object.assign({},options,{headers:headers});}return nativeFetch.call(window,input,options);};
  var shellConfig=document.getElementById('shell-config').dataset;
  function configLiteral(name){return JSON.parse(shellConfig[name]);}
  var artifactId=configLiteral('artifactId'),prevId=configLiteral('prevId'),nextId=configLiteral('nextId'),bundleRawPrefix=configLiteral('bundleRawPrefix'),versionQuery=configLiteral('versionQuery');
