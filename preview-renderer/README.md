# artifact-preview

`artifact-preview` turns untrusted artifact HTML into a PNG preview. It is a separate service
with no access to artifact data. Treat every request as hostile.

It is one static Rust binary that starts Chrome for Testing `chrome-headless-shell` for each
render and drives it over the Chrome DevTools Protocol (CDP). The renderer is a separate Cargo
workspace in `preview-renderer/`.

The artifact-mcp server calls it through `src/integrations/preview.rs` when
`PREVIEW_RENDERER_URL` is set.

## HTTP contract

| Request | Response |
|---|---|
| `GET /health` | `200 {"status":"ok"}` |
| `POST /render` with JSON `{"html": "...", "width": 1200, "height": 630}` | `200`, `content-type: image/png` |
| `POST /render` without `authorization: Bearer <token>`, when a token is set | `401 {"error":"unauthorized"}`, `www-authenticate: Bearer` |
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

### Shared token

`RENDER_TOKEN` or `RENDER_TOKEN_FILE` sets an optional shared bearer token. It is defence in
depth: the network policy that lets only the artifact-mcp server reach the renderer stays the
main control.

* When a token is set, `POST /render` requires `authorization: Bearer <token>`. The scheme
  `Bearer` is case-sensitive, and the comparison takes constant time. A missing or wrong value
  gets `401 {"error":"unauthorized"}`. The check comes before the busy check and before the body
  is read, so a client without the token cannot hold the render slot or learn whether a render
  runs.
* `GET /health` needs no token.
* Without a token, the contract is unchanged, and an `authorization` header is ignored. So the
  server can start to send the token before the renderer requires it.
* The service never logs the token or puts it in an error message. A rejected request logs
  `[artifact-preview] rejected a render request without a valid token`.

