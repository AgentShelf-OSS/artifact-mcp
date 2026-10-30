  function updateVisibility(card, control) {
    var nextHidden = card.dataset.hidden !== "1";
    control.disabled = true;
    jsonRequest("/" + card.dataset.id + "/visibility", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hidden: nextHidden }),
    }).then(function (body) {
      document.querySelectorAll('.card[data-id="' + CSS.escape(card.dataset.id) + '"]').forEach(function (instance) { instance.dataset.hidden = body.hidden ? "1" : "0"; instance.classList.toggle("is-hidden",!!body.hidden); });
      card.classList.toggle("is-hidden", !!body.hidden);
      control.setAttribute("aria-label", (body.hidden ? "Show " : "Hide ") + card.querySelector(".card-title").textContent.trim() + " in the gallery");
      control.title = body.hidden ? "Show in gallery" : "Hide from gallery";
      control.innerHTML = body.hidden
        ? '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m3 3 18 18"></path><path d="M10.6 6.2A10.5 10.5 0 0 1 12 6c6 0 9.5 6 9.5 6a17.7 17.7 0 0 1-3.1 3.8M6.1 6.1C3.8 7.7 2.5 10 2.5 12c0 0 3.5 6 9.5 6 1.4 0 2.7-.3 3.8-.8"></path><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"></path></svg>'
        : '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"></path><circle cx="12" cy="12" r="2.5"></circle></svg>';
      document.querySelectorAll('.card[data-id="'+CSS.escape(card.dataset.id)+'"] [data-action="visibility"]').forEach(function(button){button.innerHTML=control.innerHTML;button.title=control.title;button.setAttribute("aria-label",control.getAttribute("aria-label"));});
      toast(body.hidden ? "Artifact hidden from Gallery" : "Artifact shown in Gallery");
      document.dispatchEvent(new CustomEvent("artifact:updated", { detail: { id: card.dataset.id, body: body } }));
    }).catch(function (error) {
      toast(error.message || "Could not change visibility");
    }).finally(function () {
      control.disabled = false;
    });
  }

  function updateFavorite(card, control) {
    var nextFavorite = card.dataset.fav !== "1";
    control.disabled = true;
    jsonRequest("/" + card.dataset.id + "/react", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ favorite: nextFavorite, vote: Number(card.dataset.vote || 0) }),
    }).then(function (body) {
      var favorite = !!body.favorite;
      document.querySelectorAll('.card[data-id="' + CSS.escape(card.dataset.id) + '"]').forEach(function (instance) {
        instance.dataset.fav = favorite ? "1" : "0";
        var button = instance.querySelector('[data-action="favorite"]');
        if(button){button.classList.toggle("active",favorite);button.setAttribute("aria-pressed",String(favorite));var text=button.querySelector("span");if(text)text.textContent=favorite?"Saved":"Save";}
      });
      control.classList.toggle("active", favorite);
      control.setAttribute("aria-pressed", favorite ? "true" : "false");
      var label = control.querySelector("span");
      if (label) label.textContent = favorite ? "Saved" : "Save";
      applyFilters();
      toast(favorite ? "Saved to favorites" : "Removed from favorites");
      document.dispatchEvent(new CustomEvent("artifact:updated", { detail: { id: card.dataset.id, body: body } }));
    }).catch(function (error) {
      toast(error.message || "Could not update favorite");
    }).finally(function () {
      control.disabled = false;
    });
  }

  function requestCategory(card, category) {
    return jsonRequest("/" + card.dataset.id + "/category", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ category: category }),
    }).then(function () {
      toast("Category changed to " + (category || "Uncategorized"));
      setTimeout(function () { location.reload(); }, 250);
    });
  }

  function requestMove(card, org, category) {
    return jsonRequest("/" + card.dataset.id + "/move", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ org: org, category: category }),
    }).then(function () {
      toast("Artifact moved to " + org);
      setTimeout(function () { location.reload(); }, 250);
    });
  }

  document.addEventListener("click", function (event) {
    if (notifPanel && !notifPanel.hidden && !event.target.closest(".notif-wrap")) openNotifications(false);

    var more = event.target.closest('[data-action="more"]');
    if (more) {
      openMenu(more);
      return;
    }
    if (!event.target.closest(".card-menu") && !event.target.closest('[data-action="more"]') && !event.target.closest(".category-dialog")) {
      document.querySelectorAll(".card-menu:not([hidden])").forEach(function (menu) {
        closeMenu(menu, false);
      });
    }

    var card = event.target.closest(".card");
    if (!card) return;
    var visibility = event.target.closest('[data-action="visibility"]');
    if (visibility) {
      updateVisibility(card, visibility);
      return;
    }
    var favorite = event.target.closest('[data-action="favorite"]');
    if (favorite) {
      updateFavorite(card, favorite);
      return;
    }
    var share = event.target.closest('[data-action="share"]');
    if (share) {
      openShare(card, share);
      return;
    }
    var del = event.target.closest('[data-action="delete"]');
    if (del) {
      openDelete(card, del);
      return;
    }
    var moveNo = event.target.closest(".move-no");
    if (moveNo) {
      var moveConfirm = moveNo.closest(".move-confirm");
      moveConfirm.hidden = true;
      var orgMenu = card.querySelector(".org-menu");
      orgMenu.value = "";
      rebuildCardCategory(card, card.dataset.org, card.dataset.category || "");
      orgMenu.focus();
      return;
    }
    var moveYes = event.target.closest(".move-yes");
    if (moveYes) {
      var destination = moveYes.closest(".move-confirm").dataset.destination;
      var destinationCategory = card.querySelector(".category-menu").value;
      moveYes.disabled = true;
      requestMove(card, destination, destinationCategory).catch(function (error) {
        moveYes.disabled = false;
        toast(error.message || "Could not move artifact");
      });
    }
  });

  document.addEventListener("change", function (event) {
    var card = event.target.closest(".card");
    if (!card) return;
    var category = event.target.closest('[data-action="category"]');
    if (category) {
      var nextCategory = category.value;
      var orgMenu = card.querySelector(".org-menu");
      var targetOrg = orgMenu && orgMenu.value || card.dataset.org;
      if (nextCategory === createCategoryValue) {
        category.value = category.dataset.stagedCategory || "";
        openCategoryDialog({ mode: "card", org: targetOrg, card: card, restoreFocus: category });
        return;
      }
      category.dataset.stagedCategory = nextCategory;
      if (targetOrg !== card.dataset.org) {
        stageMove(card, targetOrg);
        return;
      }
      category.disabled = true;
      requestCategory(card, nextCategory).catch(function (error) {
        category.disabled = false;
        toast(error.message || "Could not change category");
      });
      return;
    }
    var org = event.target.closest('[data-action="move-org"]');
    if (org) {
      var destination = org.value || card.dataset.org;
      var confirm = card.querySelector(".move-confirm");
      if (destination === card.dataset.org) {
        confirm.hidden = true;
        rebuildCardCategory(card, card.dataset.org, card.dataset.category || "");
        return;
      }
      var available = categoryRows(destination).some(function (row) { return row.name === card.dataset.category; });
      var selected = rebuildCardCategory(card, destination, available ? card.dataset.category : "");
      stageMove(card, destination);
      card.querySelector(".category-menu").value = selected;
      card.querySelector(".category-menu").focus();
    }
  });

