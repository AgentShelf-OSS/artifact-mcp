/* Shared confirmations for registry administration and revision restore. */
(function () {
  var sequence = 0;
  function open(options, notice) {
    return new Promise(function (resolve) {
      var launcher = document.activeElement;
      var dialog = document.createElement("dialog");
      var headingId = "artifact-confirm-heading-" + (++sequence);
      var messageId = "artifact-confirm-message-" + sequence;
      dialog.className = "artifact-confirm-dialog";
      dialog.setAttribute("aria-labelledby", headingId);
      dialog.setAttribute("aria-describedby", messageId);
      dialog.innerHTML = '<form method="dialog"><header><div><p class="artifact-confirm-kicker"></p><h2></h2></div><button type="button" data-confirm-close aria-label="Close">×</button></header><div class="artifact-confirm-body"><strong class="artifact-confirm-subject"></strong><p></p></div><footer><button type="button" data-confirm-cancel>Cancel</button><button type="submit" value="confirm" class="artifact-confirm-primary"></button></footer></form>';
      dialog.querySelector("h2").id = headingId;
      dialog.querySelector("h2").textContent = options.title;
      dialog.querySelector(".artifact-confirm-kicker").textContent = notice ? "Action could not be completed" : "Before you continue";
      var subject = dialog.querySelector(".artifact-confirm-subject");
      subject.textContent = options.subject || "";
      subject.hidden = !options.subject;
      var message = dialog.querySelector(".artifact-confirm-body p");
      message.id = messageId;
      message.textContent = options.message;
      var cancel = dialog.querySelector("[data-confirm-cancel]");
      var primary = dialog.querySelector(".artifact-confirm-primary");
      primary.textContent = options.action || "OK";
      if(options.danger) primary.classList.add("is-danger");
      if(notice) cancel.remove();
      else cancel.addEventListener("click", function(){dialog.close("cancel");});
      dialog.querySelector("[data-confirm-close]").addEventListener("click", function(){dialog.close("cancel");});
      dialog.addEventListener("close", function(){
        var confirmed = dialog.returnValue === "confirm";
        dialog.remove();
        if(launcher?.isConnected) launcher.focus();
        resolve(confirmed);
      }, {once:true});
      document.body.appendChild(dialog);
      dialog.showModal();
      (notice ? primary : cancel).focus();
    });
  }
  window.ArtifactDialogs = {
    confirm: function(options){return open(options,false);},
    notice: function(message){return open({title:"Please try again",message:String(message)},true);}
  };
})();
