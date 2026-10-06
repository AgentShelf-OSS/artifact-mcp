(function () {
  "use strict";

  var surface = document.getElementById("collection-surface");
  if (!surface) return;
  var config = document.getElementById("collection-config");
  var configData = config && config.dataset ? config.dataset : {};
  var viewerOrg = configData.viewerOrg || "";
  var viewerEmail = configData.viewerEmail || "";
  var isAdmin = configData.viewerAdmin === "1";
  var defaultOrg = isAdmin ? "all" : viewerOrg;
  var state = { collections: [], uncollectedCount: 0, preferences: {}, org: defaultOrg, selected: null };
  var view = "reel";
  var collectionStatus = new URL(location.href).searchParams.get("collectionStatus") || "all";
  var suppressPreviewFocus = false;
  var saveTimer = 0;
  var reelId = null;
  var dismissedPreviewId = null;
  var reelPinned = false;
  var dragId = null;
  var artifactDragging = false;
  var requestSequence = 0;
  var reelPage = 0;
  var reelFace = null;
  var selectedArtifacts = new Set();
  var ribbonPages = {};
  var savedPreferences = null;
  var preferenceVersion = 0;
  var canonicalCardIndex = null;
  var canonicalCardIndexKey = "";
  var membershipNamesByArtifact = new Map();
  var canonicalEnhancementSignatures = new WeakMap();
  var membershipRevision = 0;
  var lastCanonicalEnhancementRevision = -1;
  var lastCanonicalEnhancementCount = -1;
  var collectionPerformance = { renders: 0, canonicalIndexBuilds: 0, canonicalLookups: 0, canonicalEnhancements: 0, canonicalEnhancementWrites: 0, selectionWrites: 0 };

  function escapeHtml(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (char) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[char];
    });
  }
  function json(value) { try { return JSON.parse(value || "{}"); } catch (_error) { return {}; } }
  function icon(name) {
    var paths = {
      folder: '<path d="M3 7V5h6l2 2h10v13H3Z"/>',
      plus: '<path d="M12 5v14M5 12h14"/>',
      grip: '<circle cx="8" cy="5" r="1"/><circle cx="16" cy="5" r="1"/><circle cx="8" cy="12" r="1"/><circle cx="16" cy="12" r="1"/><circle cx="8" cy="19" r="1"/><circle cx="16" cy="19" r="1"/>',
      up: '<path d="m6 14 6-6 6 6"/>', down: '<path d="m6 10 6 6 6-6"/>', close: '<path d="m6 6 12 12M18 6 6 18"/>'
    };
    return '<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">' + (paths[name] || "") + "</svg>";
  }
  function status(message, kind) {
    var node = surface.querySelector(".collection-status");
    if (!node) return;
    if(message && window.ArtifactPortal?.toast)window.ArtifactPortal.toast(message);
    node.textContent = message || "";
    node.dataset.kind = kind || "";
  }
  function showCollectionDialog(dialog, restoreFocus, onClose) {
    dialog.setAttribute("aria-label", dialog.querySelector("h2").textContent);
    dialog.querySelectorAll("[data-dialog-close]").forEach(function (button) {
      button.addEventListener("click", function () { dialog.close("cancel"); });
    });
    dialog.addEventListener("close", function () {
      dialog.remove();
      if (onClose) onClose(dialog.returnValue);
      if (restoreFocus && restoreFocus.isConnected) restoreFocus.focus();
    });
    document.body.appendChild(dialog);
    dialog.showModal();
  }
  function endpoint(path, options) {
    var opts = Object.assign({}, options || {});
    var match = path.match(/^\/collections\/([^/?]+)(?:\/memberships)?(?:\?|$)/);
    if (match && opts.method) {
      var row = state.collections.find(function (item) { return item.id === decodeURIComponent(match[1]); });
      if (row) {
        if (opts.method === "PATCH") { var payload = JSON.parse(opts.body || "{}"); payload.org = row.org; opts.body = JSON.stringify(payload); }
        else { path += (path.indexOf("?") < 0 ? "?" : "&") + "org=" + encodeURIComponent(row.org); }
      }
    }
    var headers = Object.assign({ accept: "application/json", "x-artifact-mutation": "1" }, opts.headers || {});
    if (opts.body && !headers["content-type"]) headers["content-type"] = "application/json";
    return fetch(path, Object.assign({}, opts, { headers: headers })).then(function (response) {
      return response.json().catch(function () { return {}; }).then(function (body) {
        if (!response.ok) throw new Error(body.message || "Collection request failed");
        return body;
      });
    });
  }
  function projectionUrl() {
    var org = state.org || defaultOrg;
    return "/collections?org=" + encodeURIComponent(org || "all") + "&include=projection";
  }
  function normalizeCollection(row) {
    var collection = Object.assign({}, row || {});
    collection.id = String(collection.id || "");
    collection.name = String(collection.name || "Untitled folder");
    collection.org = String(collection.org || viewerOrg);
    collection.artifactIds = Array.isArray(collection.artifactIds) ? collection.artifactIds.map(String) : [];
    collection.previewArtifacts = Array.isArray(collection.previewArtifacts) ? collection.previewArtifacts : [];
    collection.artifactCount = Number(collection.artifactCount == null ? collection.artifactIds.length : collection.artifactCount) || 0;
    collection.color = collection.color || "#e4d3b4";
    return collection;
  }
  function normalizeProjection(body) {
    var rows = Array.isArray(body.collections) ? body.collections : [];
    state.collections = rows.map(normalizeCollection);
    membershipRevision += 1;
    membershipNamesByArtifact = new Map();
    state.collections.forEach(function (collection) {
      collection.artifactIds.forEach(function (artifactId) {
        var names = membershipNamesByArtifact.get(artifactId) || [];
        names.push(collection.name);
        membershipNamesByArtifact.set(artifactId, names);
      });
    });
    state.uncollectedCount = Number(body.uncollectedCount || 0);
    if(!saveTimer) { state.preferences = body.preferences || state.preferences || {}; savedPreferences=JSON.parse(JSON.stringify(state.preferences)); }
    state.org = body.org || state.org || defaultOrg || "all";
    var scope = preferenceOrg();
    ["collectionOrderByOrg", "collapsedCollectionIdsByOrg"].forEach(function (key) {
      var values = state.preferences[key] || {};
      if (values[""] && !values[scope]) values[scope] = values[""];
      delete values[""];
      state.preferences[key] = values;
    });
    view = new URL(location.href).searchParams.get("libraryView") || state.preferences.view || view;
    if (["reel", "sheets", "ribbons", "all"].indexOf(view) < 0) view = "reel";
  }
  function preferenceOrg() {
    return isAdmin ? (state.org || "all") : viewerOrg;
  }
  function artifactById(id) {
    var wanted = String(id);
    var all = [];
    state.collections.forEach(function (collection) { all = all.concat(collection.previewArtifacts || []); });
    return all.find(function (artifact) { return String(artifact.id) === wanted; }) || null;
  }
  function canonicalCards() {
    var grid = document.getElementById("artifact-grid");
    var children = grid ? grid.children : [];
    var library = window.ArtifactPortal && window.ArtifactPortal.getLibraryState ? window.ArtifactPortal.getLibraryState() : {};
    var key = (library.sort || "recent") + ":" + children.length + ":" + (children[0] ? children[0].dataset.id : "") + ":" + (children[children.length - 1] ? children[children.length - 1].dataset.id : "");
    if (canonicalCardIndex && canonicalCardIndexKey === key) return canonicalCardIndex;
    canonicalCardIndexKey = key;
    collectionPerformance.canonicalIndexBuilds += 1;
    var cards = Array.prototype.slice.call(document.querySelectorAll("#artifact-grid .card"));
    canonicalCardIndex = {
      byId: new Map(cards.map(function (card) { return [card.dataset.id, card]; })),
      order: new Map(cards.map(function (card, index) { return [card.dataset.id, index]; }))
    };
    return canonicalCardIndex;
  }
  function collectionArtifacts(collection) {
    var previews = collection.previewArtifacts || [];
    var byId = new Map(previews.map(function (artifact) { return [String(artifact.id), artifact]; }));
    var index = canonicalCards();
    return (collection.artifactIds || []).filter(function (id) {
      collectionPerformance.canonicalLookups += 1;
      var visible = index.byId.get(String(id));
      return !visible || !visible.hidden;
    }).sort(function (a, b) {
      return (index.order.get(a) || 0) - (index.order.get(b) || 0);
    }).map(function (id) {
      var known = byId.get(String(id));
      collectionPerformance.canonicalLookups += 1;
      var canonical = index.byId.get(String(id));
      if (known) {
        if (canonical) return Object.assign({}, known, { title: (canonical.querySelector(".card-title") || {}).textContent || known.title, org: canonical.dataset.org || known.org, category: canonical.dataset.category || known.category, description: (canonical.querySelector(".desc") || {}).textContent || known.description, favorite: canonical.dataset.fav === "1", hidden: canonical.dataset.hidden === "1" });
        return known;
      }
      if (canonical) return { id: id, title: (canonical.querySelector(".card-title") || {}).textContent || "Artifact", org: canonical.dataset.org, category: canonical.dataset.category, description: (canonical.querySelector(".desc") || {}).textContent || "", thumbnailSrc: (canonical.querySelector(".pv") || {}).src || "", favorite: canonical.dataset.fav === "1", hidden: canonical.dataset.hidden === "1", showDelete: !!canonical.querySelector('[data-action="delete"]') };
      var knownArtifact = artifactById(id);
      return knownArtifact || { id: id, title: "Artifact " + id };
    });
  }
  function previewImage(artifact) {
    var src = artifact && (artifact.thumbnailSrc || artifact.thumbnail_src || artifact.thumbnail || artifact.preview);
    return src ? '<img src="' + escapeHtml(src) + '" alt="" loading="lazy" decoding="async">' : '<span class="collection-peek-empty">Preview unavailable</span>';
  }
  function artifactData(artifact) {
    var data = Object.assign({}, artifact || {});
    data.id = String(data.id || "");
    data.org = data.org || viewerOrg;
    data.category = data.category || "";
    data.title = data.title || "Artifact";
    data.categoryOptions = Array.isArray(data.categoryOptions) ? data.categoryOptions : [];
    data.orgOptions = Array.isArray(data.orgOptions) ? data.orgOptions : [];
    return data;
  }
  function folderFieldMarkup(artifactId) {
    var names = membershipNamesByArtifact.get(String(artifactId)) || [];
    var summary = names.length ? names.join(", ") : "Choose folders";
    return '<div class="collection-folder-field"><span>Folders</span><button type="button" class="collection-folder-select" data-collection-folder-picker="' + escapeHtml(artifactId) + '" aria-label="Choose folders" aria-expanded="false"><span>' + escapeHtml(summary) + '</span><span aria-hidden="true">⌄</span></button><div class="collection-folder-panel" data-collection-folder-panel hidden></div></div>';
  }
  function folderPopoverContext(trigger) {
    var card = trigger.closest(".card");
    var ribbon = card.closest("[data-ribbon-id]");
    return {
      artifactId: card.dataset.id,
      menu: trigger.closest(".card-menu"),
      selector: (card.closest(".collection-reel") ? ".collection-reel " : ribbon ? '[data-ribbon-id="' + CSS.escape(ribbon.dataset.ribbonId) + '"] ' : "#artifact-grid ") + '.card[data-id="' + CSS.escape(card.dataset.id) + '"]'
    };
  }
  function resizeArtifactMenu(menu) {
    document.dispatchEvent(new CustomEvent("artifact-menu:resize", { detail: { menu: menu } }));
  }
  function restoreFolderPopover(context, folderId) {
    if (context.menu.hidden) return;
    var card = document.querySelector(context.selector);
    if (!card) return;
    var menu = card.querySelector(".card-menu");
    if (!menu.hidden && typeof menu.showPopover === "function" && !menu.matches(":popover-open")) menu.hidden = true;
    if (menu.hidden) card.querySelector('[data-action="more"]').click();
    var trigger = menu.querySelector("[data-collection-folder-picker]");
    var panel = menu.querySelector("[data-collection-folder-panel]");
    trigger.setAttribute("aria-expanded", "false");
    openFolderPopover(trigger);
    var focus = folderId ? panel.querySelector('[data-collection-folder-option="' + CSS.escape(folderId) + '"]') : trigger;
    if (focus) focus.focus({ preventScroll: true });
    resizeArtifactMenu(menu);
  }
  function openFolderPopover(trigger) {
    var field = trigger.closest(".collection-folder-field");
    var panel = field.querySelector("[data-collection-folder-panel]");
    var expanded = trigger.getAttribute("aria-expanded") === "true";
    trigger.setAttribute("aria-expanded", String(!expanded));
    panel.hidden = expanded;
    if (expanded) { resizeArtifactMenu(trigger.closest(".card-menu")); return; }
    var context = folderPopoverContext(trigger);
    var artifactOrg = trigger.closest(".card").dataset.org;
    var folders = state.collections.filter(function (row) { return row.org === artifactOrg; });
    panel.innerHTML = '<div class="picker-list">' + folders.map(function (row) {
      return '<label class="picker-row"><input type="checkbox" data-collection-folder-option="' + escapeHtml(row.id) + '"' + (row.artifactIds.indexOf(context.artifactId) >= 0 ? ' checked' : '') + (row.editable ? '' : ' disabled') + '><span>' + escapeHtml(row.name) + '</span></label>';
    }).join('') + (folders.length ? '' : '<p class="collection-folder-empty">No folders in this organization yet.</p>') + '</div><button type="button" data-picker-new>+ Create new folder</button><p class="category-error" role="status" aria-live="polite"></p>';
    panel.querySelectorAll("[data-collection-folder-option]").forEach(function (input) {
      input.addEventListener("change", function () {
        var checked = input.checked;
        input.disabled = true;
        panel.querySelector(".category-error").textContent = "";
        endpoint("/collections/" + encodeURIComponent(input.dataset.collectionFolderOption) + "/memberships", { method: checked ? "POST" : "DELETE", body: JSON.stringify({ artifactIds: [context.artifactId] }) }).then(function () {
          return refresh();
        }).then(function () { restoreFolderPopover(context, input.dataset.collectionFolderOption); }).catch(function (error) {
          input.checked = !checked;
          input.disabled = false;
          panel.querySelector(".category-error").textContent = error.message;
        });
      });
    });
    panel.querySelector("[data-picker-new]").addEventListener("click", function (event) {
      createCollection([context.artifactId], event.currentTarget, function () { restoreFolderPopover(context); });
    });
    resizeArtifactMenu(context.menu);
  }
  function cardMarkup(raw, compact) {
    var a = artifactData(raw);
    var canonical = canonicalCards().byId.get(a.id);
    if (canonical) {
      var clone = canonical.cloneNode(true);
      clone.hidden = false;
      clone.classList.add("collection-artifact-card");
      clone.removeAttribute("style");
      clone.querySelectorAll("[id]").forEach(function (node) { node.removeAttribute("id"); });
      var canonicalMenu = clone.querySelector(".card-menu");
      if (canonicalMenu && !canonicalMenu.querySelector("[data-collection-folder-picker]")) {
        var folders = document.createElement("div"); folders.innerHTML = folderFieldMarkup(a.id); canonicalMenu.insertBefore(folders.firstElementChild, canonicalMenu.querySelector('[data-action="delete"]') || canonicalMenu.querySelector(".move-confirm"));
      }
      if (canonicalMenu) {
        canonicalMenu.hidden = true;
        clone.classList.remove("menu-open");
        clone.querySelector('[data-action="more"]').setAttribute("aria-expanded", "false");
        canonicalMenu.querySelector(".collection-folder-field").outerHTML = folderFieldMarkup(a.id);
      }
      if (!clone.querySelector("[data-collection-select]")) {
        var select = document.createElement("input"); select.type = "checkbox"; select.dataset.collectionSelect = a.id; select.className = "collection-artifact-select"; select.setAttribute("aria-label", "Select " + a.title); var preview = clone.querySelector(".preview"); if (preview) preview.prepend(select);
      }
      clone.draggable = true; clone.dataset.collectionArtifact = a.id;
      return clone.outerHTML;
    }
    var categoryOptions = a.categoryOptions.map(function (option) {
      var value = typeof option === "string" ? option : option.value;
      var label = typeof option === "string" ? option : option.label || option.value;
      return '<option value="' + escapeHtml(value) + '"' + (value === a.category ? " selected" : "") + '>' + escapeHtml(label) + '</option>';
    }).join("");
    var orgOptions = a.orgOptions.map(function (option) {
      var value = typeof option === "string" ? option : option.value;
      var label = typeof option === "string" ? option : option.label || option.value;
      return '<option value="' + escapeHtml(value) + '">' + escapeHtml(label) + '</option>';
    }).join("");
    var src = a.thumbnailSrc || a.thumbnail_src || a.thumbnail || "";
    var actions = '<div class="actions collection-actions">' +
      '<a class="act open" href="/' + encodeURIComponent(a.id) + '">Open</a>' +
      '<button class="act save" data-action="favorite" type="button" aria-label="Save ' + escapeHtml(a.title) + '">Save</button>' +
      '<button class="act share" data-action="share" type="button" aria-label="Share ' + escapeHtml(a.title) + '">Share</button>' +
      '<button class="act icon-act more" data-action="more" type="button" aria-label="More actions for ' + escapeHtml(a.title) + '" aria-expanded="false">…</button>' +
      '<div class="card-menu" data-ui="card-menu" hidden>' +
      folderFieldMarkup(a.id) +
      '<label>Category<select class="category-menu" data-action="category" data-can-create="0" aria-label="Change category for ' + escapeHtml(a.title) + '"><option value="">Uncategorized</option>' + categoryOptions + '</select></label>' +
      (isAdmin ? '<label>Organization<select class="org-menu" data-action="move-org"><option value="">' + escapeHtml(a.org) + '</option>' + orgOptions + '</select></label>' : "") +
      (a.showDelete ? '<button class="menu-action del" data-action="delete" type="button">Delete artifact</button>' : "") +
      '</div></div>';
    return '<article class="card collection-artifact-card" draggable="true" data-collection-artifact="' + escapeHtml(a.id) + '" data-id="' + escapeHtml(a.id) + '" data-org="' + escapeHtml(a.org) + '" data-category="' + escapeHtml(a.category) + '" data-fav="' + (a.favorite ? "1" : "0") + '" data-hidden="' + (a.hidden ? "1" : "0") + '" data-needs-review="' + (a.needsReview ? "1" : "0") + '" data-q="' + escapeHtml([a.title, a.org, a.category, a.description].join(" ").toLowerCase()) + '">' +
      '<div class="collection-artifact-preview"><input type="checkbox" class="collection-artifact-select" data-collection-select="' + escapeHtml(a.id) + '" aria-label="Select ' + escapeHtml(a.title) + '">' + (src ? '<img src="' + escapeHtml(src) + '" alt="" loading="lazy" decoding="async">' : '<span class="collection-peek-empty">Preview unavailable</span>') + '<a href="/' + encodeURIComponent(a.id) + '" aria-label="Open ' + escapeHtml(a.title) + '"></a></div>' +
      '<div class="collection-artifact-copy"><strong class="collection-artifact-title card-title">' + escapeHtml(a.title) + '</strong><small class="collection-artifact-sub">' + escapeHtml(a.org) + (a.category ? " · " + escapeHtml(a.category) : "") + '</small></div>' + actions + '</article>';
  }
  function previewRows(collection, limit) {
    var rows = collectionArtifacts(collection);
    if (collection.coverArtifactId) rows.sort(function(a,b){return Number(b.id === collection.coverArtifactId) - Number(a.id === collection.coverArtifactId);});
    return rows.slice(0, limit || 3).map(artifactData);
  }
  function faceMarkup(collection) {
    var previews = previewRows(collection, 3).map(function (artifact) { return '<span class="collection-peek">' + previewImage(artifact) + '</span>'; }).join("");
    return '<article class="collection-face" data-collection-id="' + escapeHtml(collection.id) + '" style="--collection-color:' + escapeHtml(collection.color) + '">' +
      '<div class="collection-face-preview">' + previews + '</div>' +
      '<div class="collection-face-head"><button type="button" data-collection-peek="' + escapeHtml(collection.id) + '" aria-expanded="' + (reelId === collection.id) + '" aria-label="Preview ' + escapeHtml(collection.name) + '"><strong class="collection-face-title">' + escapeHtml(collection.name) + '</strong><span class="collection-face-meta"><span>' + collectionArtifacts(collection).length + ' ' + (collection.artifactCount === 1 ? "artifact" : "artifacts") + '</span><span aria-hidden="true">·</span><span class="org">' + escapeHtml(collection.org) + '</span></span></button></div></article>';
  }
  function reelPageSize() { return matchMedia("(max-width:760px)").matches ? 1 : 3; }
  function renderReel() {
    var rows = state.collections;
    var current = rows.find(function (row) { return row.id === reelId; });
    var currentArtifacts = current ? collectionArtifacts(current) : [];
    var pageCount = Math.ceil(currentArtifacts.length / reelPageSize());
    if (current && reelPage > Math.max(0, pageCount - 1)) reelPage = 0;
    var reelItems = currentArtifacts.slice(reelPage * reelPageSize(), (reelPage + 1) * reelPageSize());
    var createTile = '<button type="button" class="collection-new-tile" data-collection-create>' + icon("plus") + '<strong>New folder</strong><small>A home for another idea</small></button>';
    var shelf = '<header class="collection-shelf-heading"><h2>Your folders</h2><div class="collection-shelf-tools"><p>Hover to peek. Click to pin. Drop to collect.</p><button type="button" class="shelf-arrow" data-shelf-scroll="-1" aria-label="Previous folders">‹</button><button type="button" class="shelf-arrow" data-shelf-scroll="1" aria-label="Next folders">›</button></div></header>' +
      '<div class="collection-shelf-nav"><div class="collection-grid">' + rows.map(faceMarkup).join("") + createTile + '</div></div>';
    if (!current) return shelf + '<section class="collection-reel" hidden></section>';
    var empty = '<div class="collection-empty">' + (current.artifactCount ? 'No artifacts match the current filters.' : 'This folder is ready for its first artifact.') + '</div>';
    var paging = '<div class="collection-reel-paging"><button class="pager" type="button" data-reel-prev aria-label="Previous reel page"' + (reelPage ? '' : ' disabled') + '>‹</button><small>' + (currentArtifacts.length ? (reelPage + 1) + ' / ' + pageCount : '0 / 0') + '</small><button class="pager" type="button" data-reel-next aria-label="Next reel page"' + (reelPage + 1 < pageCount ? '' : ' disabled') + '>›</button></div>';
    var tools = '<button type="button" data-collection-open="' + escapeHtml(current.id) + '">Open folder ↗</button>' +
      '<button type="button" data-reel-pin aria-label="' + (reelPinned ? 'Unpin' : 'Pin') + ' preview reel">' + (reelPinned ? 'Unpin' : 'Pin open') + '</button>' + paging +
      (current.editable ? '<button type="button" data-collection-edit="' + escapeHtml(current.id) + '" aria-label="Edit folder">Edit</button>' : '') +
      '<button class="pager" type="button" data-reel-close aria-label="Close preview reel">' + icon('close') + '</button>';
    return shelf + '<section class="collection-reel" aria-label="Preview reel for ' + escapeHtml(current.name) + '"><header class="collection-reel-head"><strong class="collection-reel-title"><span>' + (reelPinned ? 'Pinned open' : 'Peeking at') + ' / </span>' + escapeHtml(current.name) + '</strong><div class="collection-reel-tools">' + tools + '</div></header><div class="collection-reel-track">' + reelItems.map(function (artifact) { return '<div class="collection-reel-card">' + cardMarkup(artifact, true) + '</div>'; }).join('') + (currentArtifacts.length ? '' : empty) + '</div></section>';
  }
  function renderSheets() {
    return '<div class="collection-sheet">' + state.collections.map(function (collection) {
      var allRows = collectionArtifacts(collection).map(artifactData); var rows = previewRows(collection, 4); var count = allRows.length === 1 ? "1" : allRows.length === 2 ? "2" : allRows.length === 3 ? "3" : "more";
      return '<article class="collection-sheet-card" data-collection-id="' + escapeHtml(collection.id) + '" style="--collection-color:' + escapeHtml(collection.color) + '"><div class="collection-sheet-cover" data-count="' + count + '">' + rows.map(function (a) { return '<span class="sheet-shot">' + previewImage(a) + '</span>'; }).join("") + (rows.length ? "" : '<span class="collection-peek-empty">'+(collection.artifactCount?"No matching artifacts":"Empty folder")+'</span>') + '</div><div class="collection-sheet-body"><h3>' + escapeHtml(collection.name) + '</h3><p>' + collectionArtifacts(collection).length + ' artifacts · ' + escapeHtml(collection.org) + '</p><div class="collection-face-actions"><button type="button" data-collection-open="' + escapeHtml(collection.id) + '">Open folder</button>' + (collection.editable ? '<button type="button" data-collection-edit="'+escapeHtml(collection.id)+'" aria-label="Edit '+escapeHtml(collection.name)+'">Edit</button>' : '') + '</div></div></article>';
    }).join("") + '</div>';
  }
  function orderedCollections() {
    var order = (state.preferences.collectionOrderByOrg || {})[preferenceOrg()] || [];
    return order.map(function (id) { return state.collections.find(function (row) { return row.id === id; }); }).filter(Boolean).concat(state.collections.filter(function (row) { return order.indexOf(row.id) < 0; }));
  }
  function renderRibbons() {
    var collapsed = ((state.preferences.collapsedCollectionIdsByOrg || {})[preferenceOrg()] || []).map(String);
    return '<div class="collection-ribbons">' + orderedCollections().map(function (collection, index) {
      var isCollapsed = collapsed.indexOf(collection.id) >= 0; var rows = collectionArtifacts(collection).map(artifactData);
      var pageSize=reelPageSize(),page=Math.min(ribbonPages[collection.id]||0,Math.max(0,Math.ceil(rows.length/pageSize)-1));ribbonPages[collection.id]=page;
      var pageRows=rows.slice(page*pageSize,(page+1)*pageSize);
      var body = isCollapsed ? '<div class="collection-ribbon-fan" aria-label="Preview fan">' + rows.slice(0, 3).map(function (a, i) { return '<span style="--i:' + i + '">' + previewImage(a) + '</span>'; }).join("") + '</div>' : '<div class="collection-ribbon-cards">' + pageRows.map(function (a) { return '<div class="collection-ribbon-card">' + cardMarkup(a, true) + '</div>'; }).join("") + (rows.length ? "" : '<div class="collection-empty">'+(collection.artifactCount?"No matching artifacts":"Empty folder")+'</div>') + '</div>';
      return '<article class="collection-ribbon ' + (isCollapsed ? "is-collapsed" : "") + '" draggable="true" data-collection-id="' + escapeHtml(collection.id) + '" data-ribbon-id="' + escapeHtml(collection.id) + '" style="--collection-color:' + escapeHtml(collection.color) + '"><header class="collection-ribbon-head"><button class="collection-ribbon-grip" type="button" draggable="true" data-ribbon-drag aria-label="Reorder ' + escapeHtml(collection.name) + '">' + icon("grip") + '</button><div class="collection-ribbon-title"><button type="button" data-collection-open="' + escapeHtml(collection.id) + '">' + escapeHtml(collection.name) + '</button><small>' + collectionArtifacts(collection).length + ' artifacts · ' + escapeHtml(collection.org) + '</small></div><div class="collection-ribbon-tools">' + (!isCollapsed ? '<button type="button" data-ribbon-page="-1" aria-label="Previous artifacts in '+escapeHtml(collection.name)+'" '+(page===0?'disabled':'')+'>‹</button><button type="button" data-ribbon-page="1" aria-label="Next artifacts in '+escapeHtml(collection.name)+'" '+((page+1)*pageSize>=rows.length?'disabled':'')+'>›</button>':'') + (collection.editable ? '<button type="button" data-collection-edit="'+escapeHtml(collection.id)+'" aria-label="Edit '+escapeHtml(collection.name)+'">Edit</button>' : '') + '<button type="button" data-ribbon-move="up" aria-label="Move ' + escapeHtml(collection.name) + ' up">' + icon("up") + '</button><button type="button" data-ribbon-move="down" aria-label="Move ' + escapeHtml(collection.name) + ' down">' + icon("down") + '</button><button type="button" data-ribbon-collapse aria-expanded="' + (!isCollapsed) + '">' + (isCollapsed ? "Expand" : "Collapse") + '</button></div></header><div class="collection-ribbon-body">' + body + '</div></article>';
    }).join("") + '</div>';
  }
  function render() {
    collectionPerformance.renders += 1;
    var shelf=surface.querySelector(".collection-grid"),shelfScroll=shelf?shelf.scrollLeft:0;
    var active=document.activeElement,focusSelector=null;
    if(surface.contains(active)){
      var row=active.closest("[data-ribbon-id]");
      if(row){var attr=["data-ribbon-collapse","data-ribbon-move","data-ribbon-drag","data-ribbon-page"].find(function(name){return active.hasAttribute(name);});if(attr)focusSelector='[data-ribbon-id="'+CSS.escape(row.dataset.ribbonId)+'"] ['+attr+(active.getAttribute(attr)?'="'+CSS.escape(active.getAttribute(attr))+'"':'')+']';}
      else {var name=["data-collection-peek","data-reel-pin","data-reel-close","data-reel-prev","data-reel-next"].find(function(attr){return active.hasAttribute(attr);});if(name)focusSelector='['+name+(active.getAttribute(name)?'="'+CSS.escape(active.getAttribute(name))+'"':'')+']';}
    }
    var tabs = '<div class="collection-viewbar"><div class="collection-viewtabs" role="tablist" aria-label="Collection view"><button type="button" data-collection-view="reel" aria-pressed="' + (view === "reel") + '">Reel shelf</button><button type="button" data-collection-view="sheets" aria-pressed="' + (view === "sheets") + '">Contact sheets</button><button type="button" data-collection-view="ribbons" aria-pressed="' + (view === "ribbons") + '">Gallery ribbons</button></div><div class="collection-viewmeta"><span><strong>' + state.collections.length + '</strong> folders</span><span><strong>' + state.uncollectedCount + '</strong> uncollected</span><button class="collection-new" type="button" data-collection-create>' + icon("plus") + ' New folder</button></div></div><p class="collection-status" role="status" aria-live="polite"></p>';
    var selected=state.collections.find(function(row){return row.id===state.selected;});
    var body=selected?'<div class="collection-folder-heading"><button type="button" data-collection-back>← Collections</button><div><h2>'+escapeHtml(selected.name)+'</h2><p>'+escapeHtml(selected.org)+' · '+collectionArtifacts(selected).length+' matching artifacts</p></div>'+(selected.editable?'<button type="button" data-collection-edit="'+escapeHtml(selected.id)+'">Edit folder</button>':'')+'</div>':view === "sheets" ? renderSheets() : view === "ribbons" ? renderRibbons() : view === "all" ? "" : renderReel();
    if(!selected && !state.collections.length && view !== "reel")body='<div class="collection-empty">No folders yet. Create a folder to give your artifacts another home.</div>';
    surface.innerHTML = tabs + body;
    surface.hidden = view === "all" && !selected;
    surface.dataset.view = view;
    surface.querySelectorAll(".collection-artifact-card").forEach(function (card) {
      window.ArtifactPortal.revealCardPreview(card);
    });
    var nextShelf=surface.querySelector(".collection-grid");if(nextShelf)nextShelf.scrollLeft=shelfScroll;
    bindViewEvents();
    enhanceCanonicalCards();
    updateSelection();
    document.dispatchEvent(new CustomEvent("collections:rendered"));
    placeReel();
    if(focusSelector){var target=surface.querySelector(focusSelector);if(target&&!target.disabled){suppressPreviewFocus=true;target.focus({preventScroll:true});suppressPreviewFocus=false;}}
  }
  function enhanceCanonicalCards() {
    var grid = document.getElementById("artifact-grid");
    if (!grid) return;
    if (lastCanonicalEnhancementRevision === membershipRevision && lastCanonicalEnhancementCount === grid.children.length) return;
    lastCanonicalEnhancementRevision = membershipRevision;
    lastCanonicalEnhancementCount = grid.children.length;
    Array.prototype.forEach.call(grid.children, function (card) {
      collectionPerformance.canonicalEnhancements += 1;
      var names = membershipNamesByArtifact.get(card.dataset.id) || [];
      var signature = names.join("\u0001");
      if (canonicalEnhancementSignatures.get(card) === signature) return;
      canonicalEnhancementSignatures.set(card, signature);
      card.draggable = true; card.dataset.collectionArtifact = card.dataset.id;
      card.dataset.collectionNames = names.join(" ").toLowerCase();
      collectionPerformance.canonicalEnhancementWrites += 2;
      if (!card.querySelector("[data-collection-select]")) {
        var checkbox = document.createElement("input"); checkbox.type = "checkbox"; checkbox.className = "collection-artifact-select"; checkbox.dataset.collectionSelect = card.dataset.id; checkbox.setAttribute("aria-label", "Select " + ((card.querySelector(".card-title") || {}).textContent || card.dataset.id)); var preview = card.querySelector(".preview"); if (preview) preview.prepend(checkbox);
      }
      var menu = card.querySelector(".card-menu");
      if (menu && !menu.querySelector("[data-collection-folder-picker]")) {
        var folders = document.createElement("div"); folders.innerHTML = folderFieldMarkup(card.dataset.id); menu.insertBefore(folders.firstElementChild, menu.querySelector('[data-action="delete"]') || menu.querySelector(".move-confirm"));
      }
      var folderTrigger = menu && menu.querySelector("[data-collection-folder-picker]");
      if (folderTrigger) {
        var label = names.length ? names.join(", ") : "Choose folders";
        if (folderTrigger.firstElementChild.textContent !== label) {
          folderTrigger.firstElementChild.textContent = label;
          collectionPerformance.canonicalEnhancementWrites += 1;
        }
      }
    });
  }
  function updateSelection() {
    var node=document.getElementById("collection-selection");
    if(!node){node=document.createElement("div");node.id="collection-selection";node.className="collection-selection";node.setAttribute("aria-label","Selected artifacts");node.innerHTML='<span role="status"></span><button type="button" data-bulk-collect>Add to folders</button><button type="button" data-bulk-clear>Clear selection</button>';document.getElementById("library-controls")?.after(node);node.querySelector("[data-bulk-collect]").addEventListener("click",function(){folderPicker("collection:selection");});node.querySelector("[data-bulk-clear]").addEventListener("click",function(){selectedArtifacts.clear();updateSelection();});}
    node.hidden=selectedArtifacts.size===0;node.querySelector("span").textContent=selectedArtifacts.size+" selected";
    document.querySelectorAll("[data-collection-select]").forEach(function(input){var checked=selectedArtifacts.has(input.dataset.collectionSelect);if(input.checked!==checked){input.checked=checked;collectionPerformance.selectionWrites+=1;}});
  }
  function placeReel() {
    var reel=surface.querySelector(".collection-reel:not([hidden])"),anchor=surface.querySelector(".collection-shelf-nav");
    if(!reel||!anchor)return;
    reel.classList.toggle("is-pinned",reelPinned);
    if(reelPinned){reel.dataset.placement="below";reel.style.top="";reel.style.maxHeight="";return;}
    var viewport=window.visualViewport,top=viewport?viewport.offsetTop:0,bottom=top+(viewport?viewport.height:innerHeight);
    var toolbar=document.getElementById("library-controls"),bar=toolbar&&toolbar.getBoundingClientRect();
    if(bar&&getComputedStyle(toolbar).position==="sticky"&&bar.top<=top+1)top=Math.max(top,bar.bottom);
    var rect=anchor.getBoundingClientRect(),surfaceRect=surface.getBoundingClientRect(),gap=12;
    reel.style.maxHeight="";
    var above=Math.max(0,rect.top-top-gap-12),below=Math.max(0,bottom-rect.bottom-gap-12);
    var placement=reel.scrollHeight>above&&below>above?"below":"above";
    var space=placement==="below"?below:above;
    reel.dataset.placement=placement;
    reel.style.maxHeight=Math.max(1,Math.floor(space))+"px";
    var desiredTop=placement==="below"?rect.bottom+gap:rect.top-gap-reel.offsetHeight;var fitTop=Math.min(Math.max(top+8,desiredTop),Math.max(top+8,bottom-reel.offsetHeight-8));reel.style.top=(fitTop-surfaceRect.top)+"px";
  }
  function savePreferences(patch) {
    state.preferences=Object.assign({},state.preferences,patch);
    var version=++preferenceVersion; clearTimeout(saveTimer);
    saveTimer=setTimeout(function(){
      var payload={view:state.preferences.view,previewSize:state.preferences.previewSize,artifactLayout:state.preferences.artifactLayout,collectionOrderByOrg:state.preferences.collectionOrderByOrg||{},collapsedCollectionIdsByOrg:state.preferences.collapsedCollectionIdsByOrg||{}};
      endpoint("/gallery/preferences",{method:"PUT",body:JSON.stringify(payload)}).then(function(body){
        if(version!==preferenceVersion)return;saveTimer=0;state.preferences=body.preferences||body;savedPreferences=JSON.parse(JSON.stringify(state.preferences));
      }).catch(function(error){
        if(version!==preferenceVersion)return;saveTimer=0;state.preferences=savedPreferences||{};view=state.preferences.view||"reel";var url=new URL(location.href);url.searchParams.set("libraryView",view);history.replaceState(history.state,"",url);document.documentElement.dataset.collectionDensity=state.preferences.previewSize||"compact";document.querySelector('.layout-toggle [data-layout="'+(state.preferences.artifactLayout||"grid")+'"]')?.click();render();status(error.message,"error");
      });
    },180);
  }
  function refresh() {
    var sequence = ++requestSequence;
    surface.setAttribute("aria-busy", "true");
    if (!surface.children.length) surface.innerHTML = '<p class="collection-status" role="status">Loading folders…</p>';
    var library=window.ArtifactPortal?.getLibraryState();if(library)state.org=isAdmin?(library.org||"all"):viewerOrg;collectionStatus=new URL(location.href).searchParams.get("collectionStatus")||"all";
    return endpoint(projectionUrl()).then(function (body) {
      if (sequence !== requestSequence) return;
      surface.removeAttribute("aria-busy");
      normalizeProjection(body);
      var selected = new URL(location.href).searchParams.get("collection");
      var scoped = state.collections.find(function (row) { return row.id === selected; });
      state.selected = scoped ? scoped.id : null;
      if (window.ArtifactPortal && window.ArtifactPortal.setCollectionScope) window.ArtifactPortal.setCollectionScope(collectionStatus === "uncollected" ? function(id){return !state.collections.some(function(row){return row.artifactIds.indexOf(String(id))>=0;});} : scoped ? function (artifactId) { return scoped.artifactIds.indexOf(String(artifactId)) >= 0; } : null);
      document.documentElement.dataset.collectionDensity = state.preferences.previewSize || "compact";
      window.ArtifactPortal?.setArtifactLayout(state.preferences.artifactLayout || "grid");
      render();
    }).catch(function (error) {
      if (sequence !== requestSequence) return;
      surface.removeAttribute("aria-busy");
      surface.hidden = false;
      surface.innerHTML = '<p class="collection-status" role="alert"></p><button type="button" class="collection-retry">Retry loading folders</button>';
      surface.querySelector(".collection-retry").addEventListener("click", refresh);
      status(error.message, "error");
    });
  }
  function folderEditor(collection, initialArtifactIds, restoreTarget, onClose) {
    var editing = Boolean(collection);
    var cards = Array.from(document.querySelectorAll("#artifact-grid .card"));
    var selectedOrg = initialArtifactIds.length ? cards.find(function(card){return card.dataset.id === initialArtifactIds[0];})?.dataset.org : null;
    // The admin identity is an access role, not a registered organization.
    var orgs = Array.from(new Set(Array.from(document.querySelectorAll("#org-filter option")).map(function(option){return option.value;}).filter(function(org){return org && org !== "all";}))).sort();
    var initialOrg = editing ? collection.org : selectedOrg || (state.org === "all" ? (isAdmin ? "" : viewerOrg) : state.org);
    var orgField = !editing && isAdmin && state.org === "all" ? '<label>Organization<select name="org" required' + (selectedOrg ? ' disabled' : '') + '><option value="">Choose an organization…</option>' + orgs.map(function(org){return '<option value="'+escapeHtml(org)+'">'+escapeHtml(org)+'</option>';}).join("") + '</select></label>' : '';
    var colors = [{value:"#e4d3b4",label:"Sand"},{value:"#c8d8cf",label:"Sage"},{value:"#cbd7e5",label:"Slate"},{value:"#e2c9ce",label:"Rose"}];
    var initialColor = collection?.color || colors[0].value;
    if (!colors.some(function(color){return color.value.toLowerCase() === initialColor.toLowerCase();})) colors.push({value:initialColor,label:"Custom"});
    var colorMarkup = colors.map(function(color){return '<label class="collection-color-option"><input type="radio" name="color" value="'+escapeHtml(color.value)+'"'+(color.value.toLowerCase() === initialColor.toLowerCase() ? ' checked' : '')+'><span class="collection-color-swatch" style="--swatch:'+escapeHtml(color.value)+'" aria-hidden="true"></span><span>'+color.label+'</span></label>';}).join("");
    var dialog = document.createElement("dialog");
    dialog.className = "category-dialog collection-create-dialog";
    dialog.innerHTML = '<form method="dialog" class="category-panel"><div class="dialog-head"><div><p class="eyebrow">A place for related ideas</p><h2>'+(editing ? 'Edit folder' : 'Create a folder')+'</h2></div><button type="button" data-dialog-close aria-label="Close">×</button></div><div class="collection-create-layout"><div class="collection-create-fields">'+orgField+'<label>Folder name <span class="collection-field-note">Up to 80 characters</span><input name="name" maxlength="80" required autocomplete="off" placeholder="e.g. Interface experiments"></label><label>A little context <span class="collection-field-note">Optional</span><textarea name="description" aria-label="'+(editing ? 'Description' : 'A little context')+'" maxlength="500" placeholder="What belongs here?"></textarea></label><fieldset class="collection-color-field"><legend>Folder color</legend><div class="collection-color-options">'+colorMarkup+'</div></fieldset><label class="collection-cover-field">Cover preview<select name="coverArtifactId"><option value="">First artifact</option></select></label><p class="category-error" role="alert"></p></div><aside class="collection-create-preview" data-folder-live-preview aria-live="polite"><p class="collection-preview-kicker">Your folder, as it will look</p><div class="collection-preview-face"><div class="collection-preview-papers"></div><div class="collection-preview-face-content"><strong>Your new folder</strong><span></span></div></div><p class="collection-preview-help">The first collected artifact becomes the cover. A chosen cover stays fixed until you change it.</p></aside></div><div class="category-actions">'+(editing ? '<button type="button" class="collection-delete-folder">Delete folder</button>' : '<p class="collection-create-note">Artifacts can belong to more than one folder.</p>')+'<button type="button" data-dialog-close>Cancel</button><button type="submit" class="solid" value="'+(editing ? 'save' : 'create')+'">'+(editing ? 'Save changes' : 'Create folder')+'</button></div></form>';
    showCollectionDialog(dialog, restoreTarget || document.activeElement, onClose);
    var form = dialog.querySelector("form");
    var submit = form.querySelector("button[type=submit]");
    function targetOrg(){return form.elements.org ? form.elements.org.value : initialOrg;}
    function colorValue(){return form.elements.color.value;}
    form.elements.name.value = collection?.name || "";
    form.elements.description.value = collection?.description || "";
    if(form.elements.org) form.elements.org.value = initialOrg;
    function updatePreview(){
      var coverId = form.elements.coverArtifactId.value;
      var count = new Set(initialArtifactIds.concat(coverId ? [coverId] : [])).size;
      var cover = cards.find(function(card){return card.dataset.id === (coverId || initialArtifactIds[0]);});
      var source = cover?.querySelector(".pv, img");
      var face = dialog.querySelector(".collection-preview-face");
      face.style.setProperty("--collection-color",colorValue());
      var channels = colorValue().slice(1).match(/.{2}/g).map(function(channel){var value=parseInt(channel,16)/255;return value <= .04045 ? value/12.92 : Math.pow((value+.055)/1.055,2.4);});
      face.style.setProperty("--collection-ink",channels[0]*.2126+channels[1]*.7152+channels[2]*.0722 > .18 ? "#17232d" : "#ffffff");
      dialog.querySelector(".collection-preview-face-content strong").textContent = form.elements.name.value.trim() || "Your new folder";
      dialog.querySelector(".collection-preview-face-content span").textContent = count + " artifact" + (count === 1 ? "" : "s") + " · " + (targetOrg() || "Choose an organization");
      dialog.querySelector(".collection-preview-papers").innerHTML = source?.src ? '<img src="'+escapeHtml(source.src)+'" alt="">' : '';
    }
    function updateCovers(){
      var previous = form.elements.coverArtifactId.value;
      form.elements.coverArtifactId.innerHTML = '<option value="">First artifact</option>' + cards.filter(function(card){return card.dataset.org === targetOrg() && (!editing || initialArtifactIds.indexOf(card.dataset.id) >= 0);}).map(function(card){return '<option value="'+escapeHtml(card.dataset.id)+'">'+escapeHtml(card.querySelector(".card-title")?.textContent || card.dataset.id)+'</option>';}).join("");
      // Preserve covers outside the currently filtered artifact grid when editing.
      if(editing && collection.coverArtifactId && !Array.from(form.elements.coverArtifactId.options).some(function(option){return option.value === collection.coverArtifactId;})) form.elements.coverArtifactId.add(new Option("Current cover",collection.coverArtifactId));
      form.elements.coverArtifactId.value = previous || "";
      updatePreview();
    }
    updateCovers();
    form.elements.coverArtifactId.value = collection?.coverArtifactId || "";
    updatePreview();
    form.elements.org?.addEventListener("change",updateCovers);
    form.elements.name.addEventListener("input",updatePreview);
    form.elements.coverArtifactId.addEventListener("change",updatePreview);
    form.querySelectorAll('input[name="color"]').forEach(function(input){input.addEventListener("change",updatePreview);});
    if(!editing && isAdmin && state.org === "all" && !orgs.length){form.querySelector(".category-error").textContent = "Create an organization before adding a folder.";submit.disabled = true;}
    if(editing) dialog.querySelector(".collection-delete-folder").addEventListener("click",function(){deleteCollection(collection.id,function(){dialog.close("deleted");});});
    form.elements.name.focus();
    form.addEventListener("submit",function(event){
      event.preventDefault();
      if(!event.submitter || event.submitter !== submit || submit.disabled) return;
      var name = form.elements.name.value.trim();
      if(!name){form.elements.name.setCustomValidity("Enter a folder name.");form.elements.name.reportValidity();return;}
      var body = {org:targetOrg(),name:name,description:form.elements.description.value.trim(),color:colorValue(),coverArtifactId:form.elements.coverArtifactId.value || (editing ? null : undefined)};
      if(!body.org) return;
      if(!editing) body.artifactIds = Array.from(new Set(initialArtifactIds.concat(form.elements.coverArtifactId.value || [])));
      submit.disabled = true;
      form.querySelector(".category-error").textContent = "";
      endpoint(editing ? "/collections/"+encodeURIComponent(collection.id) : "/collections",{method:editing ? "PATCH" : "POST",body:JSON.stringify(body)}).then(function(){dialog.close(editing ? "saved" : "created");return refresh();}).then(function(){status(editing ? "Folder updated" : "Folder created","success");}).catch(function(error){form.querySelector(".category-error").textContent = error.message;submit.disabled = false;});
    });
    form.elements.name.addEventListener("input",function(){form.elements.name.setCustomValidity("");});
  }
  function createCollection(initialArtifactIds, restoreTarget, onClose) {
    initialArtifactIds = Array.isArray(initialArtifactIds) ? initialArtifactIds : Array.from(selectedArtifacts);
    var orgs = new Set(initialArtifactIds.map(function(id){return document.querySelector('#artifact-grid .card[data-id="'+CSS.escape(id)+'"]')?.dataset.org;}));
    if(orgs.size > 1 || orgs.has(undefined)){status("Select artifacts from one organization to create a folder.","error");return;}
    folderEditor(null,initialArtifactIds,restoreTarget,onClose);
  }
  function editCollection(id) {
    var collection = state.collections.find(function(row){return row.id === id;});
    if(collection?.editable) folderEditor(collection,collection.artifactIds);
  }
  function deleteCollection(id, onDeleted) {
    var collection = state.collections.find(function(row){return row.id === id;});
    if(!collection?.editable) return;
    var dialog = document.createElement("dialog");
    dialog.className = "category-dialog collection-delete-dialog";
    dialog.innerHTML = '<form method="dialog" class="category-panel"><div class="dialog-head"><div><p class="eyebrow">Remove a collection</p><h2>Delete folder</h2></div><button type="button" data-dialog-close aria-label="Close">×</button></div><div class="category-body"><div class="collection-delete-summary"><strong>'+escapeHtml(collection.name)+'</strong><span>'+escapeHtml(collection.org)+' · '+collection.artifactIds.length+' artifacts</span></div><p>Your artifacts will stay in the library and in their other folders.</p><p class="category-error" role="alert"></p></div><div class="category-actions"><button type="button" data-dialog-close>Cancel</button><button type="submit" class="collection-confirm-delete" value="delete">Delete folder</button></div></form>';
    showCollectionDialog(dialog,document.activeElement);
    dialog.querySelector('.category-actions [data-dialog-close]').focus();
    dialog.querySelector("form").addEventListener("submit",function(event){
      event.preventDefault();
      var submit = dialog.querySelector("button[value=delete]");
      if(event.submitter !== submit || submit.disabled) return;
      submit.disabled = true;
      dialog.querySelector(".category-error").textContent = "";
      endpoint("/collections/"+encodeURIComponent(id),{method:"DELETE"}).then(function(){dialog.close("deleted");if(onDeleted) onDeleted();status("Folder deleted","success");return refresh();}).catch(function(error){dialog.querySelector(".category-error").textContent = error.message;submit.disabled = false;});
    });
  }
  function openFolder(collectionId) {
    var collection = state.collections.find(function (row) { return row.id === collectionId; });
    if (!collection) return;
    var url = new URL(location.href); url.searchParams.set("collection", collectionId); history.pushState({ collection: collectionId }, "", url.pathname + url.search + url.hash);
    state.selected = collectionId; collectionStatus="all";url.searchParams.delete("collectionStatus");history.replaceState(history.state,"",url);
    if (window.ArtifactPortal && window.ArtifactPortal.setCollectionScope) {
      window.ArtifactPortal.setCollectionScope(function (artifactId) { return collection.artifactIds.indexOf(String(artifactId)) >= 0; });
    }
    render();
  }
  function folderPicker(target) {
    var artifactId = target.indexOf("collection:") === 0 ? "" : target;
    var selectedIds = artifactId ? [artifactId] : Array.from(selectedArtifacts);
    var targetOrgs = new Set(selectedIds.map(function(id){var card=document.querySelector('#artifact-grid .card[data-id="'+CSS.escape(id)+'"]');return card&&card.dataset.org;}).filter(Boolean));
    if(targetOrgs.size!==1){status("Select artifacts from one organization to choose their folders.","error");return;}
    var artifactOrg = selectedIds.length ? ((document.querySelector('#artifact-grid .card[data-id="' + CSS.escape(selectedIds[0]) + '"]') || {}).dataset || {}).org : null;
    var allowedCollections = state.collections.filter(function (collection) { return collection.editable && (!artifactOrg || collection.org === artifactOrg); });
    var selected = allowedCollections.filter(function (collection) { return selectedIds.some(function (id) { return collection.artifactIds.indexOf(id) >= 0; }); }).map(function (collection) { return collection.id; });
    var restoreFocus=document.activeElement;
    var dialog = document.createElement("dialog"); dialog.className = "category-dialog collection-picker-dialog";
    dialog.innerHTML = '<form method="dialog" class="category-panel"><div class="dialog-head"><div><p class="eyebrow">Organize artifact</p><h2>Choose folders</h2></div><button type="button" data-dialog-close value="cancel" aria-label="Close">×</button></div><div class="category-body"><div class="picker-list">' + allowedCollections.map(function (collection) { return '<label class="picker-row"><input type="checkbox" value="' + escapeHtml(collection.id) + '"' + (selected.indexOf(collection.id) >= 0 ? " checked" : "") + '>' + escapeHtml(collection.name) + '<small>' + escapeHtml(collection.org) + '</small></label>'; }).join("") + '</div><button type="button" data-picker-new>+ New folder</button><p class="category-error"></p></div><div class="category-actions"><button type="button" data-dialog-close value="cancel">Cancel</button><button type="submit" class="solid" value="save">Save folders</button></div></form>';
    showCollectionDialog(dialog, restoreFocus);
    dialog.querySelectorAll('.picker-row input').forEach(function(input){var row=allowedCollections.find(function(c){return c.id===input.value;});var count=selectedIds.filter(function(id){return row.artifactIds.indexOf(id)>=0;}).length;input.checked=count===selectedIds.length;input.indeterminate=count>0&&count<selectedIds.length;input.addEventListener("change",function(){input.dataset.changed="1";});});
    dialog.querySelector("[data-picker-new]").addEventListener("click", function (event) {
      createCollection(selectedIds, event.currentTarget, function (result) {
        if (result === "created") dialog.close("created");
      });
    });
    dialog.querySelector("form").addEventListener("submit", function (event) { if (!event.submitter || event.submitter.value !== "save") return; event.preventDefault(); if (!selectedIds.length) { dialog.close(); dialog.remove(); return; } var changed = Array.from(dialog.querySelectorAll('.picker-row input[data-changed="1"]')); var adds=changed.filter(function(input){return input.checked;}).map(function(input){return input.value;});var removes=changed.filter(function(input){return !input.checked;}).map(function(input){return input.value;}); var submit=dialog.querySelector("button[value=save]");submit.disabled=true;var calls = adds.map(function (id) { return endpoint("/collections/" + encodeURIComponent(id) + "/memberships", { method: "POST", body: JSON.stringify({ artifactIds: selectedIds }) }); }).concat(removes.map(function (id) { return endpoint("/collections/" + encodeURIComponent(id) + "/memberships", { method: "DELETE", body: JSON.stringify({ artifactIds: selectedIds }) }); })); Promise.all(calls).then(function () { selectedIds.forEach(function (id) { selectedArtifacts.delete(id); }); updateSelection(); dialog.close(); dialog.remove(); return refresh(); }).catch(function (error) { dialog.querySelector(".category-error").textContent = error.message;submit.disabled=false;refresh(); }); });
  }
  function reorder(id, delta) { var rows = orderedCollections(); var index = rows.findIndex(function (row) { return row.id === id; }); var next = index + delta; if (index < 0 || next < 0 || next >= rows.length) return; var order = rows.map(function (row) { return row.id; }); var moved = order.splice(index, 1)[0]; order.splice(next, 0, moved); var byOrg = Object.assign({}, state.preferences.collectionOrderByOrg || {}); byOrg[preferenceOrg()] = order; savePreferences({ collectionOrderByOrg: byOrg }); render(); status("Moved folder to position "+(next+1)+" of "+rows.length,"success"); }
  function toggleCollapse(id) { var byOrg = Object.assign({}, state.preferences.collapsedCollectionIdsByOrg || {}); var list = (byOrg[preferenceOrg()] || []).slice(); var index = list.indexOf(id); if (index < 0) list.push(id); else list.splice(index, 1); byOrg[preferenceOrg()] = list; savePreferences({ collapsedCollectionIdsByOrg: byOrg }); render(); }
  function collectArtifacts(collectionId, artifactIds) {
    var ids = artifactIds.filter(Boolean).map(String);
    if (!ids.length) return;
    endpoint("/collections/" + encodeURIComponent(collectionId) + "/memberships", { method: "POST", body: JSON.stringify({ artifactIds: ids }) }).then(function () { status("Added " + ids.length + " artifact" + (ids.length === 1 ? "" : "s") + " to the folder", "success"); return refresh(); }).catch(function (error) { status(error.message, "error"); });
  }
  function bindViewEvents() {
    surface.querySelector("[data-collection-back]")?.addEventListener("click",function(){window.ArtifactCollections.setScope(null);});
    surface.querySelectorAll("[data-shelf-scroll]").forEach(function (button) { button.addEventListener("click", function () { var rail = surface.querySelector(".collection-grid"); if (rail) rail.scrollBy({ left: Number(button.dataset.shelfScroll) * Math.max(240, rail.clientWidth * .72), behavior: "smooth" }); }); });
    surface.querySelectorAll("[data-collection-view]").forEach(function (button) { button.addEventListener("click", function () { view = button.dataset.collectionView; savePreferences({ view: view }); reelId = null; render(); }); });
    surface.querySelectorAll("[data-collection-open]").forEach(function (button) { button.addEventListener("click", function () { openFolder(button.dataset.collectionOpen); }); });
    surface.querySelectorAll(".collection-face[data-collection-id]").forEach(function (face) {
      face.addEventListener("mouseenter", function () { if (artifactDragging || reelPinned || reelId === face.dataset.collectionId || dismissedPreviewId === face.dataset.collectionId) return;dismissedPreviewId=null; reelId = face.dataset.collectionId; reelPage = 0; render(); });
    });
    surface.querySelectorAll("[data-collection-peek]").forEach(function (button) {
      button.addEventListener("mouseenter", function () { if (artifactDragging || reelPinned || reelId === button.dataset.collectionPeek || dismissedPreviewId === button.dataset.collectionPeek) return;dismissedPreviewId=null; reelId = button.dataset.collectionPeek; reelPage = 0; render(); });
      button.addEventListener("focus", function () { if (suppressPreviewFocus || reelPinned || reelId === button.dataset.collectionPeek || dismissedPreviewId === button.dataset.collectionPeek) return; reelId = button.dataset.collectionPeek; reelPage = 0; reelPinned = false; render(); requestAnimationFrame(function () { var next = surface.querySelector('[data-collection-peek="' + CSS.escape(reelId) + '"]'); if (next) next.focus({ preventScroll: true }); }); });
      button.addEventListener("click", function () { var id=button.dataset.collectionPeek;var dismiss=reelId===id&&reelPinned;dismissedPreviewId=dismiss?id:null;reelId=dismiss?null:id;reelPage=0;reelPinned=!dismiss;render();requestAnimationFrame(function(){var next=surface.querySelector('[data-collection-peek="'+CSS.escape(id)+'"]');suppressPreviewFocus=true;if(next)next.focus({preventScroll:true});suppressPreviewFocus=false;}); });
      button.addEventListener("keydown", function (event) { if (event.key === "ArrowUp") { event.preventDefault(); reelId = button.dataset.collectionPeek; reelPinned = true; render(); var close = surface.querySelector("[data-reel-close]"); if (close) close.focus(); } });
    });
    var close = surface.querySelector("[data-reel-close]"); if (close) close.addEventListener("click", function () { dismissedPreviewId=reelId;reelId = null; reelPinned = false; render(); });
    var pin = surface.querySelector("[data-reel-pin]"); if (pin) pin.addEventListener("click", function () { reelPinned = !reelPinned; render(); });
    var previous = surface.querySelector("[data-reel-prev]"); if (previous) previous.addEventListener("click", function () { if (reelPage > 0) { reelPage -= 1; render(); } });
    var next = surface.querySelector("[data-reel-next]"); if (next) next.addEventListener("click", function () { var current = state.collections.find(function (row) { return row.id === reelId; }); var total = current ? collectionArtifacts(current).length : 0; if (current && reelPage + 1 < Math.ceil(total / reelPageSize())) { reelPage += 1; render(); } });
    surface.querySelectorAll("[data-collection-create]").forEach(function (create) { create.addEventListener("click", createCollection); });
    surface.querySelectorAll("[data-collection-edit]").forEach(function (button) { button.addEventListener("click", function () { editCollection(button.dataset.collectionEdit); }); });
    surface.querySelectorAll("[data-collection-delete]").forEach(function (button) { button.addEventListener("click", function () { deleteCollection(button.dataset.collectionDelete); }); });
    surface.querySelectorAll("[data-ribbon-page]").forEach(function(button){button.addEventListener("click",function(){var id=button.closest("[data-ribbon-id]").dataset.ribbonId;ribbonPages[id]=Math.max(0,(ribbonPages[id]||0)+Number(button.dataset.ribbonPage));render();});});
    surface.querySelectorAll("[data-ribbon-collapse]").forEach(function (button) { button.addEventListener("click", function () { toggleCollapse(button.closest("[data-ribbon-id]").dataset.ribbonId); }); });
    surface.querySelectorAll("[data-ribbon-move]").forEach(function (button) { button.addEventListener("click", function () { reorder(button.closest("[data-ribbon-id]").dataset.ribbonId, button.dataset.ribbonMove === "up" ? -1 : 1); }); });
    surface.querySelectorAll("[data-ribbon-drag]").forEach(function (grip) { grip.addEventListener("keydown", function (event) { if (event.key === "ArrowUp" || event.key === "ArrowDown") { event.preventDefault(); reorder(grip.closest("[data-ribbon-id]").dataset.ribbonId, event.key === "ArrowUp" ? -1 : 1); } }); });
    surface.querySelectorAll("[data-ribbon-id]").forEach(function (row) { row.addEventListener("dragstart", function (event) { if (event.target.closest("[data-collection-artifact]")) return; dragId = row.dataset.ribbonId; row.classList.add("is-dragging"); }); row.addEventListener("dragend", function () { dragId = null; row.classList.remove("is-dragging"); }); row.addEventListener("dragover", function (event) { if (!dragId) return; event.preventDefault(); var after=event.clientY>row.getBoundingClientRect().top+row.getBoundingClientRect().height/2;row.classList.toggle("drop-after",after);row.classList.toggle("drop-before",!after); }); row.addEventListener("dragleave", function () { row.classList.remove("drop-after","drop-before"); }); row.addEventListener("drop", function (event) { event.preventDefault(); row.classList.remove("drop-after","drop-before"); if (!dragId || dragId === row.dataset.ribbonId) return; var rows = orderedCollections().map(function (item) { return item.id; }); var from = rows.indexOf(dragId); var after=event.clientY>row.getBoundingClientRect().top+row.getBoundingClientRect().height/2;rows.splice(from,1);var to=rows.indexOf(row.dataset.ribbonId)+(after?1:0);rows.splice(to,0,dragId); var byOrg = Object.assign({}, state.preferences.collectionOrderByOrg || {}); byOrg[preferenceOrg()] = rows; savePreferences({ collectionOrderByOrg: byOrg }); render(); }); });
    surface.querySelectorAll("[data-collection-id]").forEach(function (drop) { drop.addEventListener("dragover", function (event) { if (dragId) return; event.preventDefault(); drop.classList.add("drop-ready"); }); drop.addEventListener("dragleave", function () { drop.classList.remove("drop-ready"); }); drop.addEventListener("drop", function (event) { if (dragId) return; event.preventDefault(); drop.classList.remove("drop-ready"); var ids = event.dataTransfer && event.dataTransfer.getData("text/plain"); collectArtifacts(drop.dataset.collectionId, ids ? ids.split(",") : Array.from(selectedArtifacts)); }); });
    surface.querySelectorAll("[data-collection-select]").forEach(function (input) { input.checked = selectedArtifacts.has(input.dataset.collectionSelect); input.addEventListener("change", function () { if (input.checked) selectedArtifacts.add(input.dataset.collectionSelect); else selectedArtifacts.delete(input.dataset.collectionSelect); }); });
  }
  document.addEventListener("click", function (event) { var picker = event.target.closest("[data-collection-folder-picker]"); if (picker) { event.preventDefault(); openFolderPopover(picker); } });
  document.addEventListener("change", function (event) {
    var input = event.target.closest("[data-collection-select]");
    if (!input) return;
    if (input.checked) selectedArtifacts.add(input.dataset.collectionSelect); else selectedArtifacts.delete(input.dataset.collectionSelect);
    updateSelection();
  });
  document.addEventListener("dragstart", function (event) {
    var card = event.target.closest("[data-collection-artifact]");
    if (!card) return;
    var control = event.target.closest("button,input,select,textarea,a");
    if (control && !control.matches(".preview a,.collection-artifact-preview a")) { event.preventDefault(); return; }
    if (!event.dataTransfer) return;
    artifactDragging = true;
    clearTimeout(leaveTimer);
    event.dataTransfer.effectAllowed = "copy";
    event.dataTransfer.setData("text/plain", selectedArtifacts.has(card.dataset.collectionArtifact) ? Array.from(selectedArtifacts).join(",") : card.dataset.collectionArtifact);
  });
  document.addEventListener("dragend", function () {
    artifactDragging = false;
    surface.querySelectorAll(".drop-ready").forEach(function (node) { node.classList.remove("drop-ready"); });
  });
  document.addEventListener("keydown", function (event) {
    if (event.defaultPrevented || event.key !== "Escape" || !reelId || document.querySelector("dialog[open]")) return;
    var id = reelId; dismissedPreviewId=id;reelId = null; reelPinned = false; render();
    requestAnimationFrame(function () { var face = surface.querySelector('[data-collection-peek="' + CSS.escape(id) + '"]'); suppressPreviewFocus = true; if (face) face.focus({ preventScroll: true }); suppressPreviewFocus = false; });
  });
  document.addEventListener("pointermove",function(event){if(!dismissedPreviewId)return;var face=surface.querySelector('[data-collection-peek="'+CSS.escape(dismissedPreviewId)+'"]');var rect=face?.getBoundingClientRect();if(!rect||event.clientX<rect.left||event.clientX>rect.right||event.clientY<rect.top||event.clientY>rect.bottom)dismissedPreviewId=null;});
  window.addEventListener("scroll", placeReel, { passive: true });
  var narrowViewport=matchMedia("(max-width:760px)");narrowViewport.addEventListener("change",function(){reelPage=0;ribbonPages={};render();});
  window.addEventListener("resize", placeReel);
  window.addEventListener("popstate", refresh);
  function previewHasInteraction(){return artifactDragging || !!document.querySelector("dialog[open]") || !!surface.querySelector(".card-menu:not([hidden])") || !!surface.querySelector(".collection-reel:focus-within");}
  var leaveTimer = 0;
  surface.addEventListener("pointerleave", function () {
    if (artifactDragging || !reelId || reelPinned) return;
    clearTimeout(leaveTimer);
    leaveTimer = setTimeout(function () {
      if (!reelId || surface.matches(":hover") || previewHasInteraction()) return;
      reelId = null; reelPage = 0; render();
    }, 140);
  });
  surface.addEventListener("pointerout", function (event) {
    if (artifactDragging || !reelId || reelPinned || !event.target.closest(".collection-grid")) return;
    var next = event.relatedTarget;
    if (next && (next.closest && (next.closest(".collection-grid") || next.closest(".collection-reel")))) return;
    clearTimeout(leaveTimer);
    leaveTimer = setTimeout(function () { if (reelId && !surface.querySelector(".collection-reel:hover") && !previewHasInteraction()) { reelId = null; reelPage = 0; render(); } }, 140);
  });
  document.addEventListener("collections:filters-changed", function () {
    var library = window.ArtifactPortal && window.ArtifactPortal.getLibraryState ? window.ArtifactPortal.getLibraryState() : {};
    var nextOrg = isAdmin ? (library.org || "all") : viewerOrg;
    if (nextOrg !== state.org) { state.org = nextOrg; reelId = null; reelPinned = false; refresh(); }
    else render();
  });
  document.addEventListener("artifact:updated", function () { refresh(); });
  window.ArtifactCollections = Object.freeze({
    refresh: refresh,
    getState: function () { var library = window.ArtifactPortal && window.ArtifactPortal.getLibraryState ? window.ArtifactPortal.getLibraryState() : {}; return { preferences: state.preferences, selected: state.selected, org: state.org, view: view, status: collectionStatus === "uncollected" ? "uncollected" : library.status || "all", query: library.q || "", category: library.category || "all", sort: library.sort || "recent", collections: state.collections.slice(), uncollectedCount: state.uncollectedCount }; },
    getPerformanceStats: function () { return Object.assign({}, collectionPerformance); },
    setView: function (next) { if (["reel", "sheets", "ribbons", "all"].indexOf(next) < 0) return; view = next; var url = new URL(location.href); url.searchParams.set("libraryView",next); history.replaceState(history.state,"",url); savePreferences({ view: next }); reelId=null; reelPinned=false; render(); },
    setStatus: function (next) {
      collectionStatus=next;
      var url=new URL(location.href); if(next==="uncollected"){url.searchParams.set("collectionStatus",next);url.searchParams.delete("collection");}else url.searchParams.delete("collectionStatus");history.replaceState(history.state,"",url);
      var control = document.getElementById("library-status"); if (control) control.value = next;
      if (next === "uncollected") {
        if (window.ArtifactPortal && window.ArtifactPortal.setCollectionScope) window.ArtifactPortal.setCollectionScope(function (artifactId) { return !state.collections.some(function (row) { return row.artifactIds.indexOf(String(artifactId)) >= 0; }); });
        state.selected = null; render(); return;
      }
      var scoped=state.collections.find(function(row){return row.id===state.selected;});
      if(window.ArtifactPortal?.setCollectionScope)window.ArtifactPortal.setCollectionScope(scoped?function(id){return scoped.artifactIds.indexOf(String(id))>=0;}:null);
      var button = document.querySelector('[data-filter-view="' + CSS.escape(next) + '"]'); if (button) button.click();
    },
    setDensity: function (next) { if (next !== "compact" && next !== "large") return; savePreferences({ previewSize: next }); document.documentElement.dataset.collectionDensity = next; render(); },
    setLayout: function (next) { var button = document.querySelector('.layout-toggle [data-layout="' + CSS.escape(next) + '"]'); if (button) button.click(); savePreferences({ artifactLayout: next }); },
    setScope: function (id) { if (id) openFolder(id); else { state.selected = null; var url=new URL(location.href);url.searchParams.delete("collection");history.replaceState(history.state,"",url); if (window.ArtifactPortal && window.ArtifactPortal.clearCollectionScope) window.ArtifactPortal.clearCollectionScope(); render(); } },
    clearFilters: function () { collectionStatus="all"; state.selected=null; var url=new URL(location.href);url.searchParams.delete("collection");url.searchParams.delete("collectionStatus");history.replaceState(history.state,"",url); if (window.ArtifactPortal && window.ArtifactPortal.clearCollectionScope)window.ArtifactPortal.clearCollectionScope(); if (window.ArtifactPortal && window.ArtifactPortal.clearFilters) window.ArtifactPortal.clearFilters(); }
  });
  var initial = window.ArtifactPortal && window.ArtifactPortal.getLibraryState ? window.ArtifactPortal.getLibraryState() : {};
  state.org = isAdmin ? (initial.org || "all") : viewerOrg;
  refresh();
}());
