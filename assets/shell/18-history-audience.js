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
  if(histList){histList.addEventListener('click',async function(e){
    var b=e.target.closest('.vh-restore');if(!b)return;
    var rev=b.getAttribute('data-rev');
    if(!await window.ArtifactDialogs.confirm({title:'Restore revision',subject:'Version '+rev,message:'This creates a new revision at the same URL. All existing revisions will remain available.',action:'Restore revision'}))return;
    b.disabled=true;b.textContent='Restoring…';
    fetch('/'+artifactId+'/restore',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({revision:Number(rev)})})
      .then(function(r){return r.json().then(function(d){if(!r.ok)throw new Error(d.error||'Restore failed');return d;});})
      .then(function(){location.reload();})
      .catch(function(err){b.disabled=false;b.textContent='Restore';window.ArtifactDialogs.notice(err.message||'Restore failed. Try again.');});
  });}

  // Admin-only audience drawer. Its markup is absent for regular org viewers.
  var viewToggle=document.getElementById('vview-toggle');
  function viewOpen(open){if(!document.getElementById('inspector-audience'))return;if(open)inspectorOpen('audience',viewToggle);else inspectorClosePanel();}
})();
