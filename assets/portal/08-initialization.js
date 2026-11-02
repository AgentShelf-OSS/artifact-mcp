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

  // Collection views render the same artifact actions in a different arrangement. Keep
  // their integration on this trusted portal surface so they reuse the gallery's existing
  // request handlers, URL/history behavior, and feedback instead of copying mutations.
  window.ArtifactPortal = Object.freeze({
    getLibraryState: function () {
      return {
        q: search && search.value || "",
        view: activeView,
        org: activeOrg,
        category: activeCategory,
        sort: sort && sort.value || "recent",
        status: activeView
      };
    },
    applyFilters: applyFilters,
    revealCardPreview: revealCardPreview,
    saveLibraryState: saveLibraryState,
    getPerformanceStats: function () { return Object.assign({}, performanceStats); },
    clearFilters: function () {
      activeView = "all"; activeOrg = "all"; activeCategory = "all";
      if (search) search.value = "";
      if (sort) sort.value = "recent";
      if (orgFilter) orgFilter.value = "all";
      rebuildCategoryFilter("all");
      var all = document.querySelector('[data-filter-view="all"]');
      if (all) pressOnly("[data-filter-view]", all);
      applyFilters();
    },
    setCollectionScope: function (predicate) { collectionScope = typeof predicate === "function" ? predicate : null; applyFilters(); },
    setArtifactLayout: function (layout) {
      var button = document.querySelector('.layout-toggle [data-layout="' + (layout === "list" ? "list" : "grid") + '"]');
      if (!grid || !button) return;
      grid.dataset.layout = button.dataset.layout;
      pressOnly(".layout-toggle [data-layout]", button);
      try { localStorage.setItem("artifact-layout", button.dataset.layout); } catch (_error) {}
    },
    clearCollectionScope: function () { collectionScope = null; applyFilters(); },
    toast: toast,
    openActions: function (trigger) {
      if (trigger && trigger.matches && trigger.matches('[data-action="more"]')) openMenu(trigger);
    },
    triggerAction: function (card, action) {
      if (!card || !action) return false;
      var target = card.querySelector('[data-action="' + CSS.escape(action) + '"]');
      if (!target) return false;
      target.click();
      return true;
    }
  });
}());
