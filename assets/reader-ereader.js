(function () {
  "use strict";
  try {
    var MAX_JSON_BYTES = 2 * 1024 * 1024;
    var bookNode = document.getElementById("book");
    if (!bookNode || new TextEncoder().encode(bookNode.textContent || "").length > MAX_JSON_BYTES) return;
    var book = JSON.parse(bookNode.textContent || "{}");
    if (!Array.isArray(book.chapters) || !book.chapters.length) return;
    var chapters = book.chapters;
    var normalize = function (value) { return String(value == null ? "" : value).replace(/\s+/g, " ").trim().toLowerCase(); };
    var allText = chapters.map(function (chapter) {
      return [chapter.part, chapter.title].concat((chapter.blocks || []).map(function (block) { return block && block.t !== "break" ? block.s : ""; })).join(" ");
    }).join("\n");
    var hash = 2166136261;
    for (var hashIndex = 0; hashIndex < allText.length; hashIndex++) { hash ^= allText.charCodeAt(hashIndex); hash = Math.imul(hash, 16777619); }
    var fingerprint = function () {
      var current = chapterFromDom && chapterFromDom();
      var visibleText = current ? [chapters[current.index].part, chapters[current.index].title].concat(current.paragraphs.map(function (paragraph) { return textOf(paragraph); })).join(" ") : "";
      var value = allText + "\n" + visibleText, nextHash = 2166136261;
      for (var i = 0; i < value.length; i++) { nextHash ^= value.charCodeAt(i); nextHash = Math.imul(nextHash, 16777619); }
      return (nextHash >>> 0).toString(16).padStart(8, "0") + ":" + value.length;
    };
    var visible = function (element) {
      if (!element || element.nodeType !== 1) return false;
      if (element.hidden || element.closest("[hidden], [aria-hidden='true']")) return false;
      var style = window.getComputedStyle(element);
      return style.display !== "none" && style.visibility !== "hidden" && style.visibility !== "collapse";
    };
    var textOf = function (element) { return String(element && element.innerText || element && element.textContent || "").replace(/\s+/g, " ").trim(); };
    var chapterFromDom = function () {
      var article = document.getElementById("article");
      var head = article && article.querySelector(".ch-head");
      var part = head && head.querySelector(".part");
      var title = head && head.querySelector("h1");
      if (!article || !head || !visible(article) || !visible(head) || !visible(part) || !visible(title)) return null;
      var partText = normalize(part.textContent), titleText = normalize(title.textContent);
      var paragraphs = Array.prototype.filter.call(article.querySelectorAll("p[data-p]"), visible);
      var first = normalize(paragraphs[0] && paragraphs[0].textContent);
      var index = chapters.findIndex(function (chapter) {
        if (normalize(chapter.part) !== partText || normalize(chapter.title) !== titleText) return false;
        var source = Array.isArray(chapter.blocks) && chapter.blocks.find(function (block) { return block && block.t !== "break" && normalize(block.s); });
        return !source || !first || normalize(source.s) === first;
      });
      return index < 0 ? null : { article: article, index: index, paragraphs: paragraphs, label: String(chapters[index].part || "") + " · " + String(chapters[index].title || ""), hasNext: index + 1 < chapters.length };
    };
    var scan = function () {
      var current = chapterFromDom();
      if (!current) return null;
      var groups = [];
      var header = current.article.querySelector(".ch-head");
      var headerText = textOf(header && header.querySelector(".part")) + " " + textOf(header && header.querySelector("h1"));
      if (headerText.trim()) groups.push({ element: header, text: headerText.trim() });
      current.paragraphs.forEach(function (paragraph) {
        var text = textOf(paragraph);
        if (text) groups.push({ element: paragraph, text: text });
      });
      return { groups: groups, truncated: false, chapter: { index: current.index, label: current.label, hasNext: current.hasNext } };
    };
    var next = function () {
      var current = chapterFromDom();
      if (!current || !current.hasNext) return false;
      var link = current.article.querySelector(".ch-nav .next[data-go]");
      if (!link || !visible(link)) return false;
      var raw = String(link.getAttribute("data-go") || "");
      if (!/^\d+$/.test(raw) || Number(raw) !== current.index + 1) return false;
      link.click();
      return true;
    };
    var seek = function (target) {
      if (!Number.isInteger(target) || target < 0 || target >= chapters.length) return false;
      var current = chapterFromDom();
      if (current && current.index === target) return true;
      var findTarget = function () { return Array.prototype.find.call(document.querySelectorAll(".toc [data-go]"), function (candidate) {
        return String(candidate.getAttribute("data-go")) === String(target) && visible(candidate);
      }); };
      var link = findTarget();
      if (!link) {
        var contents = document.querySelector("#t-contents, [data-panel='contents']");
        if (contents && visible(contents)) { contents.click(); link = findTarget(); }
      }
      if (!link) return false;
      link.click();
      return true;
    };
    var outline = function () { return { chapters: chapters.slice(0, 500).map(function (chapter, index) { return { index: index, label: (String(chapter.part || '') + ' · ' + String(chapter.title || '')).slice(0, 120) }; }) }; };
    window.__artifactEreader = Object.freeze({ scan: scan, next: next, seek: seek, outline: outline, fingerprint: fingerprint });
  } catch (_) {}
}());
