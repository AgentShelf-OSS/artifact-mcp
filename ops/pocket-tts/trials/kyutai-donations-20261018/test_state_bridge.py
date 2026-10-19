import sys
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    b = p.chromium.launch(executable_path="/usr/bin/google-chrome", args=["--no-sandbox","--autoplay-policy=no-user-gesture-required","--allow-file-access-from-files"])
    pg = b.new_page(); errs=[]; pg.on("pageerror", lambda e: errs.append(str(e)))
    pg.goto("file://" + sys.argv[1] + "/host-test.html"); pg.wait_for_timeout(2500)
    f = pg.frame_locator("#f")
    print("first card", f.locator("li.voice").first.get_attribute("data-c"), f.locator(".name").first.inner_text())
    f.locator("li.voice").nth(1).locator(".decide button[data-v=keep]").click()
    f.locator("li.voice").nth(2).locator("details.more summary").click()
    f.locator("li.voice").nth(2).locator("input[type=text]").fill("Warm: narrator, one")
    f.locator("li.voice").nth(2).locator("select").first.select_option("British")
    f.locator("li.voice").nth(3).locator(".play").first.click(); pg.wait_for_timeout(2500)
    frame = pg.frames[1]
    print("audio t", frame.evaluate("document.getElementById('player').currentTime"), "status", frame.evaluate("document.getElementById('status').textContent"))
    st = pg.evaluate("store.audition"); print("stored", {k: v for k, v in st["d"].items()})
    print("log", sorted(set(pg.evaluate("log"))), "errors", errs)
    b.close()
