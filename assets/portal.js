(function () {
  "use strict";

  // Every portal mutation carries the stateless request-authenticity signal checked by the
  // server. Cross-origin scripts cannot add it to a credentialed request without CORS preflight.
  var nativeFetch = window.fetch;
  window.fetch = function portalFetch(input, init) {
    var options = init || {};
    var method = String(options.method || (input && input.method) || "GET").toUpperCase();
    if (method === "POST" || method === "PUT" || method === "PATCH" || method === "DELETE") {
      var headers = new Headers(options.headers || (input && input.headers));
      headers.set("x-artifact-mutation", "1");
      options = Object.assign({}, options, { headers: headers });
    }
    return nativeFetch.call(window, input, options);
  };

  var notifToggle = document.getElementById("notif-toggle");
  var notifPanel = document.getElementById("notif-panel");
  var notifSeen = document.getElementById("notif-seen");
  var notifCount = document.querySelector(".notif-count");
  var notifMarked = false;

  function markNotificationsSeen() {
    if (notifMarked) return;
    notifMarked = true;
    if (notifCount) {
      notifCount.hidden = true;
      notifCount.textContent = "0";
    }
    document.querySelectorAll(".notif-row.unread").forEach(function (row) {
      row.classList.remove("unread");
    });
    fetch("/notifications/seen", { method: "POST" }).catch(function () {
      notifMarked = false;
    });
  }

  function openNotifications(open) {
    if (!notifPanel || !notifToggle) return;
    notifPanel.hidden = !open;
    notifToggle.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) markNotificationsSeen();
  }

  if (notifToggle) {
    notifToggle.addEventListener("click", function (event) {
      event.stopPropagation();
      openNotifications(notifPanel.hidden);
    });
  }
  if (notifSeen) notifSeen.addEventListener("click", markNotificationsSeen);

  var theme = document.getElementById("theme");
  if (theme) {
    theme.addEventListener("click", function () {
      var current = document.documentElement.dataset.theme;
      var dark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
      var next = current === "dark" ? "light" : current === "light" ? "dark" : dark ? "light" : "dark";
      document.documentElement.dataset.theme = next;
      try {
        localStorage.setItem("artifact-theme", next);
      } catch (_error) {
        // Theme persistence is optional.
      }
    });
  }

  var cards = Array.prototype.slice.call(document.querySelectorAll(".card"));
  cards.forEach(function (card, index) {
    card.style.animationDelay = (index % 12 * 35) + "ms";
    var image = card.querySelector(".pv");
    if (!image) return;
    var reveal = function () {
      card.classList.add("preview-ready");
    };
    if (image.complete && image.naturalWidth > 0) reveal();
    else {
      image.addEventListener("load", reveal, { once: true });
      image.addEventListener("error", reveal, { once: true });
    }
  });

  var search = document.getElementById("q");
  var count = document.getElementById("count");
  var empty = document.getElementById("empty");
  var grid = document.getElementById("artifact-grid");
  var sort = document.getElementById("sort");
  var sortLabel = document.getElementById("sort-label");
  var activeView = "all";
  var activeOrg = "all";
  var activeCategory = "all";

  function visibleCards() {
    return cards.filter(function (card) { return card.isConnected; });
  }

  function viewCount(card) {
    var badge = card.querySelector(".view-badge");
    return badge ? Number(String(badge.textContent || "").replace(/[^0-9]/g, "")) || 0 : 0;
  }

  function applySort() {
    if (!grid || !sort) return;
    var mode = sort.value;
    visibleCards().sort(function (left, right) {
      if (mode === "title") return (left.querySelector(".card-title").textContent || "").localeCompare(right.querySelector(".card-title").textContent || "");
      if (mode === "views") return viewCount(right) - viewCount(left);
      return String(right.dataset.updated || "").localeCompare(String(left.dataset.updated || ""));
    }).forEach(function (card) { grid.appendChild(card); });
    if (sortLabel && sort.selectedOptions[0]) sortLabel.textContent = sort.selectedOptions[0].textContent;
  }

  function saveLibraryState() {
    var state = {
      q: search && search.value || "",
      view: activeView,
      org: activeOrg,
      category: activeCategory,
      sort: sort && sort.value || "recent",
      scrollY: Math.max(0, window.scrollY || window.pageYOffset || 0)
    };
    try { sessionStorage.setItem("artifact-library-state", JSON.stringify(state)); } catch (_error) {}
    try {
      var url = new URL(location.href);
      ["q", "view", "org", "category", "sort"].forEach(function (key) {
        if (state[key] && state[key] !== "all" && state[key] !== "recent") url.searchParams.set(key, state[key]);
        else url.searchParams.delete(key);
      });
      history.replaceState(null, "", url.pathname + url.search + url.hash);
    } catch (_error) {}
  }

  function pressOnly(selector, active) {
    document.querySelectorAll(selector).forEach(function (button) {
      button.setAttribute("aria-pressed", button === active ? "true" : "false");
    });
  }

  function applyFilters() {
    var term = (search && search.value || "").trim().toLowerCase();
    var shown = 0;
    applySort();
    visibleCards().forEach(function (card) {
      if (!card.isConnected) return;
      card.classList.add("settled");
      var viewMatch =
        activeView === "all" ||
        (activeView === "favorites" && card.dataset.fav === "1") ||
        (activeView === "review" && card.dataset.needsReview === "1") ||
        (activeView === "hidden" && card.dataset.hidden === "1");
      var orgMatch = activeOrg === "all" || card.dataset.org === activeOrg;
      var categoryMatch = activeCategory === "all" || card.dataset.category === activeCategory;
      var termMatch = !term || card.dataset.q.indexOf(term) !== -1;
      var visible = viewMatch && orgMatch && categoryMatch && termMatch;
      card.hidden = !visible;
      if (visible) shown += 1;
    });
    if (empty) empty.hidden = shown !== 0;
    if (count) count.textContent = "Showing " + shown + " of " + visibleCards().length;
    saveLibraryState();
  }

  document.addEventListener("click", function (event) {
    var viewFilter = event.target.closest("[data-filter-view]");
    if (viewFilter) {
      activeView = viewFilter.dataset.filterView;
      pressOnly("[data-filter-view]", viewFilter);
      applyFilters();
      return;
    }
    var orgFilter = event.target.closest("[data-filter-org]");
    if (orgFilter) {
      activeOrg = orgFilter.dataset.filterOrg;
      pressOnly("[data-filter-org]", orgFilter);
      applyFilters();
      return;
    }
    var categoryFilter = event.target.closest("[data-filter-category]");
    if (categoryFilter) {
      activeCategory = categoryFilter.dataset.filterCategory;
      pressOnly("[data-filter-category]", categoryFilter);
      applyFilters();
      return;
    }
    var resetFilters = event.target.closest("[data-reset-filters]");
    if (resetFilters) {
      activeView = "all";
      activeOrg = "all";
      activeCategory = "all";
      if (search) search.value = "";
      if (sort) sort.value = "recent";
      ["[data-filter-view]", "[data-filter-org]", "[data-filter-category]"].forEach(function (selector) {
        var all = document.querySelector(selector.slice(0, -1) + '="all"]');
        if (all) pressOnly(selector, all);
      });
      applyFilters();
    }
  });

  if (search) search.addEventListener("input", applyFilters);
  if (sort) sort.addEventListener("change", applyFilters);

  // The viewer's Back link is deliberately a clean `href="/"`. Preserve a return snapshot
  // only when the viewer was opened from this library, rather than writing on every scroll.
  document.addEventListener("click", function (event) {
    var artifactLink = event.target.closest(".card a[href]");
    if (!artifactLink || artifactLink.hasAttribute("download") || /\/raw\//.test(artifactLink.pathname || "")) return;
    saveLibraryState();
    try { sessionStorage.setItem("artifact-library-return", "1"); } catch (_error) {}
  }, true);
  window.addEventListener("pagehide", function () { saveLibraryState(); });
  // A browser Back can revive this document from bfcache without rerunning restoreLibraryState.
  // Its DOM already has the prior library state, so discard the one-shot marker immediately.
  window.addEventListener("pageshow", function (event) {
    if (!event.persisted) return;
    try { sessionStorage.removeItem("artifact-library-return"); } catch (_error) {}
  });

  document.querySelectorAll("[data-layout]").forEach(function (button) {
    if (!button.closest(".layout-toggle")) return;
    button.addEventListener("click", function () {
      if (!grid) return;
      grid.dataset.layout = button.dataset.layout;
      pressOnly(".layout-toggle [data-layout]", button);
      try {
        localStorage.setItem("artifact-layout", button.dataset.layout);
      } catch (_error) {
        // Layout persistence is optional.
      }
    });
  });
  if (grid) {
    try {
      var storedLayout = localStorage.getItem("artifact-layout");
      if (storedLayout === "list") {
        var storedButton = document.querySelector('.layout-toggle [data-layout="list"]');
        if (storedButton) storedButton.click();
      }
    } catch (_error) {
      // Layout persistence is optional.
    }
  }

  function restoreLibraryState() {
    var state = null;
    var restoreScroll = false;
    try {
      var url = new URL(location.href);
      if (["q", "view", "org", "category", "sort"].some(function (key) { return url.searchParams.has(key); })) {
        state = Object.fromEntries(url.searchParams.entries());
      } else if (sessionStorage.getItem("artifact-library-return") === "1") {
        state = JSON.parse(sessionStorage.getItem("artifact-library-state") || "null");
      }
      restoreScroll = sessionStorage.getItem("artifact-library-return") === "1";
      sessionStorage.removeItem("artifact-library-return");
    } catch (_error) {}
    if (!state) return null;
    if (search && state.q) search.value = state.q;
    if (sort && state.sort && Array.prototype.some.call(sort.options, function (option) { return option.value === state.sort; })) sort.value = state.sort;
    [["view", "activeView", "[data-filter-view]"], ["org", "activeOrg", "[data-filter-org]"], ["category", "activeCategory", "[data-filter-category]"]].forEach(function (entry) {
      var value = state[entry[0]] || "all";
      var button = Array.prototype.find.call(document.querySelectorAll(entry[2]), function (candidate) {
        return candidate.getAttribute(entry[2].slice(1, -1)) === value;
      });
      if (!button) return;
      if (entry[1] === "activeView") activeView = value;
      if (entry[1] === "activeOrg") activeOrg = value;
      if (entry[1] === "activeCategory") activeCategory = value;
      pressOnly(entry[2], button);
    });
    return restoreScroll && Number.isFinite(Number(state.scrollY)) ? Math.max(0, Number(state.scrollY)) : null;
  }

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") {
      if (notifPanel && !notifPanel.hidden) {
        openNotifications(false);
        notifToggle.focus();
        return;
      }
      var openMenu = document.querySelector(".card-menu:not([hidden])");
      if (openMenu) {
        closeMenu(openMenu, true);
        return;
      }
      if (document.activeElement === search && search.value) {
        search.value = "";
        applyFilters();
      }
    }
    if (!search || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key === "/" && !event.target.closest("input,textarea,select,[contenteditable]")) {
      event.preventDefault();
      search.focus();
    }
  });

  var toastNode = document.getElementById("toast");
  var toastTimer;
  function toast(message) {
    if (!toastNode) return;
    clearTimeout(toastTimer);
    toastNode.textContent = message;
    toastNode.classList.add("show");
    toastTimer = setTimeout(function () {
      toastNode.classList.remove("show");
    }, 2600);
  }

  function jsonRequest(url, options) {
    return fetch(url, options).then(function (response) {
      return response.json().catch(function () { return {}; }).then(function (body) {
        if (!response.ok) throw new Error(body.error || "Request failed");
        return body;
      });
    });
  }

  function closeMenu(menu, restoreFocus) {
    if (!menu) return;
    menu.hidden = true;
    var card = menu.closest(".card");
    if (card) card.classList.remove("menu-open");
    var trigger = card && card.querySelector('[data-action="more"]');
    if (trigger) {
      trigger.setAttribute("aria-expanded", "false");
      if (restoreFocus) trigger.focus();
    }
  }

  function openMenu(trigger) {
    var card = trigger.closest(".card");
    var menu = card.querySelector(".card-menu");
    document.querySelectorAll(".card-menu:not([hidden])").forEach(function (candidate) {
      if (candidate !== menu) closeMenu(candidate, false);
    });
    var open = menu.hidden;
    menu.hidden = !open;
    card.classList.toggle("menu-open", open);
    trigger.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      var first = menu.querySelector("select,button");
      if (first) first.focus();
    }
  }

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
          toast("Link selected — copy it manually");
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
            card.remove();
            applyFilters();
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

  function updateVisibility(card, control) {
    var nextHidden = card.dataset.hidden !== "1";
    control.disabled = true;
    jsonRequest("/" + card.dataset.id + "/visibility", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hidden: nextHidden }),
    }).then(function (body) {
      card.dataset.hidden = body.hidden ? "1" : "0";
      card.classList.toggle("is-hidden", !!body.hidden);
      control.setAttribute("aria-label", (body.hidden ? "Show " : "Hide ") + card.querySelector(".card-title").textContent.trim() + " in the gallery");
      control.title = body.hidden ? "Show in gallery" : "Hide from gallery";
      control.innerHTML = body.hidden
        ? '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="m3 3 18 18"></path><path d="M10.6 6.2A10.5 10.5 0 0 1 12 6c6 0 9.5 6 9.5 6a17.7 17.7 0 0 1-3.1 3.8M6.1 6.1C3.8 7.7 2.5 10 2.5 12c0 0 3.5 6 9.5 6 1.4 0 2.7-.3 3.8-.8"></path><path d="M9.9 9.9a3 3 0 0 0 4.2 4.2"></path></svg>'
        : '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"></path><circle cx="12" cy="12" r="2.5"></circle></svg>';
      toast(body.hidden ? "Artifact hidden from Gallery" : "Artifact shown in Gallery");
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
      card.dataset.fav = favorite ? "1" : "0";
      control.classList.toggle("active", favorite);
      control.setAttribute("aria-pressed", favorite ? "true" : "false");
      var label = control.querySelector("span");
      if (label) label.textContent = favorite ? "Saved" : "Save";
      applyFilters();
      toast(favorite ? "Saved to favorites" : "Removed from favorites");
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

  function requestMove(card, org) {
    return jsonRequest("/" + card.dataset.id + "/move", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ org: org }),
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
    if (!event.target.closest(".card-menu") && !event.target.closest('[data-action="more"]')) {
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
      card.querySelector(".org-menu").focus();
      return;
    }
    var moveYes = event.target.closest(".move-yes");
    if (moveYes) {
      var destination = moveYes.closest(".move-confirm").dataset.destination;
      moveYes.disabled = true;
      requestMove(card, destination).catch(function (error) {
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
      category.disabled = true;
      requestCategory(card, nextCategory).catch(function (error) {
        category.disabled = false;
        toast(error.message || "Could not change category");
      });
      return;
    }
    var org = event.target.closest('[data-action="move-org"]');
    if (org && org.value) {
      var confirm = card.querySelector(".move-confirm");
      confirm.dataset.destination = org.value;
      confirm.querySelector(".move-question").textContent =
        "Move from " + card.dataset.org + " to " + org.value + "? Active public share links will be revoked.";
      confirm.hidden = false;
      confirm.querySelector(".move-no").focus();
    }
  });

  try {
    var currentUrl = new URL(location.href);
    if (currentUrl.searchParams.get("deleted") === "1") {
      toast("Artifact deleted");
      currentUrl.searchParams.delete("deleted");
      history.replaceState(null, "", currentUrl.pathname + currentUrl.search + currentUrl.hash);
    }
  } catch (_error) {
    // The success message is optional when URL APIs are unavailable.
  }

  var returnScrollY = restoreLibraryState();
  applyFilters();
  if (returnScrollY !== null) requestAnimationFrame(function () { window.scrollTo(0, returnScrollY); });
}());