The artifact-mcp server sends the same value from `PREVIEW_RENDERER_TOKEN` or
`PREVIEW_RENDERER_TOKEN_FILE` (see `docs/configuration.md`).

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `CHROME_HEADLESS_SHELL` | none (required) | Absolute path of the `chrome-headless-shell` executable. The service does not start without it. |
| `PORT` | `3000` | TCP port. The service listens on `0.0.0.0`. |
| `RENDER_TIMEOUT_MS` | `7000` | Wall-clock limit for one render: launch, load and screenshot. Keep it below the client's 8 s timeout. |
| `MAX_BODY_BYTES` | `5242880` | Request body limit. |
| `TMPDIR` | `/tmp` | Where the service creates the per-render directories. |
| `RENDER_TOKEN` | unset | The shared bearer token that `POST /render` then requires (see [Shared token](#shared-token)). 32 to 4096 printable ASCII characters without spaces. Empty means unset. |
| `RENDER_TOKEN_FILE` | unset | Path of a file that holds the token instead. The service reads it once at startup and removes one trailing newline. Setting both variables is an error. |
| `CHROME_SANDBOX` | off | `1` or `true` keeps Chromium's own sandbox: the browser starts without `--no-sandbox`, and every other flag stays the same. `0`, `false`, empty or unset mean off. Any other value stops the service at startup. It needs unprivileged user namespaces and a user other than root; see [Chromium sandbox](#chromium-sandbox-chrome_sandbox). |
| `LANG`, `LANGUAGE`, `LC_ALL`, `TZ` | unset | Passed to the browser if set. The browser gets no other variable from the service. |

## Commands

```sh
artifact-preview               # serve
artifact-preview healthcheck   # exit 0 if GET http://127.0.0.1:$PORT/health returns 200, else 1
artifact-preview smoke         # render a test page through $PREVIEW_RENDERER_URL (default http://127.0.0.1:3000)
```

`healthcheck` lets a container healthcheck run without curl. `smoke` checks the status, the content
type, the PNG signature and the 2400×1260 size.
It reads `RENDER_TOKEN` or `RENDER_TOKEN_FILE` with the same rules as the service and sends the
token when one is set.

## Isolation model

The container or the systemd unit is the security boundary. By default Chromium runs with
`--no-sandbox`, as it did under Playwright (`chromiumSandbox: false`), because its sandbox needs
unprivileged user namespaces (`chrome-headless-shell` ships no setuid helper). A page that
exploits the renderer then gets the service user inside that boundary. Where user namespaces are
available, `CHROME_SANDBOX=1` adds Chromium's own sandbox inside the boundary (see
[Chromium sandbox](#chromium-sandbox-chrome_sandbox)). The service adds these layers.

### Chromium sandbox (`CHROME_SANDBOX`)

With `CHROME_SANDBOX=1`, the service leaves `--no-sandbox` out of the launch line and changes
nothing else. Chromium then starts each renderer process in new user, PID and network namespaces
and under its seccomp-bpf system call filter. A page that exploits a renderer must then also
escape that sandbox to reach the service user.

Requirements:

* The service user can create user namespaces. In a Proxmox LXC container, set
  `features: nesting=1`. On Ubuntu 23.10 and later, AppArmor restricts unprivileged user
  namespaces (`kernel.apparmor_restrict_unprivileged_userns`). A quick check as the service user:
  `unshare --user --map-root-user --pid --net --fork true` must exit 0.
* The service does not run as root. Chromium refuses its sandbox for root, and every render then
  fails.
* The systemd unit has no `RestrictNamespaces=` line. That setting blocks the namespaces.

If the sandbox cannot start, Chromium exits at launch: every render fails with
`500 {"error":"render failed"}`, and the log shows
`[artifact-preview] render failed: browser closed the devtools pipe`. Set `CHROME_SANDBOX=0` (or
remove it) to go back.

To check that the sandbox is active, compare `/proc/<pid>/status` of a renderer process
(`--type=renderer` in its command line) with that of the service while a render runs. With the
sandbox, the renderer shows `Seccomp: 2` and a higher `Seccomp_filters` count than the service:
Chromium's filter is added to any filter the service already has. Without the sandbox, the
renderer shows the same values as the service: `Seccomp: 0`, or the filters that the container
or a systemd setting such as `LockPersonality=` or `RestrictSUIDSGID=` gives the whole service.
`tests/sandbox.rs` makes this check.

The Compose service leaves `CHROME_SANDBOX` unset. The sandbox has not been tested in the
container. Whether a container can create user namespaces depends on the container runtime and
the host: Docker's default seccomp profile refuses the `clone` namespace flags to a container
without `CAP_SYS_ADMIN`, and AppArmor can refuse them on Ubuntu hosts. The supplied
`seccomp_profile.json` itself allows them (its first rule).

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
arguments of `defaultArgs()`), with `--no-sandbox` (left out when `CHROME_SANDBOX` is on),
`--no-startup-window`, `--user-data-dir` and `--remote-debugging-pipe`. The service drops no other
Playwright switch.

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

By default the browser runs without its sandbox, so browser security fixes matter here. Keep the
pin current.

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

The crate is its own workspace. It is not a member of the root workspace. The workspace has a
second member, `cdp/` (`artifact-cdp`): the DevTools pipe transport and the process-group
launcher, with the crate's only `unsafe` code (`cdp/src/sys.rs`). The browser test harness in
`../browser-tests` uses the same crate. The renderer chooses its launch policy in
`src/browser.rs`; a unit test there pins the production launch line byte for byte.

```sh
cd preview-renderer
export CHROME_HEADLESS_SHELL=/path/to/chrome-headless-shell-linux64/chrome-headless-shell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests that need a browser are skipped (with a message) when `CHROME_HEADLESS_SHELL` is unset.
When `CI` is set, they fail instead, so a CI job cannot pass by skipping them. The tests with
`CHROME_SANDBOX` on follow the same rule when the environment cannot run Chromium's sandbox: as
root, or without unprivileged user namespaces. To run the whole suite as root on a development
host, map root to another user id: `unshare --user --map-user=1000 --map-group=1000 cargo test`.
The suite covers:

* `cdp/` unit tests: CDP framing, `/proc` parsing, launch switches per network policy, private
  temporary directories;
* `src/` unit tests: JavaScript number rules, configuration, clip maths, the pinned launch
  line with `CHROME_SANDBOX` off and on;
* `src/tests.rs`: 37 network vectors per canary address, run with all defences, with each
  layer alone, and with none (the control). Also checks, with `CHROME_SANDBOX` off and on, that
  every browser process stays in the process group and that none survives;
* `tests/contract.rs`: the HTTP contract against the running binary, including the shared
  token: `401` for a missing, wrong or wrong-case value, `200` for the right one, an open
  `/health`, `smoke` with the token, and startup errors that never echo the token;
* `tests/isolation.rs`: the vectors against the binary, and `data:` images rendering, checked by
  pixel colour;
* `tests/lifecycle.rs`: hostile pages hitting the deadline, cleanup after a client disconnect,
  `SIGTERM` and `SIGKILL` of the service, and 20 sequential renders with no process,
  directory or file descriptor leak;
* `tests/sandbox.rs`: with `CHROME_SANDBOX=1`, a renderer process has `Seccomp: 2` and a seccomp
  filter of its own while a render runs; with `CHROME_SANDBOX=0`, no renderer has one. An invalid
  value stops the service.

Lifecycle tests scan for processes started from `CHROME_HEADLESS_SHELL`. Do not run another
browser from the same path at the same time.
