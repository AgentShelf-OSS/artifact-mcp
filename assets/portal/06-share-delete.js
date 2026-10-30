  var shareDialog = document.getElementById("share-dialog");
  var shareTitle = document.getElementById("share-title");
  var shareList = document.getElementById("share-list");
  var shareForm = document.getElementById("share-form");
  var shareExpiry = document.getElementById("share-expiry");
  var shareArtifactId = "";
  var shareTrigger = null;

  function renderShares(rows) {
    if (!shareList) return;
    shareList.replaceChildren();
    if (!rows.length) {
      var emptyRow = document.createElement("p");
      emptyRow.className = "share-copy";
      emptyRow.textContent = "No active public links.";
      shareList.appendChild(emptyRow);
      return;
    }
    rows.forEach(function (share) {
      var row = document.createElement("div");
      row.className = "share-row";
      var url = document.createElement("span");
      url.className = "share-url";
      url.textContent = share.url || (location.origin + "/s/" + share.token);
      var copy = document.createElement("button");
      copy.type = "button";
      copy.textContent = "Copy";
      copy.addEventListener("click", function () {
        var value = url.textContent;
        var fallback = function () {
          var selection = window.getSelection();
          var range = document.createRange();
          range.selectNodeContents(url);
          selection.removeAllRanges();
          selection.addRange(range);
          toast("Link selected. Copy it manually");
        };
        if (!navigator.clipboard) {
          fallback();
          return;
        }
        navigator.clipboard.writeText(value).then(function () {
          toast("Share link copied");
        }).catch(fallback);
      });
      var revoke = document.createElement("button");
      revoke.type = "button";
      revoke.className = "revoke-share";
      revoke.textContent = "Revoke";
      revoke.addEventListener("click", function () {
        revoke.disabled = true;
        jsonRequest("/" + shareArtifactId + "/shares/" + encodeURIComponent(share.token), { method: "DELETE" })
          .then(loadShares)
          .catch(function (error) {
            revoke.disabled = false;
            toast(error.message || "Could not revoke link");
          });
      });
      row.append(url, copy, revoke);
      shareList.appendChild(row);
    });
  }

  function loadShares() {
    if (!shareList || !shareArtifactId) return Promise.resolve();
    shareList.textContent = "Loading links…";
    return jsonRequest("/" + shareArtifactId + "/shares", { headers: { accept: "application/json" } })
      .then(function (body) {
        renderShares(body.shares || []);
      })
      .catch(function (error) {
        shareList.textContent = error.message || "Could not load links.";
      });
  }

  function openShare(card, trigger) {
    if (!shareDialog) return;
    shareArtifactId = card.dataset.id;
    shareTrigger = trigger;
    shareTitle.textContent = "Share " + card.querySelector(".card-title").textContent.trim();
    shareDialog.showModal();
    loadShares();
  }

  if (shareForm) {
    shareForm.addEventListener("submit", function (event) {
      event.preventDefault();
      var submit = shareForm.querySelector('button[type="submit"]');
      submit.disabled = true;
      jsonRequest("/" + shareArtifactId + "/shares", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ expires: shareExpiry.value || null }),
      }).then(function () {
        shareExpiry.value = "";
        toast("Public link created");
        return loadShares();
      }).catch(function (error) {
        toast(error.message || "Could not create link");
      }).finally(function () {
        submit.disabled = false;
      });
    });
  }
  if (shareDialog) {
    shareDialog.addEventListener("close", function () {
      if (shareTrigger) shareTrigger.focus();
    });
  }

  var deleteDialog = document.getElementById("delete-dialog");
  var deleteTitle = document.getElementById("delete-title");
  var deleteContext = document.getElementById("delete-context");
  var deleteError = document.getElementById("delete-error");
  var deleteConfirm = document.getElementById("delete-confirm");
  var deleteCard = null;
  var deleteTrigger = null;

  function openDelete(card, trigger) {
    if (!deleteDialog) return;
    deleteCard = card;
    deleteTrigger = trigger;
    closeMenu(card.querySelector(".card-menu"), false);
    var title = card.querySelector(".card-title").textContent.trim();
    deleteTitle.textContent = 'Delete "' + title + '"?';
    deleteContext.textContent = card.dataset.org + " · /" + card.dataset.id;
    deleteError.textContent = "";
    deleteDialog.showModal();
    var cancel = deleteDialog.querySelector(".delete-cancel");
    if (cancel) setTimeout(function () { cancel.focus(); }, 0);
  }

  if (deleteDialog) {
    deleteDialog.addEventListener("close", function () {
      deleteError.textContent = "";
      deleteConfirm.disabled = false;
      deleteConfirm.textContent = "Delete artifact";
      if (deleteTrigger && deleteTrigger.isConnected) deleteTrigger.focus();
      deleteCard = null;
      deleteTrigger = null;
    });
  }

  if (deleteConfirm) {
    deleteConfirm.addEventListener("click", function () {
      if (!deleteCard) return;
      var card = deleteCard;
      deleteConfirm.disabled = true;
      deleteConfirm.textContent = "Deleting…";
      deleteError.textContent = "";
      jsonRequest("/" + card.dataset.id, { method: "DELETE", headers: { accept: "application/json" } })
        .then(function () {
          deleteDialog.close("deleted");
          card.style.opacity = "0";
          card.style.transform = "scale(.975)";
          setTimeout(function () {
            document.querySelectorAll('.card[data-id="' + CSS.escape(card.dataset.id) + '"]').forEach(function(instance){instance.remove();});
            sortDirty = true;
            applyFilters();
            document.dispatchEvent(new CustomEvent("artifact:updated",{detail:{id:card.dataset.id,deleted:true}}));
            toast("Artifact deleted");
          }, 210);
        })
        .catch(function (error) {
          deleteConfirm.disabled = false;
          deleteConfirm.textContent = "Delete artifact";
          deleteError.textContent = error.message || "Could not delete artifact";
          deleteConfirm.focus();
        });
    });
  }

