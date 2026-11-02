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

  function registerCategoryLocally(org, name) {
    var rows = Array.isArray(orgCategoryIndex[org]) ? orgCategoryIndex[org] : [];
    if (!rows.some(function (row) { return row.name === name; })) rows.push({ name: name, count: 0 });
    orgCategoryIndex[org] = sortCategoryRows(rows);
    if (categoryDataNode) categoryDataNode.dataset.json = JSON.stringify(orgCategoryIndex);
  }

  function rebuildCardCategory(card, org, preferred) {
    var select = card && card.querySelector(".category-menu");
    if (!select) return "";
    var rows = sortCategoryRows(categoryRows(org));
    select.replaceChildren();
    appendOption(select, "", "Uncategorized", preferred === "");
    rows.forEach(function (row) {
      if (row.name) appendOption(select, row.name, row.name, preferred === row.name);
    });
    if (select.dataset.canCreate === "1") appendOption(select, createCategoryValue, "+ Category", false);
    var valid = preferred !== createCategoryValue && Array.prototype.some.call(select.options, function (option) { return option.value === preferred; });
    select.value = valid ? preferred : "";
    select.dataset.stagedCategory = select.value;
    return select.value;
  }

  function stageMove(card, destination) {
    var confirm = card.querySelector(".move-confirm");
    var category = card.querySelector(".category-menu");
    if (!confirm || !category) return;
    var label = category.value || "Uncategorized";
    confirm.dataset.destination = destination;
    confirm.dataset.category = category.value;
    confirm.querySelector(".move-question").textContent =
      "Move from " + card.dataset.org + " to " + destination + " in " + label + "? Active public share links will be revoked.";
    confirm.hidden = false;
  }

  var categoryDialog = document.getElementById("category-dialog");
  var categoryForm = document.getElementById("category-form");
  var categoryName = document.getElementById("category-name");
  var categoryOrg = document.getElementById("category-org");
  var categoryOrgLabel = document.getElementById("category-org-label");
  var categoryCopy = document.getElementById("category-copy");
  var categoryError = document.getElementById("category-error");
  var categorySubmit = categoryForm && categoryForm.querySelector(".category-submit");
  var categoryContext = null;

  function openCategoryDialog(context) {
    if (!categoryDialog || !categoryForm || !categoryName || !categoryOrg) return;
    categoryContext = context;
    categoryError.textContent = "";
    categoryName.value = "";
    categoryOrg.replaceChildren();
    Object.keys(orgCategoryIndex).sort().forEach(function (org) {
      appendOption(categoryOrg, org, org, false);
    });
    var chooseOrg = context.org === "all";
    categoryOrgLabel.hidden = !chooseOrg;
    categoryOrg.disabled = !chooseOrg;
    if (!chooseOrg) {
      if (!Array.prototype.some.call(categoryOrg.options, function (option) { return option.value === context.org; })) {
        appendOption(categoryOrg, context.org, context.org, false);
      }
      categoryOrg.value = context.org;
    }
    var target = chooseOrg ? "the selected organization" : context.org;
    categoryCopy.textContent = context.mode === "card"
      ? "Create a category for " + target + ". It will be selected for this artifact."
      : "Create a category for " + target + ".";
    categoryDialog.showModal();
    categoryName.focus();
  }

  function closeCategoryDialog() {
    if (categoryDialog && categoryDialog.open) categoryDialog.close("cancel");
  }

  if (categoryDialog) {
    categoryDialog.querySelectorAll(".category-close,.category-cancel").forEach(function (button) {
      button.addEventListener("click", closeCategoryDialog);
    });
    categoryDialog.addEventListener("close", function () {
      var restore = categoryContext && categoryContext.restoreFocus;
      categoryContext = null;
      categoryOrg.disabled = false;
      categoryForm.reset();
      categoryError.textContent = "";
      if (restore && restore.isConnected) restore.focus();
    });
  }

  if (categoryForm) {
    categoryForm.addEventListener("submit", function (event) {
      event.preventDefault();
      if (!categoryContext) return;
      var name = String(categoryName.value || "").trim().replace(/\s+/g, " ").slice(0, 60);
      var org = categoryContext.org === "all" ? categoryOrg.value : categoryContext.org;
      if (!name || !org) {
        categoryError.textContent = !name ? "Enter a category name." : "Choose an organization.";
        return;
      }
      categorySubmit.disabled = true;
      categoryError.textContent = "";
      var context = categoryContext;
      var create = canCreateCategories
        ? jsonRequest("/settings/orgs/" + encodeURIComponent(org) + "/categories", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ name: name }),
          })
        : Promise.resolve({ org: org, name: name });
      create.then(function (body) {
        var created = String(body.name || name);
        registerCategoryLocally(org, created);
        if (context.mode === "filter") {
          if (activeOrg === "all") {
            activeOrg = org;
            if (orgFilter) orgFilter.value = org;
          }
          rebuildCategoryFilter("all");
          applyFilters();
          categoryDialog.close("created");
          toast("Category created for " + org);
          return;
        }
        var card = context.card;
        var selected = rebuildCardCategory(card, org, created);
        categoryDialog.close("created");
        if (org === card.dataset.org) {
          var select = card.querySelector(".category-menu");
          select.disabled = true;
          requestCategory(card, selected).catch(function (error) {
            select.disabled = false;
            toast(error.message || "Could not change category");
          });
        } else {
          stageMove(card, org);
          toast("Category created for " + org);
        }
      }).catch(function (error) {
        categoryError.textContent = error.message || "Could not create category.";
      }).finally(function () {
        categorySubmit.disabled = false;
      });
    });
  }

