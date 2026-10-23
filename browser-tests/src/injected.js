// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// The in-page half of the locator and assertion layer (`src/locator.rs`, `src/expect.rs`). The
// Rust side evaluates this file once in each JavaScript context it queries (the main world of a
// frame) and then calls `globalThis[Symbol.for("browser-tests")]` through
// `Runtime.callFunctionOn`. The property is a non-enumerable symbol, so page code does not see it
// in `Object.keys(window)`.
//
// The engines follow Playwright's injected script (playwright-core `injectedScript.ts`,
// `selectorUtils.ts`, `roleUtils.ts`, `domUtils.ts`) so that ported tests keep their meaning:
// text, label and role matching, accessible names, which elements are hidden, element states,
// strict single-element resolution and the `expect` matchers.
(() => {
  const KEY = Symbol.for("browser-tests");
  if (globalThis[KEY]) return;

  // ---------------------------------------------------------------- DOM helpers

  const tagName = (element) => (element.localName || "").toUpperCase();

  function parentElementOrShadowHost(element) {
    if (element.parentElement) return element.parentElement;
    if (!element.parentNode) return null;
    if (element.parentNode.nodeType === 11 && element.parentNode.host) return element.parentNode.host;
    return null;
  }

  function enclosingShadowRootOrDocument(element) {
    let node = element;
    while (node.parentNode) node = node.parentNode;
    if (node.nodeType === 11 || node.nodeType === 9) return node;
    return null;
  }

  function enclosingShadowHost(element) {
    while (element.parentElement) element = element.parentElement;
    return parentElementOrShadowHost(element);
  }

  function closestCrossShadow(element, css) {
    while (element) {
      const closest = element.closest(css);
      if (closest) return closest;
      element = enclosingShadowHost(element);
    }
    return null;
  }

  function computedStyle(element, pseudo) {
    const view = element.ownerDocument && element.ownerDocument.defaultView;
    return view ? view.getComputedStyle(element, pseudo) : null;
  }

  const normalizeWhiteSpace = (text) => text.replace(/[​­]/g, "").trim().replace(/\s+/g, " ");

  // Every element under `root` (not `root` itself), piercing open shadow roots, light DOM first.
  function allElements(root) {
    const result = [];
    const visit = (scope) => {
      const shadows = [];
      if (scope.shadowRoot) shadows.push(scope.shadowRoot);
      for (const element of scope.querySelectorAll("*")) {
        result.push(element);
        if (element.shadowRoot) shadows.push(element.shadowRoot);
      }
      shadows.forEach(visit);
    };
    visit(root);
    return result;
  }

  // `selector` under `root`, piercing open shadow roots.
  function queryCSS(root, selector) {
    const result = [];
    const visit = (scope) => {
      for (const element of scope.querySelectorAll(selector)) result.push(element);
      for (const element of scope.querySelectorAll("*")) if (element.shadowRoot) visit(element.shadowRoot);
    };
    visit(root);
    return result;
  }

  function getIdRefs(element, ref) {
    if (!ref) return [];
    const root = enclosingShadowRootOrDocument(element);
    if (!root) return [];
    const result = [];
    for (const id of ref.split(" ").filter(Boolean)) {
      const found = root.querySelector("#" + CSS.escape(id));
      if (found && !result.includes(found)) result.push(found);
    }
    return result;
  }

  // ---------------------------------------------------------------- visibility

  function isStyleVisible(element, style) {
    style = style || computedStyle(element);
    if (!style) return true;
    if (typeof element.checkVisibility === "function" && !element.checkVisibility()) return false;
    return style.visibility === "visible";
  }

  function isVisibleTextNode(node) {
    const range = node.ownerDocument.createRange();
    range.selectNode(node);
    const rect = range.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }

  // Playwright's `isElementVisible`: a non-empty box and visibility:visible. Opacity does not count.
  function isElementVisible(element) {
    const style = computedStyle(element);
    if (!style) return true;
    if (style.display === "contents") {
      for (let child = element.firstChild; child; child = child.nextSibling) {
        if (child.nodeType === 1 && isElementVisible(child)) return true;
        if (child.nodeType === 3 && isVisibleTextNode(child)) return true;
      }
      return false;
    }
    if (!isStyleVisible(element, style)) return false;
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }

  // ---------------------------------------------------------------- text

  function shouldSkipForText(node) {
    return node.nodeName === "SCRIPT" || node.nodeName === "NOSCRIPT" || node.nodeName === "STYLE" ||
      !!(node.ownerDocument && node.ownerDocument.head && node.ownerDocument.head.contains(node));
  }

  function elementText(cache, root) {
    let value = cache.get(root);
    if (value !== undefined) return value;
    value = { full: "", normalized: "", immediate: [] };
    if (!shouldSkipForText(root)) {
      let current = "";
      if (root.nodeName === "INPUT" && (root.type === "submit" || root.type === "button")) {
        value = { full: root.value, normalized: normalizeWhiteSpace(root.value), immediate: [root.value] };
      } else {
        for (let child = root.firstChild; child; child = child.nextSibling) {
          if (child.nodeType === 3) {
            value.full += child.nodeValue || "";
            current += child.nodeValue || "";
          } else if (child.nodeType === 8) {
            continue;
          } else {
            if (current) value.immediate.push(current);
            current = "";
            if (child.nodeType === 1) value.full += elementText(cache, child).full;
          }
        }
        if (current) value.immediate.push(current);
        if (root.shadowRoot) value.full += elementText(cache, root.shadowRoot).full;
        if (value.full) value.normalized = normalizeWhiteSpace(value.full);
      }
    }
    cache.set(root, value);
    return value;
  }

  // A pattern is {kind: "substring" | "exact" | "regex", text, flags}. As in Playwright's internal
  // text matcher: a substring is case-insensitive, exact is case-sensitive and whole, both after
  // whitespace normalisation; a regular expression is tested against the raw text.
  function textMatcher(pattern) {
    if (pattern.kind === "regex") {
      const re = new RegExp(pattern.text, pattern.flags || "");
      return { kind: "regex", match: (text) => { re.lastIndex = 0; return re.test(text.full); } };
    }
    const wanted = normalizeWhiteSpace(pattern.text);
    if (pattern.kind === "exact") return { kind: "strict", match: (text) => text.normalized === wanted };
    const lower = wanted.toLowerCase();
    return { kind: "lax", match: (text) => text.normalized.toLowerCase().includes(lower) };
  }

  function stringMatcher(pattern) {
    const { match } = textMatcher(pattern);
    return (value) => match({ full: value, normalized: normalizeWhiteSpace(value), immediate: [value] });
  }

  function elementMatchesText(cache, element, match) {
    if (shouldSkipForText(element)) return "none";
    if (!match(elementText(cache, element))) return "none";
    for (let child = element.firstChild; child; child = child.nextSibling) {
      if (child.nodeType === 1 && match(elementText(cache, child))) return "selfAndChildren";
    }
    if (element.shadowRoot && match(elementText(cache, element.shadowRoot))) return "selfAndChildren";
    return "self";
  }

  // getByText: the smallest elements whose text matches (the root itself included).
  function queryText(root, pattern) {
    const { kind, match } = textMatcher(pattern);
    const cache = new Map();
    const result = [];
    let lastDidNotMatchSelf = null;
    const append = (element) => {
      if (kind === "lax" && lastDidNotMatchSelf && lastDidNotMatchSelf.contains(element)) return;
      const matches = elementMatchesText(cache, element, match);
      if (matches === "none") lastDidNotMatchSelf = element;
      if (matches === "self") result.push(element);
    };
    if (root.nodeType === 1) append(root);
    for (const element of allElements(root)) append(element);
    return result;
  }

  // ---------------------------------------------------------------- ARIA roles

  const validRoles = new Set(["alert", "alertdialog", "application", "article", "banner", "blockquote", "button",
    "caption", "cell", "checkbox", "code", "columnheader", "combobox", "complementary", "contentinfo", "definition",
    "deletion", "dialog", "directory", "document", "emphasis", "feed", "figure", "form", "generic", "grid",
    "gridcell", "group", "heading", "img", "insertion", "link", "list", "listbox", "listitem", "log", "main", "mark",
    "marquee", "math", "meter", "menu", "menubar", "menuitem", "menuitemcheckbox", "menuitemradio", "navigation",
    "none", "note", "option", "paragraph", "presentation", "progressbar", "radio", "radiogroup", "region", "row",
    "rowgroup", "rowheader", "scrollbar", "search", "searchbox", "separator", "slider", "spinbutton", "status",
    "strong", "subscript", "superscript", "switch", "tab", "table", "tablist", "tabpanel", "term", "textbox", "time",
    "timer", "toolbar", "tooltip", "tree", "treegrid", "treeitem"]);

  const ancestorPreventingLandmark = "article:not([role]), aside:not([role]), main:not([role]), nav:not([role]), section:not([role]), [role=article], [role=complementary], [role=main], [role=navigation], [role=region]";

  const globalAriaAttributes = [
    ["aria-atomic"], ["aria-busy"], ["aria-controls"], ["aria-current"], ["aria-describedby"], ["aria-details"],
    ["aria-dropeffect"], ["aria-flowto"], ["aria-grabbed"], ["aria-hidden"], ["aria-keyshortcuts"],
    ["aria-label", ["caption", "code", "deletion", "emphasis", "generic", "insertion", "paragraph", "presentation", "strong", "subscript", "superscript"]],
    ["aria-labelledby", ["caption", "code", "deletion", "emphasis", "generic", "insertion", "paragraph", "presentation", "strong", "subscript", "superscript"]],
    ["aria-live"], ["aria-owns"], ["aria-relevant"], ["aria-roledescription", ["generic"]],
  ];

  function hasGlobalAriaAttribute(element, forRole) {
    return globalAriaAttributes.some(([attribute, prohibited]) =>
      !(prohibited && prohibited.includes(forRole || "")) && element.hasAttribute(attribute));
  }

  const hasExplicitAccessibleName = (element) => element.hasAttribute("aria-label") || element.hasAttribute("aria-labelledby");
  const hasTabIndex = (element) => !Number.isNaN(Number(String(element.getAttribute("tabindex"))));

  function isNativelyFocusable(element) {
    const name = tagName(element);
    if (["BUTTON", "DETAILS", "SELECT", "TEXTAREA"].includes(name)) return true;
    if (name === "A" || name === "AREA") return element.hasAttribute("href");
    if (name === "INPUT") return !element.hidden && element.type !== "hidden";
    return element.isContentEditable === true;
  }

  const isFocusable = (element) => !isNativelyDisabled(element) && (isNativelyFocusable(element) || hasTabIndex(element));
  const hasPresentationConflictResolution = (element, role) => hasGlobalAriaAttribute(element, role) || isFocusable(element);

  const inputTypeToRole = { button: "button", checkbox: "checkbox", image: "button", number: "spinbutton",
    radio: "radio", range: "slider", reset: "button", submit: "button" };

  function cellRole(element) {
    const table = closestCrossShadow(element, "table");
    const role = table ? getExplicitAriaRole(table) : "";
    return role === "grid" || role === "treegrid" ? "gridcell" : "cell";
  }

  const implicitRoleByTagName = {
    A: (e) => e.hasAttribute("href") ? "link" : null,
    AREA: (e) => e.hasAttribute("href") ? "link" : null,
    ARTICLE: () => "article", ASIDE: () => "complementary", BLOCKQUOTE: () => "blockquote", BUTTON: () => "button",
    CAPTION: () => "caption", CODE: () => "code", DATALIST: () => "listbox", DD: () => "definition",
    DEL: () => "deletion", DETAILS: () => "group", DFN: () => "term", DIALOG: () => "dialog", DT: () => "term",
    EM: () => "emphasis", FIELDSET: () => "group", FIGURE: () => "figure",
    FOOTER: (e) => closestCrossShadow(e, ancestorPreventingLandmark) ? null : "contentinfo",
    FORM: (e) => hasExplicitAccessibleName(e) ? "form" : null,
    H1: () => "heading", H2: () => "heading", H3: () => "heading", H4: () => "heading", H5: () => "heading", H6: () => "heading",
    HEADER: (e) => closestCrossShadow(e, ancestorPreventingLandmark) ? null : "banner",
    HR: () => "separator", HTML: () => "document",
    IMG: (e) => e.getAttribute("alt") === "" && !e.getAttribute("title") && !hasGlobalAriaAttribute(e) && !hasTabIndex(e) ? "presentation" : "img",
    INPUT: (e) => {
      const type = e.type.toLowerCase();
      if (type === "search") return e.hasAttribute("list") ? "combobox" : "searchbox";
      if (["email", "tel", "text", "url", ""].includes(type)) {
        const list = getIdRefs(e, e.getAttribute("list"))[0];
        return list && tagName(list) === "DATALIST" ? "combobox" : "textbox";
      }
      if (type === "hidden") return null;
      if (type === "file") return "button";
      return inputTypeToRole[type] || "textbox";
    },
    INS: () => "insertion", LI: () => "listitem", MAIN: () => "main", MARK: () => "mark", MATH: () => "math",
    MENU: () => "list", METER: () => "meter", NAV: () => "navigation", OL: () => "list", OPTGROUP: () => "group",
    OPTION: () => "option", OUTPUT: () => "status", P: () => "paragraph", PROGRESS: () => "progressbar",
    SEARCH: () => "search",
    SECTION: (e) => hasExplicitAccessibleName(e) ? "region" : null,
    SELECT: (e) => e.hasAttribute("multiple") || e.size > 1 ? "listbox" : "combobox",
    STRONG: () => "strong", SUB: () => "subscript", SUP: () => "superscript", SVG: () => "img",
    TABLE: () => "table", TBODY: () => "rowgroup", TD: cellRole, TEXTAREA: () => "textbox", TFOOT: () => "rowgroup",
    TH: (e) => {
      if (e.getAttribute("scope") === "col") return "columnheader";
      if (e.getAttribute("scope") === "row") return "rowheader";
      return cellRole(e) === "gridcell" ? "gridcell" : "columnheader";
    },
    THEAD: () => "rowgroup", TIME: () => "time", TR: () => "row", UL: () => "list",
  };

  const presentationInheritanceParents = {
    DD: ["DL", "DIV"], DIV: ["DL"], DT: ["DL", "DIV"], LI: ["OL", "UL"], TBODY: ["TABLE"], TD: ["TR"],
    TFOOT: ["TABLE"], TH: ["TR"], THEAD: ["TABLE"], TR: ["THEAD", "TBODY", "TFOOT", "TABLE"],
  };

  function getExplicitAriaRole(element) {
    const roles = (element.getAttribute("role") || "").split(" ").map((role) => role.trim());
    return roles.find((role) => validRoles.has(role)) || null;
  }

  function getImplicitAriaRole(element) {
    const compute = implicitRoleByTagName[tagName(element)];
    const implicitRole = (compute && compute(element)) || "";
    if (!implicitRole) return null;
    let ancestor = element;
    while (ancestor) {
      const parent = parentElementOrShadowHost(ancestor);
      const parents = presentationInheritanceParents[tagName(ancestor)];
      if (!parents || !parent || !parents.includes(tagName(parent))) break;
      const parentRole = getExplicitAriaRole(parent);
      if ((parentRole === "none" || parentRole === "presentation") && !hasPresentationConflictResolution(parent, parentRole)) return parentRole;
      ancestor = parent;
    }
    return implicitRole;
  }

  function getAriaRole(element) {
    const explicitRole = getExplicitAriaRole(element);
    if (!explicitRole) return getImplicitAriaRole(element);
    if (explicitRole === "none" || explicitRole === "presentation") {
      const implicitRole = getImplicitAriaRole(element);
      if (hasPresentationConflictResolution(element, implicitRole)) return implicitRole;
    }
    return explicitRole;
  }

  const ariaBoolean = (value) => value === null ? null : value.toLowerCase() === "true";

  // ---------------------------------------------------------------- hidden for ARIA

  let hiddenCache = null;

  function belongsToDisplayNoneOrAriaHiddenOrNonSlotted(element) {
    let hidden = hiddenCache && hiddenCache.get(element);
    if (hidden === undefined || hidden === null) {
      hidden = false;
      if (element.parentElement && element.parentElement.shadowRoot && !element.assignedSlot) hidden = true;
      if (!hidden) {
        const style = computedStyle(element);
        hidden = !style || style.display === "none" || ariaBoolean(element.getAttribute("aria-hidden")) === true;
      }
      if (!hidden) {
        const parent = parentElementOrShadowHost(element);
        if (parent) hidden = belongsToDisplayNoneOrAriaHiddenOrNonSlotted(parent);
      }
      if (hiddenCache) hiddenCache.set(element, hidden);
    }
    return hidden;
  }

  function isElementIgnoredForAria(element) {
    return ["STYLE", "SCRIPT", "NOSCRIPT", "TEMPLATE"].includes(tagName(element));
  }

  function isElementHiddenForAria(element) {
    if (isElementIgnoredForAria(element)) return true;
    const style = computedStyle(element);
    const isSlot = element.nodeName === "SLOT";
    if (style && style.display === "contents" && !isSlot) {
      for (let child = element.firstChild; child; child = child.nextSibling) {
        if (child.nodeType === 1 && !isElementHiddenForAria(child)) return false;
        if (child.nodeType === 3 && isVisibleTextNode(child)) return false;
      }
      return true;
    }
    const isOptionInsideSelect = element.nodeName === "OPTION" && !!element.closest("select");
    if (!isOptionInsideSelect && !isSlot && !isStyleVisible(element, style)) return true;
    return belongsToDisplayNoneOrAriaHiddenOrNonSlotted(element);
  }

  // ---------------------------------------------------------------- accessible name

  let nameCache = null;

  function getAriaLabelledByElements(element) {
    const ref = element.getAttribute("aria-labelledby");
    if (ref === null) return null;
    const refs = getIdRefs(element, ref);
    return refs.length ? refs : null;
  }

  function trimFlatString(text) {
    return text.trim();
  }

  function pseudoContent(element, pseudo) {
    const style = computedStyle(element, pseudo);
    if (!style || style.display === "none" || style.visibility === "hidden") return "";
    const content = style.content;
    let resolved;
    if ((content[0] === "'" && content[content.length - 1] === "'") || (content[0] === '"' && content[content.length - 1] === '"')) {
      resolved = content.substring(1, content.length - 1);
    } else if (content.startsWith("attr(") && content.endsWith(")")) {
      resolved = element.getAttribute(content.substring(5, content.length - 1).trim()) || "";
    }
    if (resolved === undefined) return "";
    const display = style.display || "inline";
    return display !== "inline" ? " " + resolved + " " : resolved;
  }

  function allowsNameFromContent(role, targetDescendant) {
    const always = ["button", "cell", "checkbox", "columnheader", "gridcell", "heading", "link", "menuitem",
      "menuitemcheckbox", "menuitemradio", "option", "radio", "row", "rowheader", "switch", "tab", "tooltip", "treeitem"].includes(role);
    const descendant = targetDescendant && ["", "caption", "code", "contentinfo", "definition", "deletion", "emphasis",
      "insertion", "list", "listitem", "mark", "none", "paragraph", "presentation", "region", "row", "rowgroup",
      "section", "strong", "subscript", "superscript", "table", "term", "time"].includes(role);
    return always || descendant;
  }

  function nameFromLabels(labels, options) {
    return [...labels].map((label) => getTextAlternative(label, {
      ...options,
      embeddedInLabel: { element: label, hidden: isElementHiddenForAria(label) },
      embeddedInNativeTextAlternative: undefined,
      embeddedInLabelledBy: undefined,
      embeddedInDescribedBy: undefined,
      embeddedInTargetElement: undefined,
    })).filter((name) => !!name).join(" ");
  }

  function innerAccumulatedText(element, options) {
    const tokens = [];
    const visit = (node, skipSlotted) => {
      if (skipSlotted && node.assignedSlot) return;
      if (node.nodeType === 1) {
        const style = computedStyle(node);
        const display = (style && style.display) || "inline";
        let token = getTextAlternative(node, options);
        if (display !== "inline" || node.nodeName === "BR") token = " " + token + " ";
        tokens.push(token);
      } else if (node.nodeType === 3) {
        tokens.push(node.textContent || "");
      }
    };
    tokens.push(pseudoContent(element, "::before"));
    const assigned = element.nodeName === "SLOT" ? element.assignedNodes() : [];
    if (assigned.length) {
      for (const child of assigned) visit(child, false);
    } else {
      for (let child = element.firstChild; child; child = child.nextSibling) visit(child, true);
      if (element.shadowRoot) {
        for (let child = element.shadowRoot.firstChild; child; child = child.nextSibling) visit(child, true);
      }
      for (const owned of getIdRefs(element, element.getAttribute("aria-owns"))) visit(owned, true);
    }
    tokens.push(pseudoContent(element, "::after"));
    return tokens.join("");
  }

  function getTextAlternative(element, options) {
    if (options.visitedElements.has(element)) return "";
    const childOptions = {
      ...options,
      embeddedInTargetElement: options.embeddedInTargetElement === "self" ? "descendant" : options.embeddedInTargetElement,
    };

    if (!options.includeHidden) {
      const inHiddenReference = !!(options.embeddedInLabelledBy && options.embeddedInLabelledBy.hidden) ||
        !!(options.embeddedInDescribedBy && options.embeddedInDescribedBy.hidden) ||
        !!(options.embeddedInNativeTextAlternative && options.embeddedInNativeTextAlternative.hidden) ||
        !!(options.embeddedInLabel && options.embeddedInLabel.hidden);
      if (isElementIgnoredForAria(element) || (!inHiddenReference && isElementHiddenForAria(element))) {
        options.visitedElements.add(element);
        return "";
      }
    }

    const labelledBy = getAriaLabelledByElements(element);
    if (!options.embeddedInLabelledBy) {
      const name = (labelledBy || []).map((ref) => getTextAlternative(ref, {
        ...options,
        embeddedInLabelledBy: { element: ref, hidden: isElementHiddenForAria(ref) },
        embeddedInDescribedBy: undefined,
        embeddedInTargetElement: undefined,
        embeddedInLabel: undefined,
        embeddedInNativeTextAlternative: undefined,
      })).join(" ");
      if (name) return name;
    }

    const role = getAriaRole(element) || "";
    const name = tagName(element);

    if (!!options.embeddedInLabel || !!options.embeddedInLabelledBy || options.embeddedInTargetElement === "descendant") {
      const isOwnLabel = [...(element.labels || [])].includes(element);
      const isOwnLabelledBy = (labelledBy || []).includes(element);
      if (!isOwnLabel && !isOwnLabelledBy) {
        if (role === "textbox") {
          options.visitedElements.add(element);
          if (name === "INPUT" || name === "TEXTAREA") return element.value;
          return element.textContent || "";
        }
        if (["combobox", "listbox"].includes(role)) {
          options.visitedElements.add(element);
          let selected;
          if (name === "SELECT") {
            selected = [...element.selectedOptions];
            if (!selected.length && element.options.length) selected.push(element.options[0]);
          } else {
            const listbox = role === "combobox" ? allElements(element).find((e) => getAriaRole(e) === "listbox") : element;
            selected = listbox ? [...listbox.querySelectorAll('[aria-selected="true"]')].filter((e) => getAriaRole(e) === "option") : [];
          }
          if (!selected.length && name === "INPUT") return element.value;
          return selected.map((option) => getTextAlternative(option, childOptions)).join(" ");
        }
        if (["progressbar", "scrollbar", "slider", "spinbutton", "meter"].includes(role)) {
          options.visitedElements.add(element);
          if (element.hasAttribute("aria-valuetext")) return element.getAttribute("aria-valuetext") || "";
          if (element.hasAttribute("aria-valuenow")) return element.getAttribute("aria-valuenow") || "";
          return element.getAttribute("value") || "";
        }
        if (role === "menu") {
          options.visitedElements.add(element);
          return "";
        }
      }
    }

    const ariaLabel = element.getAttribute("aria-label") || "";
    if (trimFlatString(ariaLabel)) {
      options.visitedElements.add(element);
      return ariaLabel;
    }

    if (!["presentation", "none"].includes(role)) {
      if (name === "INPUT" && ["button", "submit", "reset"].includes(element.type)) {
        options.visitedElements.add(element);
        const value = element.value || "";
        if (trimFlatString(value)) return value;
        if (element.type === "submit") return "Submit";
        if (element.type === "reset") return "Reset";
        return element.getAttribute("title") || "";
      }
      if (name === "INPUT" && element.type === "image") {
        options.visitedElements.add(element);
        const labels = element.labels || [];
        if (labels.length && !options.embeddedInLabelledBy) return nameFromLabels(labels, options);
        const alt = element.getAttribute("alt") || "";
        if (trimFlatString(alt)) return alt;
        const title = element.getAttribute("title") || "";
        if (trimFlatString(title)) return title;
        return "Submit";
      }
      if (!labelledBy && name === "BUTTON") {
        options.visitedElements.add(element);
        const labels = element.labels || [];
        if (labels.length) return nameFromLabels(labels, options);
      }
      if (!labelledBy && name === "OUTPUT") {
        options.visitedElements.add(element);
        const labels = element.labels || [];
        if (labels.length) return nameFromLabels(labels, options);
        return element.getAttribute("title") || "";
      }
      if (!labelledBy && (name === "TEXTAREA" || name === "SELECT" || name === "INPUT")) {
        options.visitedElements.add(element);
        const labels = element.labels || [];
        if (labels.length) return nameFromLabels(labels, options);
        const usePlaceholder = (name === "INPUT" && ["text", "password", "search", "tel", "email", "url"].includes(element.type)) || name === "TEXTAREA";
        const placeholder = element.getAttribute("placeholder") || "";
        const title = element.getAttribute("title") || "";
        if (!usePlaceholder || title) return title;
        return placeholder;
      }
      if (!labelledBy && (name === "FIELDSET" || name === "FIGURE")) {
        options.visitedElements.add(element);
        const caption = name === "FIELDSET" ? "LEGEND" : "FIGCAPTION";
        for (let child = element.firstElementChild; child; child = child.nextElementSibling) {
          if (tagName(child) === caption) {
            return getTextAlternative(child, { ...childOptions, embeddedInNativeTextAlternative: { element: child, hidden: isElementHiddenForAria(child) } });
          }
        }
        return element.getAttribute("title") || "";
      }
      if (name === "IMG") {
        options.visitedElements.add(element);
        const alt = element.getAttribute("alt") || "";
        if (trimFlatString(alt)) return alt;
        return element.getAttribute("title") || "";
      }
      if (name === "TABLE") {
        options.visitedElements.add(element);
        for (let child = element.firstElementChild; child; child = child.nextElementSibling) {
          if (tagName(child) === "CAPTION") {
            return getTextAlternative(child, { ...childOptions, embeddedInNativeTextAlternative: { element: child, hidden: isElementHiddenForAria(child) } });
          }
        }
        const summary = element.getAttribute("summary") || "";
        if (summary) return summary;
      }
      if (name === "AREA") {
        options.visitedElements.add(element);
        const alt = element.getAttribute("alt") || "";
        if (trimFlatString(alt)) return alt;
        return element.getAttribute("title") || "";
      }
      if (name === "SVG" || element.ownerSVGElement) {
        options.visitedElements.add(element);
        for (let child = element.firstElementChild; child; child = child.nextElementSibling) {
          if (tagName(child) === "TITLE" && child.ownerSVGElement) {
            return getTextAlternative(child, { ...childOptions, embeddedInLabelledBy: { element: child, hidden: isElementHiddenForAria(child) } });
          }
        }
      }
      if (element.ownerSVGElement && name === "A") {
        const title = element.getAttribute("xlink:title") || "";
        if (trimFlatString(title)) {
          options.visitedElements.add(element);
          return title;
        }
      }
    }

    const summaryNamesFromContent = name === "SUMMARY" && !["presentation", "none"].includes(role);
    if (allowsNameFromContent(role, options.embeddedInTargetElement === "descendant") || summaryNamesFromContent ||
        !!options.embeddedInLabelledBy || !!options.embeddedInDescribedBy || !!options.embeddedInLabel ||
        !!options.embeddedInNativeTextAlternative) {
      options.visitedElements.add(element);
      const accumulated = innerAccumulatedText(element, childOptions);
      const maybeTrimmed = options.embeddedInTargetElement === "self" ? trimFlatString(accumulated) : accumulated;
      if (maybeTrimmed) return accumulated;
    }

    if (!["presentation", "none"].includes(role) || name === "IFRAME") {
      options.visitedElements.add(element);
      const title = element.getAttribute("title") || "";
      if (trimFlatString(title)) return title;
    }

    options.visitedElements.add(element);
    return "";
  }

  function getAccessibleName(element, includeHidden) {
    const key = includeHidden ? "hidden" : "visible";
    const cached = nameCache && nameCache[key].get(element);
    if (cached !== undefined && cached !== null) return cached;
    let name = "";
    const prohibited = ["caption", "code", "definition", "deletion", "emphasis", "generic", "insertion", "mark",
      "paragraph", "presentation", "strong", "subscript", "suggestion", "superscript", "term", "time"];
    if (!prohibited.includes(getAriaRole(element) || "")) {
      name = normalizeWhiteSpace(getTextAlternative(element, {
        includeHidden,
        visitedElements: new Set(),
        embeddedInTargetElement: "self",
      }));
    }
    if (nameCache) nameCache[key].set(element, name);
    return name;
  }

  // ---------------------------------------------------------------- ARIA states

  const checkedRoles = ["checkbox", "menuitemcheckbox", "option", "radio", "switch", "menuitemradio", "treeitem"];
  const readonlyRoles = ["checkbox", "combobox", "grid", "gridcell", "listbox", "radiogroup", "slider", "spinbutton",
    "textbox", "columnheader", "rowheader", "searchbox", "switch", "treegrid"];
  const disabledRoles = ["application", "button", "composite", "gridcell", "group", "input", "link", "menuitem",
    "scrollbar", "separator", "tab", "checkbox", "columnheader", "combobox", "grid", "listbox", "menu", "menubar",
    "menuitemcheckbox", "menuitemradio", "option", "radio", "radiogroup", "row", "rowheader", "searchbox", "select",
    "slider", "spinbutton", "switch", "tablist", "textbox", "toolbar", "tree", "treegrid", "treeitem"];

  // true, false, "mixed", or "error" (not a checkbox or radio).
  function getChecked(element, allowMixed) {
    const name = tagName(element);
    if (allowMixed && name === "INPUT" && element.indeterminate) return "mixed";
    if (name === "INPUT" && ["checkbox", "radio"].includes(element.type)) return element.checked;
    if (checkedRoles.includes(getAriaRole(element) || "")) {
      const checked = element.getAttribute("aria-checked");
      if (checked === "true") return true;
      if (allowMixed && checked === "mixed") return "mixed";
      return false;
    }
    return "error";
  }

  function getAriaPressed(element) {
    if (getAriaRole(element) !== "button") return false;
    const pressed = element.getAttribute("aria-pressed");
    if (pressed === "true") return true;
    if (pressed === "mixed") return "mixed";
    return false;
  }

  function getAriaSelected(element) {
    if (tagName(element) === "OPTION") return element.selected;
    if (["gridcell", "option", "row", "tab", "columnheader", "rowheader", "treeitem"].includes(getAriaRole(element) || "")) {
      return ariaBoolean(element.getAttribute("aria-selected")) === true;
    }
    return false;
  }

  function getAriaExpanded(element) {
    if (tagName(element) === "DETAILS") return element.open;
    const expanded = element.getAttribute("aria-expanded");
    if (expanded === null) return undefined;
    return expanded === "true";
  }

  function getAriaLevel(element) {
    const native = { H1: 1, H2: 2, H3: 3, H4: 4, H5: 5, H6: 6 }[tagName(element)];
    if (native) return native;
    if (["heading", "listitem", "row", "treeitem"].includes(getAriaRole(element) || "")) {
      const level = Number(element.getAttribute("aria-level"));
      if (Number.isInteger(level) && level >= 1) return level;
    }
    return 0;
  }

  function belongsToDisabledFieldSet(element) {
    const fieldset = element.parentElement && element.parentElement.closest("fieldset[disabled]");
    if (!fieldset) return false;
    const legend = fieldset.querySelector(":scope > legend");
    return !legend || !legend.contains(element);
  }

  function isNativelyDisabled(element) {
    const native = ["BUTTON", "INPUT", "SELECT", "TEXTAREA", "OPTION", "OPTGROUP"].includes(tagName(element));
    if (!native) return false;
    if (element.hasAttribute("disabled")) return true;
    const optgroup = tagName(element) === "OPTION" && element.closest("optgroup");
    if (optgroup && optgroup.hasAttribute("disabled")) return true;
    return belongsToDisabledFieldSet(element);
  }

  function hasExplicitAriaDisabled(element, isAncestor) {
    if (!element) return false;
    if (isAncestor || disabledRoles.includes(getAriaRole(element) || "")) {
      const attribute = (element.getAttribute("aria-disabled") || "").toLowerCase();
      if (attribute === "true") return true;
      if (attribute === "false") return false;
      return hasExplicitAriaDisabled(parentElementOrShadowHost(element), true);
    }
    return false;
  }

  const getAriaDisabled = (element) => isNativelyDisabled(element) || hasExplicitAriaDisabled(element, false);

  function getReadonly(element) {
    const name = tagName(element);
    if (["INPUT", "TEXTAREA", "SELECT"].includes(name)) return element.hasAttribute("readonly");
    if (readonlyRoles.includes(getAriaRole(element) || "")) return element.getAttribute("aria-readonly") === "true";
    if (element.isContentEditable) return false;
    return "error";
  }

  // ---------------------------------------------------------------- engines

  function queryRole(root, step) {
    const result = [];
    const nameMatch = step.name ? stringMatcher(step.name) : null;
    const match = (element) => {
      if (getAriaRole(element) !== step.role) return;
      if (step.selected !== undefined && getAriaSelected(element) !== step.selected) return;
      if (step.checked !== undefined && getChecked(element, true) !== step.checked) return;
      if (step.pressed !== undefined && getAriaPressed(element) !== step.pressed) return;
      if (step.expanded !== undefined && getAriaExpanded(element) !== step.expanded) return;
      if (step.level !== undefined && getAriaLevel(element) !== step.level) return;
      if (step.disabled !== undefined && getAriaDisabled(element) !== step.disabled) return;
      if (!step.includeHidden && isElementHiddenForAria(element)) return;
      if (nameMatch && !nameMatch(getAccessibleName(element, !!step.includeHidden))) return;
      result.push(element);
    };
    for (const element of allElements(root)) match(element);
    return result;
  }

  function getElementLabels(cache, element) {
    const labelledBy = getAriaLabelledByElements(element);
    if (labelledBy) return labelledBy.map((label) => elementText(cache, label));
    const ariaLabel = element.getAttribute("aria-label");
    if (ariaLabel !== null && ariaLabel.trim()) {
      return [{ full: ariaLabel, normalized: normalizeWhiteSpace(ariaLabel), immediate: [ariaLabel] }];
    }
    const isNonHiddenInput = element.nodeName === "INPUT" && element.type !== "hidden";
    if (["BUTTON", "METER", "OUTPUT", "PROGRESS", "SELECT", "TEXTAREA"].includes(element.nodeName) || isNonHiddenInput) {
      const labels = element.labels;
      if (labels) return [...labels].map((label) => elementText(cache, label));
    }
    return [];
  }

  function queryLabel(root, pattern) {
    const { match } = textMatcher(pattern);
    const cache = new Map();
    return allElements(root).filter((element) => getElementLabels(cache, element).some((label) => match(label)));
  }

  function queryAttribute(root, name, pattern) {
    const match = stringMatcher(pattern);
    return allElements(root).filter((element) => element.hasAttribute(name) && match(element.getAttribute(name)));
  }

  function dedupe(elements) {
    return [...new Set(elements)];
  }

  // Run one frame's steps from `root` (the document, or an element for `has`).
  function run(root, steps) {
    let current = [root];
    for (const step of steps) {
      if (step.css !== undefined) {
        current = dedupe(current.flatMap((scope) => queryCSS(scope, step.css)));
      } else if (step.role !== undefined) {
        current = dedupe(current.flatMap((scope) => queryRole(scope, step)));
      } else if (step.text !== undefined) {
        current = dedupe(current.flatMap((scope) => queryText(scope, step.text)));
      } else if (step.label !== undefined) {
        current = dedupe(current.flatMap((scope) => queryLabel(scope, step.label)));
      } else if (step.attribute !== undefined) {
        current = dedupe(current.flatMap((scope) => queryAttribute(scope, step.attribute, step.value)));
      } else if (step.hasText !== undefined) {
        const { match } = textMatcher(step.hasText);
        const cache = new Map();
        current = current.filter((element) => elementMatchesText(cache, element, match) !== "none");
      } else if (step.hasNotText !== undefined) {
        const { match } = textMatcher(step.hasNotText);
        const cache = new Map();
        current = current.filter((element) => elementMatchesText(cache, element, match) === "none");
      } else if (step.has !== undefined) {
        current = current.filter((element) => run(element, step.has).some((found) => found !== element));
      } else if (step.hasNot !== undefined) {
        current = current.filter((element) => !run(element, step.hasNot).some((found) => found !== element));
      } else if (step.visible !== undefined) {
        current = current.filter((element) => element.nodeType === 1 && isElementVisible(element) === step.visible);
      } else if (step.nth !== undefined) {
        const index = step.nth < 0 ? current.length + step.nth : step.nth;
        current = index >= 0 && index < current.length ? [current[index]] : [];
      } else {
        throw new Error("browser-tests: unknown locator step " + JSON.stringify(step));
      }
    }
    return current.filter((node) => node.nodeType === 1);
  }

  function withCaches(fn) {
    hiddenCache = new Map();
    nameCache = { hidden: new Map(), visible: new Map() };
    try {
      return fn();
    } finally {
      hiddenCache = null;
      nameCache = null;
    }
  }

  const query = (steps) => withCaches(() => run(document, steps));

  // ---------------------------------------------------------------- descriptions

  function preview(element) {
    if (!element) return "<none>";
    if (element.nodeType !== 1) return String(element.nodeName);
    const name = element.localName;
    const attributes = [];
    for (const attribute of ["id", "class", "role", "type", "name", "aria-label", "href", "data-id", "title"]) {
      if (element.hasAttribute(attribute)) {
        const value = element.getAttribute(attribute);
        attributes.push(attribute + '="' + (value.length > 40 ? value.slice(0, 40) + "…" : value) + '"');
      }
    }
    let text = normalizeWhiteSpace(element.textContent || "");
    if (text.length > 50) text = text.slice(0, 50) + "…";
    return "<" + name + (attributes.length ? " " + attributes.join(" ") : "") + ">" + text + "</" + name + ">";
  }

  // ---------------------------------------------------------------- states and actions

  function retarget(node, behavior) {
    let element = node.nodeType === 1 ? node : node.parentElement;
    if (!element) return null;
    if (behavior === "none") return element;
    if (!element.matches("input, textarea, select") && !element.isContentEditable) {
      if (behavior === "button-link") element = element.closest("button, [role=button], a, [role=link]") || element;
      else element = element.closest("button, [role=button], [role=checkbox], [role=radio]") || element;
    }
    if (behavior === "follow-label") {
      if (!element.matches("a, input, textarea, button, select, [role=link], [role=button], [role=checkbox], [role=switch], [role=radio]") && !element.isContentEditable) {
        const label = element.closest("label");
        if (label && label.control) element = label.control;
      }
    }
    return element;
  }

  // Playwright's elementState: {matches, received}.
  function elementState(node, state) {
    const element = retarget(node, ["visible", "hidden"].includes(state) ? "none" : "follow-label");
    if (!element || !element.isConnected) {
      if (state === "hidden") return { matches: true, received: "hidden" };
      return { matches: false, received: "error:notconnected" };
    }
    if (state === "visible" || state === "hidden") {
      const visible = isElementVisible(element);
      return { matches: state === "visible" ? visible : !visible, received: visible ? "visible" : "hidden" };
    }
    if (state === "enabled" || state === "disabled") {
      const disabled = getAriaDisabled(element);
      return { matches: state === "disabled" ? disabled : !disabled, received: disabled ? "disabled" : "enabled" };
    }
    if (state === "editable") {
      const disabled = getAriaDisabled(element);
      const readonly = getReadonly(element);
      if (readonly === "error") throw new Error("Element is not an <input>, <textarea>, <select> or [contenteditable] and does not have a role allowing [aria-readonly]");
      return { matches: !disabled && !readonly, received: disabled ? "disabled" : readonly ? "readOnly" : "editable" };
    }
    if (state === "checked" || state === "unchecked" || state === "mixed") {
      const need = state === "checked" ? true : state === "unchecked" ? false : "mixed";
      const checked = getChecked(element, state === "mixed");
      if (checked === "error") throw new Error("Not a checkbox or radio button");
      return { matches: checked === need, received: checked === true ? "checked" : checked === false ? "unchecked" : "mixed" };
    }
    throw new Error("Unexpected element state " + state);
  }

  function isFocused(element) {
    const root = element.getRootNode();
    return root.activeElement === element && !!element.ownerDocument && element.ownerDocument.hasFocus();
  }

  function nextFrame() {
    return new Promise((resolve) => {
      let done = false;
      const finish = () => { if (!done) { done = true; resolve(); } };
      requestAnimationFrame(finish);
      // Animation frames stop in hidden or throttled frames; do not wait for ever.
      setTimeout(finish, 100);
    });
  }

  function hitTarget(target, x, y) {
    const roots = [];
    let parent = target;
    while (parent) {
      const root = enclosingShadowRootOrDocument(parent);
      if (!root) break;
      roots.push(root);
      if (root.nodeType === 9) break;
      parent = root.host;
    }
    let hit;
    for (let index = roots.length - 1; index >= 0; index--) {
      const root = roots[index];
      const elements = root.elementsFromPoint(x, y);
      const single = root.elementFromPoint(x, y);
      if (single && elements[0] && parentElementOrShadowHost(single) === elements[0]) {
        const style = computedStyle(single);
        if (style && style.display === "contents") elements.unshift(single);
      }
      if (elements[0] && elements[0].shadowRoot === root && elements[1] === single) elements.shift();
      const inner = elements[0];
      if (!inner) break;
      hit = inner;
      if (index && inner !== roots[index - 1].host) break;
    }
    const hitParents = [];
    while (hit && hit !== target) {
      hitParents.push(hit);
      hit = parentElementOrShadowHost(hit);
    }
    if (hit === target) return "done";
    let description = preview(hitParents[0] || document.documentElement);
    let element = target;
    while (element) {
      const index = hitParents.indexOf(element);
      if (index !== -1) {
        if (index > 1) description += " from " + preview(hitParents[index - 1]) + " subtree";
        break;
      }
      element = parentElementOrShadowHost(element);
    }
    return description;
  }

  // Playwright's hit-target interceptor: while a pointer action runs, the first trusted event
  // of the action must hit the target (or a descendant). If it does not (the page moved between
  // the check and the input), every event of the action is blocked and the action is retried.
  const mouseEvents = new Set(["mousedown", "mouseup", "pointerdown", "pointerup", "click", "auxclick", "dblclick", "contextmenu"]);
  const hoverEvents = new Set(["mousemove"]);
  const tapEvents = new Set(["pointerdown", "pointerup", "touchstart", "touchend", "touchcancel"]);
  let interceptor = null;
  let interceptResult;
  for (const type of new Set([...mouseEvents, ...hoverEvents, ...tapEvents])) {
    window.addEventListener(type, (event) => { if (interceptor) interceptor(event); }, { capture: true, passive: false });
  }

  function intercept(element, kind) {
    const target = retarget(element, "button-link");
    const events = kind === "hover" ? hoverEvents : kind === "tap" ? tapEvents : mouseEvents;
    interceptResult = undefined;
    interceptor = (event) => {
      if (!events.has(event.type) || !event.isTrusted) return;
      const point = (window.TouchEvent && event instanceof window.TouchEvent) ? event.touches[0] : event;
      if (interceptResult === undefined && point) interceptResult = hitTarget(target, point.clientX, point.clientY);
      if (interceptResult !== undefined && interceptResult !== "done") {
        event.preventDefault();
        event.stopPropagation();
        event.stopImmediatePropagation();
      }
    };
  }

  function stopIntercept() {
    interceptor = null;
    // No event at all (an overlaying frame, a disabled control) counts as a hit, as in Playwright.
    return interceptResult || "done";
  }

  function textMatches(expected, received, options) {
    // Playwright's ExpectedTextMatcher.
    if (received === null || received === undefined) return false;
    if (expected.kind === "regex") {
      let flags = expected.flags || "";
      if (options.ignoreCase === true && !flags.includes("i")) flags += "i";
      const re = new RegExp(expected.text, flags);
      return re.test(received);
    }
    const normalize = (text) => {
      if (options.normalize) text = normalizeWhiteSpace(text);
      if (options.ignoreCase) text = text.toLocaleLowerCase();
      return text;
    };
    const want = normalize(expected.text);
    const got = normalize(received);
    return options.substring ? got.includes(want) : got === want;
  }

  function elementValue(element) {
    if (!["INPUT", "TEXTAREA", "SELECT"].includes(tagName(element))) throw new Error("Not an input element");
    return element.value;
  }

  async function inViewportRatio(element) {
    return await new Promise((resolve) => {
      const observer = new IntersectionObserver((entries) => {
        resolve(entries[0].intersectionRatio);
        observer.disconnect();
      });
      observer.observe(element);
      requestAnimationFrame(() => {});
    });
  }

  async function expectSingle(element, m) {
    const text = (el) => m.useInnerText ? el.innerText : elementText(new Map(), el).full;
    switch (m.expression) {
      case "attached": return { matches: element.isConnected, received: "attached" };
      case "visible": case "hidden": case "enabled": case "disabled": case "editable": case "checked": case "unchecked":
        return elementState(element, m.expression);
      case "focused": {
        const focused = isFocused(element);
        return { matches: focused, received: focused ? "focused" : "not focused" };
      }
      case "inViewport": {
        const ratio = await inViewportRatio(element);
        return { matches: ratio > 0 && ratio > (m.ratio || 0) - 1e-9, received: "viewport ratio " + ratio };
      }
      case "text": {
        const received = text(element);
        return { matches: textMatches(m.expected, received, { normalize: true, substring: !!m.substring, ignoreCase: m.ignoreCase }), received };
      }
      case "attribute": {
        if (m.expected === undefined || m.expected === null) {
          const has = element.hasAttribute(m.name);
          return { matches: has, received: has ? "attribute present" : "attribute absent" };
        }
        const value = element.getAttribute(m.name);
        if (value === null) return { matches: false, received: null };
        return { matches: textMatches(m.expected, value, { ignoreCase: m.ignoreCase }), received: value };
      }
      case "value": {
        const value = elementValue(element);
        return { matches: textMatches(m.expected, value, {}), received: value };
      }
      case "class": {
        const value = element.getAttribute("class") || "";
        return { matches: textMatches(m.expected, value, { normalize: true }), received: value };
      }
      case "css": {
        const value = computedStyle(element).getPropertyValue(m.name);
        return { matches: textMatches(m.expected, value, {}), received: value };
      }
      default:
        throw new Error("Unknown expectation " + m.expression);
    }
  }

  function expectArray(elements, m) {
    if (m.expression === "count") return { matches: elements.length === m.count, received: elements.length };
    const texts = elements.map((element) => m.useInnerText ? element.innerText : elementText(new Map(), element).full);
    const options = { normalize: true, substring: !!m.substring, ignoreCase: m.ignoreCase };
    let matches;
    if (m.substring) {
      // toContainText([...]): each expected text is in some element, in order.
      let at = 0;
      matches = true;
      for (const expected of m.expected) {
        while (at < texts.length && !textMatches(expected, texts[at], options)) at++;
        if (at >= texts.length) {
          matches = false;
          break;
        }
        at++;
      }
    } else {
      matches = texts.length === m.expected.length && texts.every((text, index) => textMatches(m.expected[index], text, options));
    }
    return { matches, received: texts };
  }

  const api = {
    query,
    preview,
    // The single element `steps` select, or how many they select (0 or more than one).
    single(steps) {
      const elements = query(steps);
      return elements.length === 1 ? elements[0] : elements.length;
    },
    count: (steps) => query(steps).length,
    previews: (steps, limit) => query(steps).slice(0, limit).map(preview),
    async expect(steps, m) {
      const elements = query(steps);
      if (m.expression === "count" || m.expression === "text.array") {
        return withCaches(() => expectArray(elements, m));
      }
      if (elements.length > 1) return { strict: elements.length, previews: elements.slice(0, 10).map(preview) };
      const element = elements[0];
      if (!element) return { missing: true };
      return await expectSingle(element, m);
    },
    state: (element, state) => withCaches(() => elementState(element, state)),
    // The first state of `states` the element is not in, or null.
    waitingFor(element, states) {
      return withCaches(() => {
        for (const state of states) {
          const result = elementState(element, state);
          if (!result.matches) return { state, received: result.received };
        }
        return null;
      });
    },
    async stable(element) {
      const box = () => { const rect = element.getBoundingClientRect(); return [rect.left, rect.top, rect.width, rect.height].join(","); };
      let last = box();
      await nextFrame();
      for (let attempt = 0; attempt < 4; attempt++) {
        if (!element.isConnected) return false;
        const now = box();
        if (now === last) return true;
        last = now;
        await nextFrame();
      }
      return false;
    },
    scroll(element, alignment) {
      const block = alignment || "center";
      element.scrollIntoView({ block, inline: block, behavior: "instant" });
    },
    // The element's client rectangles and this frame's viewport, in this frame's CSS pixels.
    rects(element) {
      const rects = [...element.getClientRects()].filter((rect) => rect.width * rect.height > 0)
        .map((rect) => ({ x: rect.left, y: rect.top, width: rect.width, height: rect.height }));
      return { rects, viewport: { width: innerWidth, height: innerHeight } };
    },
    box(element) {
      if (!element.getClientRects().length) return null;
      const rect = element.getBoundingClientRect();
      return { x: rect.left, y: rect.top, width: rect.width, height: rect.height };
    },
    // The content box origin of an iframe element, in its document's viewport coordinates.
    frameOffset(frame) {
      const rect = frame.getBoundingClientRect();
      const style = computedStyle(frame);
      return {
        x: rect.left + frame.clientLeft + parseFloat(style.paddingLeft || "0"),
        y: rect.top + frame.clientTop + parseFloat(style.paddingTop || "0"),
      };
    },
    hit(element, x, y, behavior) {
      const target = retarget(element, behavior || "button-link");
      if (!target || !target.isConnected) return "the element is not connected";
      return hitTarget(target, x, y);
    },
    intercept,
    stopIntercept,
    focus(element, resetSelection) {
      const active = element.getRootNode().activeElement;
      const wasFocused = active === element && element.ownerDocument && element.ownerDocument.hasFocus();
      if (element.isContentEditable && !wasFocused && active && active.blur) active.blur();
      element.focus();
      element.focus();
      if (resetSelection && !wasFocused && element.nodeName.toLowerCase() === "input") {
        try { element.setSelectionRange(0, 0); } catch (error) { /* not a text input */ }
      }
    },
    // Playwright's fill: "done" when the value was set directly, "needsinput" when the text is
    // selected and focused for typing.
    fill(node, value) {
      const element = retarget(node, "follow-label");
      if (!element) return "error:notconnected";
      if (element.nodeName.toLowerCase() === "input") {
        const type = element.type.toLowerCase();
        const setValue = new Set(["color", "date", "time", "datetime-local", "month", "range", "week"]);
        const typeInto = new Set(["", "email", "number", "password", "search", "tel", "text", "url"]);
        if (!typeInto.has(type) && !setValue.has(type)) throw new Error('Input of type "' + type + '" cannot be filled');
        if (type === "number") {
          value = value.trim();
          if (Number.isNaN(Number(value))) throw new Error("Cannot type text into input[type=number]");
        }
        if (setValue.has(type)) {
          value = value.trim();
          element.focus();
          element.value = value;
          if (element.value !== value) throw new Error("Malformed value");
          element.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
          element.dispatchEvent(new Event("change", { bubbles: true }));
          return "done";
        }
      } else if (element.nodeName.toLowerCase() === "textarea") {
        // Nothing to check.
      } else if (!element.isContentEditable) {
        throw new Error("Element is not an <input>, <textarea> or [contenteditable] element");
      }
      if (element.nodeName.toLowerCase() === "input") {
        element.select();
        element.focus();
      } else if (element.nodeName.toLowerCase() === "textarea") {
        element.selectionStart = 0;
        element.selectionEnd = element.value.length;
        element.focus();
      } else {
        const range = element.ownerDocument.createRange();
        range.selectNodeContents(element);
        const selection = element.ownerDocument.defaultView.getSelection();
        if (selection) {
          selection.removeAllRanges();
          selection.addRange(range);
        }
        element.focus();
      }
      return "needsinput";
    },
    // Playwright's selectOption with value-or-label strings: the selected values, or
    // "error:optionsnotfound" while a wanted option is missing.
    selectOptions(node, wanted) {
      const element = retarget(node, "follow-label");
      if (!element) return "error:notconnected";
      if (element.nodeName.toLowerCase() !== "select") throw new Error("Element is not a <select> element");
      const options = [...element.options];
      const selected = [];
      let remaining = wanted.slice();
      for (const option of options) {
        const matches = (value) => value === option.value || value === option.label;
        if (!remaining.some(matches)) continue;
        selected.push(option);
        if (element.multiple) remaining = remaining.filter((value) => !matches(value));
        else { remaining = []; break; }
      }
      if (remaining.length) return "error:optionsnotfound";
      element.value = undefined;
      selected.forEach((option) => { option.selected = true; });
      element.dispatchEvent(new Event("input", { bubbles: true, composed: true }));
      element.dispatchEvent(new Event("change", { bubbles: true }));
      return selected.map((option) => option.value);
    },
    checked(element) {
      const target = retarget(element, "follow-label");
      return withCaches(() => getChecked(target, false));
    },
    accessibleName: (element, includeHidden) => withCaches(() => getAccessibleName(element, !!includeHidden)),
    role: (element) => getAriaRole(element),
    hiddenForAria: (element) => withCaches(() => isElementHiddenForAria(element)),
    isVisible: (element) => isElementVisible(element),
    elementText: (element) => elementText(new Map(), element).full,
  };

  Object.defineProperty(globalThis, KEY, { value: api, configurable: true, enumerable: false });
})();
