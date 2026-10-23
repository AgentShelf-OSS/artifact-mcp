# artifact-preview

`artifact-preview` turns untrusted artifact HTML into a PNG preview. It is a separate service
with no access to artifact data. Treat every request as hostile.

It is one static Rust binary that starts Chrome for Testing `chrome-headless-shell` for each
render and drives it over the Chrome DevTools Protocol (CDP). The service needs no Node.js and no
Playwright. It replaces the Node and Playwright renderer (`server.js`, `worker.js`) and keeps that
renderer's HTTP contract.

The artifact-mcp server calls it through `src/integrations/preview.rs` when
`PREVIEW_RENDERER_URL` is set.

## HTTP contract

| Request | Response |
|---|---|
| `GET /health` | `200 {"status":"ok"}` |
| `POST /render` with JSON `{"html": "...", "width": 1200, "height": 630}` | `200`, `content-type: image/png` |
| `POST /render` while another render runs | `503 {"error":"renderer busy"}` |
| Body larger than `MAX_BODY_BYTES` | `413 {"error":"request too large"}` |
| Body that is not JSON | `400 {"error":"invalid JSON"}` |
| `html` that is not a string | `400 {"error":"html must be a string"}` |
| Browser error or deadline | `500 {"error":"render failed"}` |
| Body not received within `RENDER_TIMEOUT_MS + 2000` ms | `408 {"error":"request timeout"}` |
| Any other method or path (exact match, no query string) | `404 {"error":"not found"}` |

Every response has `cache-control: no-store` and `x-content-type-options: nosniff`.

`width` and `height` are CSS pixels. Defaults are 1200×630, maximums are 1920×1080. The service
renders at device scale factor 2, so the PNG is `2·width` × `2·height` pixels (2400×1260 by
default). Values follow JavaScript `Number()` rules, as before: `"800"`, `8e2`, `"0x320"` and
`[800]` all mean 800. A value that is not a positive safe integer gives the default.

The service renders one page at a time. It does not queue.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `CHROME_HEADLESS_SHELL` | none (required) | Absolute path of the `chrome-headless-shell` executable. The service does not start without it. |
| `PORT` | `3000` | TCP port. The service listens on `0.0.0.0`. |
| `RENDER_TIMEOUT_MS` | `7000` | Wall-clock limit for one render: launch, load and screenshot. Keep it below the client's 8 s timeout. |
| `MAX_BODY_BYTES` | `5242880` | Request body limit. |
| `TMPDIR` | `/tmp` | Where the service creates the per-render directories. |
| `LANG`, `LANGUAGE`, `LC_ALL`, `TZ` | unset | Passed to the browser if set. The browser gets no other variable from the service. |

## Commands

```sh
artifact-preview               # serve
artifact-preview healthcheck   # exit 0 if GET http://127.0.0.1:$PORT/health returns 200, else 1
artifact-preview smoke         # render a test page through $PREVIEW_RENDERER_URL (default http://127.0.0.1:3000)
```

`healthcheck` lets a container healthcheck run without curl or Node. `smoke` replaces
`npm run smoke`. It checks the status, the content type, the PNG signature and the 2400×1260 size.

## Isolation model

The container or the systemd unit is the security boundary. Chromium runs with `--no-sandbox`,
as it did under Playwright (`chromiumSandbox: false`), because its sandbox needs user namespaces
or a setuid helper that the hardened deployment removes. A page that exploits the renderer gets
the service user inside that boundary. The service adds these layers.

### One process tree per render

* Each render starts a new browser as the leader of a new process group (`setpgid`). Tests show
  that every Chromium process (zygotes, renderers, GPU and network utility processes) stays in
  that group.
* When the screenshot is done, when the deadline passes, or when the service gets `SIGTERM`, the
  service sends `SIGKILL` to the whole group, reaps the leader, and waits until no member of the
  group still runs.
