  // Viewer-safe discussion status loads independently so a transient status failure never
  // blocks the artifact shell. Only the server-rendered management capability gets controls.
  var discussion=document.getElementById('vdiscussion'),discussionState=document.getElementById('vdiscussion-state'),discussionCopy=document.getElementById('vdiscussion-copy'),discussionActions=document.getElementById('vdiscussion-actions'),discussionStatus=document.getElementById('vdiscussion-status'),canManageDiscussion=shellConfig.canManageDiscussion==='1',discussionLoaded=false,discussionLoading=false;
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
  function loadDiscussion(focusAction){if(!discussion||discussionLoaded||discussionLoading)return;discussionLoading=true;fetch('/'+artifactId+'/discussion/override').then(function(response){return response.json().then(function(body){return {ok:response.ok,body:body};});}).then(function(result){if(!result.ok)throw new Error(result.body&&result.body.error||'Could not load discussion status.');discussionLoaded=true;renderDiscussion(result.body,focusAction);if(!result.body.actionableError)discussionMessage('');}).catch(function(error){discussionState.textContent='Status unavailable';discussionCopy.textContent='Discussion status could not be loaded. Artifact content and feedback remain available.';if(discussionActions){discussionActions.hidden=true;discussionActions.textContent='';}discussionMessage(error.message||'Could not load discussion status.',true);}).finally(function(){discussionLoading=false;});}
  if(discussionActions)discussionActions.addEventListener('click',function(event){var button=event.target.closest('[data-discussion-action]');if(!button)return;var action=button.dataset.discussionAction,all=[].slice.call(discussionActions.querySelectorAll('button')),working=action==='inherit'?'Restoring outbound organization default…':action==='artifact_only'?'Keeping discussion in Artifact MCP…':'Enabling guarded two-way sync…',done=action==='inherit'?'New comments will follow the organization outbound default.':action==='artifact_only'?'This artifact will keep discussion in Artifact MCP.':'Two-way Discord sync is enabled for this mapped thread.';all.forEach(function(item){item.disabled=true;});discussionMessage(working);fetch('/'+artifactId+'/discussion/override',{method:'PUT',headers:{'content-type':'application/json'},body:JSON.stringify({override:action})}).then(function(response){return response.json().then(function(body){return {ok:response.ok,body:body};});}).then(function(result){if(!result.ok)throw new Error(result.body&&result.body.error||'Could not update discussion settings.');discussionLoaded=true;renderDiscussion(result.body,action);discussionMessage(done);}).catch(function(error){discussionMessage(error.message||'Could not update discussion settings.',true);all.forEach(function(item){item.disabled=false;});button.focus();});});

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

