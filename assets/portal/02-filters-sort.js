  var search = document.getElementById("q");
  var count = document.getElementById("count");
  var empty = document.getElementById("empty");
  var grid = document.getElementById("artifact-grid");
  var sort = document.getElementById("sort");
  var orgFilter = document.getElementById("org-filter");
  var categoryFilter = document.getElementById("category-filter");
  var resetFilters = document.querySelector("[data-reset-filters]");
  var sortLabel = document.getElementById("sort-label");
  var categoryDataNode = document.getElementById("org-category-data");
  var createCategoryValue = "__create_category__";
  var canCreateCategories = categoryDataNode && categoryDataNode.dataset.canCreate === "1";
  var orgCategoryIndex = {};
  try {
    orgCategoryIndex = JSON.parse(categoryDataNode && categoryDataNode.dataset.json || "{}");
  } catch (_error) {
    orgCategoryIndex = {};
  }
  var activeView = "all";
  var activeOrg = "all";
  var organizationLabel = document.getElementById("organization-label");
  var defaultOrganizationLabel = organizationLabel?.textContent || "All organizations";
  var activeCategory = "all";
  var collectionScope = null;
  var libraryStateTimer = 0;
  var sortDirty = true;
  var lastSortMode = null;
  var cardSortMeta = new Map();
  var performanceStats = { filterRuns: 0, sortRuns: 0, cardVisibilityWrites: 0, sortReorders: 0, stateWrites: 0 };

  function categoryRows(org) {
    if (org !== "all") return Array.isArray(orgCategoryIndex[org]) ? orgCategoryIndex[org].slice() : [];
    var totals = new Map();
    Object.keys(orgCategoryIndex).forEach(function (name) {
      (orgCategoryIndex[name] || []).forEach(function (row) {
        totals.set(row.name, (totals.get(row.name) || 0) + Number(row.count || 0));
      });
    });
    return Array.from(totals, function (entry) { return { name: entry[0], count: entry[1] }; });
  }

  function sortCategoryRows(rows) {
    return rows.sort(function (left, right) {
      if (!left.name) return 1;
      if (!right.name) return -1;
      return String(left.name).toLowerCase().localeCompare(String(right.name).toLowerCase());
    });
  }

  function appendOption(select, value, label, selected) {
    var option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    option.selected = !!selected;
    select.appendChild(option);
  }

  function rebuildCategoryFilter(preferred) {
    if (!categoryFilter) return;
    var rows = sortCategoryRows(categoryRows(activeOrg));
    categoryFilter.replaceChildren();
    appendOption(categoryFilter, "all", "All categories", preferred === "all");
    rows.forEach(function (row) {
      appendOption(categoryFilter, row.name, (row.name || "Uncategorized") + " (" + Number(row.count || 0) + ")", preferred === row.name);
    });
    if (canCreateCategories) appendOption(categoryFilter, createCategoryValue, "+ Category", false);
    var valid = preferred && preferred !== createCategoryValue && Array.prototype.some.call(categoryFilter.options, function (option) { return option.value === preferred; });
    activeCategory = valid ? preferred : "all";
    categoryFilter.value = activeCategory;
  }

  function visibleCards() {
    return cards.filter(function (card) { return card.isConnected; });
  }

  function sortMeta(card) {
    var meta = cardSortMeta.get(card);
    if (meta) return meta;
    var title = card.querySelector(".card-title");
    var badge = card.querySelector(".view-badge");
    meta = {
      title: title ? title.textContent || "" : "",
      views: badge ? Number(String(badge.textContent || "").replace(/[^0-9]/g, "")) || 0 : 0,
      updated: String(card.dataset.updated || "")
    };
    cardSortMeta.set(card, meta);
    return meta;
  }

  function applySort() {
    if (!grid || !sort) return;
    var mode = sort.value;
    if (!sortDirty && mode === lastSortMode) return;
    performanceStats.sortRuns += 1;
    var ordered = visibleCards().sort(function (left, right) {
      var leftMeta = sortMeta(left);
      var rightMeta = sortMeta(right);
      if (mode === "title") return leftMeta.title.localeCompare(rightMeta.title);
      if (mode === "views") return rightMeta.views - leftMeta.views;
      return rightMeta.updated.localeCompare(leftMeta.updated);
    });
    // Filtering does not usually change order. Avoid detaching every card (and its
    // focused controls) on each filter or collection refresh.
    var existing = Array.prototype.slice.call(grid.children);
    if (ordered.some(function (card, index) { return existing[index] !== card; })) {
      performanceStats.sortReorders += 1;
      var fragment = document.createDocumentFragment();
      ordered.forEach(function (card) { fragment.appendChild(card); });
      grid.appendChild(fragment);
    }
    lastSortMode = mode;
    sortDirty = false;
    if (sortLabel && sort.selectedOptions[0]) sortLabel.textContent = sort.selectedOptions[0].textContent;
  }

  function queueLibraryStateSave() {
    clearTimeout(libraryStateTimer);
    libraryStateTimer = setTimeout(function () {
      libraryStateTimer = 0;
      saveLibraryState();
    }, 150);
  }

  function saveLibraryState() {
    clearTimeout(libraryStateTimer);
    libraryStateTimer = 0;
    performanceStats.stateWrites += 1;
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
    performanceStats.filterRuns += 1;
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
      var termMatch = !term || (card.dataset.q + " " + (card.dataset.collectionNames || "")).indexOf(term) !== -1;
      var scopeMatch = !collectionScope || collectionScope(card.dataset.id);
      var visible = viewMatch && orgMatch && categoryMatch && termMatch && scopeMatch;
      var nextHidden = !visible;
      if (card.hidden !== nextHidden) {
        card.hidden = nextHidden;
        performanceStats.cardVisibilityWrites += 1;
      }
      if (visible) shown += 1;
    });
    if (empty) empty.hidden = shown !== 0;
    if (organizationLabel) organizationLabel.textContent = activeOrg === "all" ? defaultOrganizationLabel : activeOrg;
    if (count) count.textContent = "Showing " + shown + " of " + visibleCards().length;
    if (resetFilters) {
      resetFilters.hidden = !term && activeView === "all" && activeOrg === "all" && activeCategory === "all" && (!sort || sort.value === "recent");
    }
    queueLibraryStateSave();
    document.dispatchEvent(new CustomEvent("collections:filters-changed"));
  }

  document.addEventListener("click", function (event) {
    var viewFilter = event.target.closest("[data-filter-view]");
    if (viewFilter) {
      activeView = viewFilter.dataset.filterView;
      pressOnly("[data-filter-view]", viewFilter);
      applyFilters();
      return;
    }
    var resetButton = event.target.closest("[data-reset-filters]");
    if (resetButton) {
      activeView = "all";
      activeOrg = "all";
      activeCategory = "all";
      if (search) search.value = "";
      if (sort) sort.value = "recent";
      if (orgFilter) orgFilter.value = "all";
      rebuildCategoryFilter("all");
      var all = document.querySelector('[data-filter-view="all"]');
      if (all) pressOnly("[data-filter-view]", all);
      applyFilters();
    }
  });

  if (search) search.addEventListener("input", applyFilters);
  if (sort) sort.addEventListener("change", function () { sortDirty = true; applyFilters(); });
  if (orgFilter) orgFilter.addEventListener("change", function () {
    activeOrg = orgFilter.value;
    rebuildCategoryFilter(activeCategory);
    applyFilters();
  });
  if (categoryFilter) categoryFilter.addEventListener("change", function () {
    if (categoryFilter.value === createCategoryValue) {
      categoryFilter.value = activeCategory;
      openCategoryDialog({ mode: "filter", org: activeOrg, restoreFocus: categoryFilter });
      return;
    }
    activeCategory = categoryFilter.value;
    applyFilters();
  });