* The browser has `PR_SET_PDEATHSIG=SIGKILL`. If the service dies, even by `SIGKILL` or the OOM
  killer, the kernel kills the browser and its children exit.
* Each render gets a new mode-0700 directory `$TMPDIR/artifact-preview-*`. It holds the profile
  and is the browser's `HOME` and `TMPDIR`. The service deletes it after the group exits.
* The browser environment is cleared. Only `HOME`, `TMPDIR` and the locale and time zone
  variables listed above are set.
* The service refuses to run as PID 1, because killed browser processes are re-parented to PID 1,
  which must reap them. The image uses `tini`. Compose also sets `init: true`.

### DevTools pipe, not a port

The service starts the browser with `--remote-debugging-pipe`. It writes NUL-terminated JSON
commands to fd 3 and reads replies and events from fd 4. No other process can reach the protocol:
there is no TCP debugging port and no WebSocket. Each incoming message is capped at 64 MiB. The
largest real message is a full-size screenshot, about 45 MB of base64.

### Page lockdown

The CDP sequence mirrors what Playwright 1.60 did for the Node renderer:

* a new browser context; downloads denied;
* viewport `width`×`height` at device scale factor 2; Playwright's default fonts, media
  features and focus emulation;
* `Network.emulateNetworkConditions` offline, with the Network domain enabled;
* `Fetch` interception of every URL: `data:` subresources continue, everything else fails with
  `BlockedByClient`;
* `navigator.serviceWorker.register` replaced (on the prototype, so a page cannot restore it);
* JavaScript dialogs dismissed;
* the HTML loaded the way `page.setContent` does it: `document.open/write/close` in an isolated
  JavaScript world, with the HTML passed as a protocol argument and never put into script
  source. Then the service waits for the main frame's `load` event;
* Playwright's screenshot preparation: finite animations finished, infinite ones cancelled, caret
  hidden. Then the service waits for `document.fonts.ready` and calls `Page.captureScreenshot`
  with the viewport clip. It retries the capture up to twice, one animation frame apart, if
  Chromium has no frame ready yet. Chrome 148 needed this about once in 100 renders.

The new renderer is stricter than the Node renderer in these ways:

* Request interception is enabled on the browser target as well as on the page. Every new page
  is paused before it runs. Popups and out-of-process iframes get the page's protections before
  they resume, and the service then closes each popup.
* Dedicated, shared and service workers stay paused before their first statement and never run.
  Playwright resumed dedicated workers.
* Launch flags send all browser traffic to an unreachable proxy and disable name resolution:

| Flag | Why |
|---|---|
| `--proxy-server=127.0.0.1:9` | Sends all HTTP, HTTPS and WebSocket traffic, including the requests that DevTools interception never sees, to a port where nothing listens. |
| `--proxy-bypass-list=<-loopback>` | Removes Chromium's implicit proxy bypass for loopback, so `127.0.0.1`, `::1` and `localhost` also go to the dead proxy. |
| `--host-resolver-rules=MAP * ~NOTFOUND` | Resolves no host name: no DNS queries (a data exfiltration channel) and no direct connection by name. |
| `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` | WebRTC may only use UDP through a proxy, and the only proxy is dead, so STUN and TURN send nothing. |

Every other switch is Playwright's headless list (`chromiumSwitches()` plus the headless
arguments of `defaultArgs()`), with `--no-sandbox`, `--no-startup-window`, `--user-data-dir` and
`--remote-debugging-pipe`. The service drops no Playwright switch.

Why the flags matter: with only interception and offline emulation, which is the Node renderer's
in-browser protection, tests show that WebRTC TURN-over-TCP connections and speculation-rules
prefetch and prerender requests still reach a canary on loopback and on a LAN address. Against
the real Node renderer, a speculation-rules prefetch reached a loopback canary. The flags alone
block all tested vectors. All layers together block all tested vectors on loopback and on a LAN
IP. See `src/tests.rs` and `tests/isolation.rs`.

