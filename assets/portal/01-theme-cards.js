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

  function revealCardPreview(card) {
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
  }
  var cards = Array.prototype.slice.call(document.querySelectorAll(".card"));
  cards.forEach(function (card, index) {
    card.style.animationDelay = (index % 12 * 35) + "ms";
    revealCardPreview(card);
  });

