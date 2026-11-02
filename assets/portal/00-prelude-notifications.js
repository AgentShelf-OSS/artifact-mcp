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