## Browser version

The image pins Chrome for Testing **156.0.8078.47** (Stable on 2026-10-22),
`chrome-headless-shell-linux64.zip`, SHA-256
`0d5acc62c3e213f8c87d3c298d66ffcf9a236d12ea4b9f85318bf85dbc279daa`.

To upgrade:

1. Find the version at <https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions.json>.
2. Download the archive and compute its SHA-256:

   ```sh
   curl -fsSLO https://storage.googleapis.com/chrome-for-testing-public/<version>/linux64/chrome-headless-shell-linux64.zip
   sha256sum chrome-headless-shell-linux64.zip
   ```

3. Change `CHROME_VERSION` and `CHROME_SHA256` together in the `Dockerfile` and in `DEPLOY-CT222.md`.
4. Run the full test suite with `CHROME_HEADLESS_SHELL` pointing at the new build.

The browser runs without its sandbox, so browser security fixes matter here. Keep the pin
current.

## Container

The `Dockerfile` has three stages:

1. Build the static binary (`x86_64-unknown-linux-musl`) in the same digest-pinned Rust image
   as the server.
2. Download and verify the browser archive.
3. Assemble `ubuntu:24.04` (digest-pinned) with the libraries the browser links against
   (from `readelf`, not the full Chrome `deb.deps`), the fonts that CT222 and the old Playwright
   image ship, `tini`, the browser in `/opt/chrome-headless-shell` and the binary in
   `/usr/local/bin/artifact-preview`. The image runs as user 65532.

Compose (`docker-compose.yml`, profile `preview`) keeps the hardening: read-only root, a 256 MB
`/tmp` tmpfs, `cap_drop: ALL`, `no-new-privileges`, the seccomp profile, `pids_limit: 256`,
`mem_limit: 768m`, and an internal-only network. The healthcheck is
`/usr/local/bin/artifact-preview healthcheck`.

`seccomp_profile.json` is unchanged. The Rust binary uses only system calls the profile allows
(`clone`, `pipe2`, `dup2`, `fcntl`, `prctl`, `setpgid`, `kill`, `wait4`, epoll and sockets). The
browser links against glibc 2.39, which tries `clone3` (and other newer calls) first. The profile
does not list them. The container runtime then answers `ENOSYS`, not `EPERM`, for any system call
newer than every call in the profile, and glibc falls back to `clone`. This was checked with
podman 5.4.2 (crun): `clone3` returned `ENOSYS`, and the image passed `healthcheck`, `smoke` and
the network canary with this profile, `--read-only`, a `/tmp` tmpfs, `--cap-drop ALL`,
`no-new-privileges`, `--pids-limit 256` and `--memory 768m`. runc, which Docker uses, adds the
same `ENOSYS` stub, but the image has not been run under Docker itself.

## Development

The crate is its own workspace. It is not a member of the root workspace.

```sh
cd preview-renderer
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests that need a browser are skipped (with a message) when `CHROME_HEADLESS_SHELL` is unset.
When `CI` is set, they fail instead, so a CI job cannot pass by skipping them. The suite covers:

* `src/` unit tests: JavaScript number rules, configuration, CDP framing, clip maths,
  `/proc` parsing;
* `src/tests.rs`: 37 network vectors per canary address, run with all defences, with each
  layer alone, and with none (the control). Also checks that every browser process stays in the
  process group and that none survives;
* `tests/contract.rs`: the HTTP contract against the running binary;
* `tests/isolation.rs`: the vectors against the binary, and `data:` images rendering, checked by
  pixel colour;
* `tests/lifecycle.rs`: hostile pages hitting the deadline, cleanup after a client disconnect,
  `SIGTERM` and `SIGKILL` of the service, and 20 sequential renders with no process,
  directory or file descriptor leak.

Lifecycle tests scan for processes started from `CHROME_HEADLESS_SHELL`. Do not run another
browser from the same path at the same time.
