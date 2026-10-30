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
    var viewValue = state.view || "all";
    var viewButton = document.querySelector('[data-filter-view="' + CSS.escape(viewValue) + '"]');
    if (viewButton) {
      activeView = viewValue;
      pressOnly("[data-filter-view]", viewButton);
    }
    if (orgFilter && state.org && Array.prototype.some.call(orgFilter.options, function (option) { return option.value === state.org; })) {
      activeOrg = state.org;
      orgFilter.value = state.org;
    }
    rebuildCategoryFilter(state.category || "all");
    if (categoryFilter && state.category && Array.prototype.some.call(categoryFilter.options, function (option) { return option.value === state.category; })) {
      activeCategory = state.category;
      categoryFilter.value = state.category;
    }
    return restoreScroll && Number.isFinite(Number(state.scrollY)) ? Math.max(0, Number(state.scrollY)) : null;
  }

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") {
      if (categoryDialog && categoryDialog.open) {
        event.preventDefault();
        closeCategoryDialog();
        return;
      }
      // Let the topmost native dialog handle Escape before the underlying menu.
      if (document.querySelector("dialog[open]")) return;
      if (notifPanel && !notifPanel.hidden) {
        event.preventDefault();
        openNotifications(false);
        notifToggle.focus();
        return;
      }
      var openMenu = document.querySelector(".card-menu:not([hidden])");
      if (openMenu) {
        event.preventDefault();
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

