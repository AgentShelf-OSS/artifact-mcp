  function closeMenu(menu, restoreFocus) {
    if (!menu) return;
    if (menu.matches(":popover-open")) menu.hidePopover();
    menu.hidden = true;
    var folderPanel = menu.querySelector("[data-collection-folder-panel]");
    var folderTrigger = menu.querySelector("[data-collection-folder-picker]");
    if (folderPanel) folderPanel.hidden = true;
    if (folderTrigger) folderTrigger.setAttribute("aria-expanded", "false");
    menu.style.removeProperty("position");
    menu.style.removeProperty("margin");
    menu.style.removeProperty("top");
    menu.style.removeProperty("bottom");
    menu.style.removeProperty("left");
    menu.style.removeProperty("right");
    menu.style.removeProperty("max-height");
    var card = menu.closest(".card");
    if (card) card.classList.remove("menu-open");
    var trigger = card && card.querySelector('[data-action="more"]');
    if (trigger) {
      trigger.setAttribute("aria-expanded", "false");
      if (restoreFocus) trigger.focus();
    }
  }

  function positionMenu(menu, trigger) {
    if (!menu || menu.hidden || !trigger) return;
    var viewport = window.visualViewport;
    var height = viewport ? viewport.height : window.innerHeight;
    var width = viewport ? viewport.width : window.innerWidth;
    var top = viewport ? viewport.offsetTop : 0;
    var left = viewport ? viewport.offsetLeft : 0;
    var pad = 12, gap = 8;
    var triggerBox = trigger.getBoundingClientRect();
    menu.style.maxHeight = Math.max(1, height - pad * 2) + "px";
    menu.style.overflowY = "auto";
    menu.style.margin = "0";
    menu.style.bottom = "auto";
    menu.style.right = "auto";
    var card = menu.closest(".card");
    var cardBox = card && card.getBoundingClientRect();
    var phone = window.innerWidth <= 760 && cardBox;
    menu.style.maxWidth = Math.max(1, width - pad * 2) + "px";
    menu.style.width = phone ? Math.min(cardBox.width, Math.max(1, width - pad * 2)) + "px" : "";
    var box = menu.getBoundingClientRect();
    var desiredX = phone ? cardBox.left : triggerBox.right - box.width;
    var x = Math.min(Math.max(desiredX, left + pad), Math.max(left + pad, left + width - box.width - pad));
    // Prefer below on phones too; try above if the menu does not fit below.
    var y = triggerBox.bottom + gap;
    if (y + box.height > top + height - pad) y = triggerBox.top - box.height - gap;
    y = Math.min(Math.max(y, top + pad), Math.max(top + pad, top + height - box.height - pad));
    var parent = menu.matches(":popover-open") ? null : menu.offsetParent;
    var parentBox = parent ? parent.getBoundingClientRect() : { left: 0, top: 0 };
    menu.style.left = Math.round(x - parentBox.left) + "px";
    menu.style.top = Math.round(y - parentBox.top) + "px";
  }

  function repositionOpenMenus() {
    document.querySelectorAll(".card-menu:not([hidden])").forEach(function (menu) {
      var trigger = menu.closest(".card")?.querySelector('[data-action="more"]');
      positionMenu(menu, trigger);
    });
  }

  function openMenu(trigger) {
    var card = trigger.closest(".card");
    var menu = card.querySelector(".card-menu");
    document.querySelectorAll(".card-menu:not([hidden])").forEach(function (candidate) {
      if (candidate !== menu) closeMenu(candidate, false);
    });
    var open = menu.hidden;
    if (!open) { closeMenu(menu, false); return; }
    menu.hidden = false;
    // A top-layer popover escapes preview clipping while retaining its card parent.
    if (typeof menu.showPopover === "function") {
      menu.setAttribute("popover", "manual");
      menu.style.position = "fixed";
      menu.showPopover();
    }
    card.classList.toggle("menu-open", open);
    trigger.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      positionMenu(menu, trigger);
      var first = menu.querySelector("a[href],select,button");
      if (first) first.focus();
    }
  }

  document.addEventListener("artifact-menu:resize", function (event) {
    var menu = event.detail && event.detail.menu;
    positionMenu(menu, menu && menu.closest(".card")?.querySelector('[data-action="more"]'));
  });
  window.addEventListener("scroll", repositionOpenMenus, { passive: true });
  window.addEventListener("resize", repositionOpenMenus);

