# Runbook: move CT222 from the Node renderer to the Rust renderer

Do the steps in order and stop at the first failed check.

**Run log.** Steps 1–6 ran on 2026-10-23 with a local build of `a34964b` (sha256 `47af6293…`); CT222
has served previews from the Rust renderer since 16:17 UTC that day. The same day CT222 got
`features: nesting=1` (for the Chrome sandbox), 768 MB of memory, and `MemoryMax=640M` in the
drop-in. Step 8 waits for a one-week soak (`ct222_soak_check.py` in the operator's migration folder).
From v1.28.0, install the attested release binary as described in step 1 instead of a local build.

## Current state (read on 2026-10-22)

| Item | Value |
|---|---|
| Container | CT222 `artifact-preview`, Ubuntu 24.04.5, unprivileged LXC, 2 cores, 1024 MB RAM, no swap |
| Features | `nesting=1`. The service user can create user namespaces, and `chrome-headless-shell` starts without `--no-sandbox` there (checked after the feature was set). See [Chrome sandbox](#chrome-sandbox). |
| Address | `192.168.0.222:3000`. nftables accepts port 3000 only from CT220 (`192.168.0.220`) and drops all output except loopback and established connections. **CT222 cannot download anything.** |
| Unit | `/etc/systemd/system/artifact-preview.service`, `User=artifact-preview` (uid 999), `ExecStart=/usr/bin/node /opt/artifact-preview/server.js` |
| Unit environment | `NODE_ENV=production`, `PORT=3000`, `HOME=/var/lib/artifact-preview`, `PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright` |
| Unit hardening | `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome`, `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `RestrictSUIDSGID`, `LockPersonality`, empty `CapabilityBoundingSet` and `AmbientCapabilities`, `TasksMax=256`, `MemoryMax=768M`, `UMask=0077`, `KillMode=mixed` |
| Node | `nodejs 22.23.4-1nodesource1`, from `/etc/apt/sources.list.d/nodesource.list` with key `/etc/apt/keyrings/nodesource.gpg` |
| Browser | Playwright 1.61.0, `/opt/ms-playwright/chromium_headless_shell-1228`, which is Chrome for Testing 149.0.7827.55. `/opt/ms-playwright` is 642 MB, including the unused full `chromium-1228`. |
| Memory | `MemoryPeak` 713 621 504 bytes (680 MiB) of the 768 MiB limit |
| Tools | `python3`, `file`, `runuser` and `curl` are installed. `unzip` is not. |

The new renderer needs the shared libraries and fonts that Playwright already installed on CT222.
It needs no network access.

## Values for this release

```sh
CHROME_VERSION=156.0.8078.47
CHROME_SHA256=0d5acc62c3e213f8c87d3c298d66ffcf9a236d12ea4b9f85318bf85dbc279daa
```

They must match `CHROME_VERSION` and `CHROME_SHA256` in `preview-renderer/Dockerfile`.

## 1. Prepare the files on pve1

Run these steps on pve1, which has network access, in an empty working directory. Set `TAG` to the
release that you install.

1. Download the attested renderer binary and the release checksums, and verify both. The flags
   are the ones the server release check uses: the attestation must come from the release
   workflow, for this tag and its commit.

   ```sh
   TAG=vX.Y.Z
   REPO=AgentShelf-OSS/artifact-mcp
   COMMIT="$(gh api "repos/$REPO/commits/$TAG" --jq .sha)"
   gh release download "$TAG" --repo "$REPO" -p artifact-preview -p SHA256SUMS
   sha256sum --check --strict --ignore-missing SHA256SUMS          # artifact-preview: OK
   gh attestation verify artifact-preview --repo "$REPO" \
     --cert-identity "https://github.com/$REPO/.github/workflows/release-provenance.yml@refs/tags/$TAG" \
     --cert-oidc-issuer https://token.actions.githubusercontent.com \
     --source-ref "refs/tags/$TAG" --source-digest "$COMMIT"
   ```

   Stop if a command fails. `sha256sum` fails with `no file was verified` when `SHA256SUMS` has
   no `artifact-preview` line. Releases before v1.28.0 have no `artifact-preview` asset.

2. Run `file artifact-preview`. It must say `static-pie linked` or `statically linked`. Record
   `sha256sum artifact-preview`.
3. Download the browser and verify it:

   ```sh
   curl -fsSLO "https://storage.googleapis.com/chrome-for-testing-public/${CHROME_VERSION}/linux64/chrome-headless-shell-linux64.zip"
   echo "${CHROME_SHA256}  chrome-headless-shell-linux64.zip" | sha256sum -c -
   ```

   Stop if the check does not print `OK`.

Fallback, only for a release without the `artifact-preview` asset or for a test build: build the
static binary from the release commit. Such a binary has no attestation, so record its digest
with the change.

```sh
cargo build --release --locked --target x86_64-unknown-linux-musl \
  --manifest-path <repo>/preview-renderer/Cargo.toml
cp "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/artifact-preview" .
```

Or copy it out of the release image:

```sh
docker create --name ap-extract <image>
docker cp ap-extract:/usr/local/bin/artifact-preview .
docker rm ap-extract
```

## 2. Copy the files into CT222

On pve1:

```sh
pct push 222 artifact-preview /root/artifact-preview.new --perms 0755
pct push 222 chrome-headless-shell-linux64.zip /root/chrome-headless-shell-linux64.zip
```

## 3. Install, without touching the running service

In CT222 (`pct enter 222`):

```sh
set -eu
CHROME_VERSION=156.0.8078.47
CHROME_SHA256=0d5acc62c3e213f8c87d3c298d66ffcf9a236d12ea4b9f85318bf85dbc279daa

# Verify again inside the container, then unpack to a versioned directory.
echo "${CHROME_SHA256}  /root/chrome-headless-shell-linux64.zip" | sha256sum -c -
DIR=/opt/chrome-headless-shell-${CHROME_VERSION}
mkdir "$DIR"
python3 -m zipfile -e /root/chrome-headless-shell-linux64.zip "$DIR"
mv "$DIR"/chrome-headless-shell-linux64/* "$DIR"/ && rmdir "$DIR"/chrome-headless-shell-linux64
# python's zipfile drops the executable bits; restore them for the binary and its libraries.
chmod 0755 "$DIR"/chrome-headless-shell "$DIR"/*.so "$DIR"/*.so.1
chown -R root:root "$DIR" && chmod -R a+rX,go-w "$DIR"
# A stable path for the unit; an upgrade only moves this link.
ln -sfn "$DIR" /opt/chrome-headless-shell

# Every shared library must resolve. This must print nothing.
ldd /opt/chrome-headless-shell/chrome-headless-shell | grep 'not found' || true

install -o root -g root -m 0755 /root/artifact-preview.new /usr/local/bin/artifact-preview
rm /root/artifact-preview.new /root/chrome-headless-shell-linux64.zip
```

If `ldd` reports a missing library, install its Ubuntu package (the `t64` names, for example
`libasound2t64`). The full package list is in the `Dockerfile`. CT222 has no network, so fetch
the `.deb` files on pve1 and `pct push` them.

## 4. Test as the service user on a spare port

Port 3999 is reachable only from inside CT222, which is all this test needs.

```sh
T=$(runuser -u artifact-preview -- mktemp -d)
runuser -u artifact-preview -- env -i PORT=3999 TMPDIR="$T" \
  CHROME_HEADLESS_SHELL=/opt/chrome-headless-shell/chrome-headless-shell \
  /usr/local/bin/artifact-preview &
sleep 1
PORT=3999 /usr/local/bin/artifact-preview healthcheck && echo healthy
PREVIEW_RENDERER_URL=http://127.0.0.1:3999 /usr/local/bin/artifact-preview smoke
kill %1; wait
ls -A "$T"; rm -rf "$T"
```

Expected: `healthy`, then `preview renderer smoke test passed (… bytes)`. `ls -A` prints nothing:
no `artifact-preview-*` directory is left.

## 5. Switch the unit

Use a drop-in, so the rollback only removes one file. Create
`/etc/systemd/system/artifact-preview.service.d/10-rust-renderer.conf`:

```ini
[Service]
# Clear the Node environment, then set the renderer's.
Environment=
Environment=PORT=3000
Environment=RENDER_TIMEOUT_MS=7000
Environment=CHROME_HEADLESS_SHELL=/opt/chrome-headless-shell/chrome-headless-shell
WorkingDirectory=/
ExecStart=
ExecStart=/usr/local/bin/artifact-preview
```

Keep every hardening line of the main unit. They all still apply:

* `PrivateTmp=true` gives the renderer a private `/tmp` for its per-render directories.
* `ProtectSystem=strict` is fine: the renderer writes only below `/tmp`.
* `KillMode=mixed`: on stop, the renderer gets `SIGTERM`, kills the browser's process group and
  exits. systemd then kills anything left in the cgroup.
* `TasksMax=256` and `MemoryMax=768M` stay. On pve1, one render peaked at about 450 MiB RSS for
  the service and browser tree, against about 670 MiB for the Node renderer.

Do not add these:

* `MemoryDenyWriteExecute=yes`: V8's JIT needs writable and executable memory.
* `PrivateNetwork=yes`: the service must accept connections from CT220.
* `RestrictNamespaces=` (any value): it blocks the user, PID and network namespaces that
  Chromium's sandbox needs (`CHROME_SANDBOX=1`, see [Chrome sandbox](#chrome-sandbox)).

These should work, but none was tested in an unprivileged LXC: `SystemCallArchitectures=native`,
`ProtectClock=yes`, `ProtectHostname=yes`, `RestrictRealtime=yes`,
`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`. Add them one at a time, and run
step 6 after each.

Then:

```sh
systemctl daemon-reload
systemctl restart artifact-preview
```

## 6. Verify

In CT222:

```sh
systemctl is-active artifact-preview                    # active
journalctl -u artifact-preview -n 5 --no-pager          # [artifact-preview] listening on 0.0.0.0:3000
systemctl show artifact-preview -p ExecStart | grep -c /usr/local/bin/artifact-preview   # 1
PORT=3000 /usr/local/bin/artifact-preview healthcheck && echo healthy
PREVIEW_RENDERER_URL=http://127.0.0.1:3000 /usr/local/bin/artifact-preview smoke
pgrep -u artifact-preview -a chrome-headless || echo "no browser while idle"
nft list chain inet filter output                        # policy drop, unchanged
```

From the server side, regenerate one artifact preview (MCP tool `regenerate_artifact_preview`)
and check that the gallery thumbnail appears. A direct check from CT220:

```sh
curl -s -o /tmp/ct222.png -w '%{http_code} %{content_type}\n' \
  -H 'content-type: application/json' -d '{"html":"<h1>ct222</h1>"}' \
  http://192.168.0.222:3000/render                       # 200 image/png
```

When the renderer has the shared token ([Shared token](#shared-token)), this `curl` gets `401`,
and the `smoke` commands above need `RENDER_TOKEN_FILE=/etc/artifact-preview/render-token` in
front of them. Use the `regenerate_artifact_preview` check from CT220 instead.

After a few renders, back in CT222:

```sh
systemctl show artifact-preview -p MemoryPeak -p TasksCurrent
ls /tmp/systemd-private-*-artifact-preview.service-*/tmp/   # no artifact-preview-* directories while idle
```

The journal records each failed render with its cause (`render timed out`,
`render failed: …`, `retrying screenshot: …`). It never logs page content.

## 7. Roll back to the Node service

The Node files, `/opt/ms-playwright` and the `nodejs` package stay untouched until step 8.

```sh
rm /etc/systemd/system/artifact-preview.service.d/10-rust-renderer.conf
rmdir /etc/systemd/system/artifact-preview.service.d 2>/dev/null || true
systemctl daemon-reload
systemctl restart artifact-preview
systemctl show artifact-preview -p ExecStart | grep -c /usr/bin/node    # 1
curl -fsS http://127.0.0.1:3000/health                                  # {"status":"ok"}
PREVIEW_RENDERER_URL=http://127.0.0.1:3000 /usr/local/bin/artifact-preview smoke
```

The `smoke` command works against either renderer.

## 8. Remove Node after the soak period

Wait until the Rust renderer has run without a rollback for at least a week. Then, in CT222:

1. Fold the drop-in into the unit. Write `/etc/systemd/system/artifact-preview.service` as:

   ```ini
   [Unit]
   Description=Isolated Artifact HTML preview renderer (Rust, chrome-headless-shell)
   After=network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   User=artifact-preview
   Group=artifact-preview
   WorkingDirectory=/
   Environment=PORT=3000
   Environment=RENDER_TIMEOUT_MS=7000
   Environment=CHROME_HEADLESS_SHELL=/opt/chrome-headless-shell/chrome-headless-shell
   ExecStart=/usr/local/bin/artifact-preview
   Restart=on-failure
   RestartSec=5s
   TimeoutStopSec=15s
   KillMode=mixed

   NoNewPrivileges=true
   PrivateTmp=true
   ProtectSystem=strict
   ProtectHome=true
   ProtectKernelTunables=true
   ProtectKernelModules=true
   ProtectKernelLogs=true
   ProtectControlGroups=true
   RestrictSUIDSGID=true
   LockPersonality=true
   CapabilityBoundingSet=
   AmbientCapabilities=
   TasksMax=256
   MemoryMax=768M
   UMask=0077

   [Install]
   WantedBy=multi-user.target
   ```

   Then remove the drop-in, run `systemctl daemon-reload` and `systemctl restart artifact-preview`,
   and repeat step 6.

2. Remove Node, Playwright and the old browser. Removal needs no network:

   ```sh
   apt-get purge -y nodejs
   rm /etc/apt/sources.list.d/nodesource.list /etc/apt/keyrings/nodesource.gpg
   rm -rf /opt/artifact-preview /opt/ms-playwright
   ```

   Keep the shared libraries and the fonts (`fonts-liberation`, `fonts-noto-color-emoji`,
   `fonts-freefont-ttf`, `fonts-ipafont-gothic`, `fonts-wqy-zenhei`, `fonts-tlwg-loma-otf`,
   `fonts-unifont`); `chrome-headless-shell` uses them. Do not run `apt-get autoremove` without
   reading its list first.

3. Repeat step 6. `command -v node` must print nothing.
4. Update `homelab-automation`: `docs/inventory/live-topology.md` says "Native Playwright", and
   `docs/ops/native-edge-services-migration-2026-08-01.md` lists Node and Playwright Chromium.

## Chrome sandbox

`CHROME_SANDBOX=1` keeps Chromium's own sandbox: the renderer starts the browser without
`--no-sandbox`, and every other flag stays the same. Each Chromium renderer process then runs in
new user, PID and network namespaces under a seccomp-bpf filter, inside the unit's hardening.
CT222 needs `features: nesting=1` for this (it has it). Turn the sandbox on after step 6 passes.

1. In CT222, check that the service user can create the namespaces. This must print `ok`:

   ```sh
   runuser -u artifact-preview -- unshare --user --map-root-user --pid --net --fork true && echo ok
   ```

2. Run step 4 again with `CHROME_SANDBOX=1` added to the `env -i` line. `smoke` must pass.
3. Add one line to the `[Service]` section of the drop-in (or of the unit, after step 8):

   ```ini
   Environment=CHROME_SANDBOX=1
   ```

   Do not add `RestrictNamespaces=`. Then run `systemctl daemon-reload`,
   `systemctl restart artifact-preview`, and step 6.
4. Check that the sandbox is active. While a slow page renders, every renderer process must show
   `Seccomp: 2` and more `Seccomp_filters` than the service's main process. `LockPersonality` and
   `RestrictSUIDSGID` already give the whole service a seccomp filter, so `Seccomp: 2` alone
   proves nothing here; compare the counts.

   ```sh
   main=$(systemctl show -p MainPID --value artifact-preview)
   curl -s -o /dev/null -w '%{http_code}\n' -H 'content-type: application/json' \
     -d '{"html":"<script>const t = Date.now() + 2000; while (Date.now() < t) {}</script>"}' \
     http://127.0.0.1:3000/render &
   sleep 1
   grep -H '^Seccomp' /proc/"$main"/status
   for pid in $(pgrep -u artifact-preview -f -- --type=renderer); do grep -H '^Seccomp' /proc/"$pid"/status; done
   wait                                                     # 200
   ```

   When the shared token is on (see [Shared token](#shared-token)), add
   `-H "authorization: Bearer $(cat /etc/artifact-preview/render-token)"` to the `curl` line.
5. After a few renders, compare `systemctl show artifact-preview -p MemoryPeak` with the value
   before the change. It must stay below `MemoryMax=768M`.

If every render fails after the change, the journal shows
`render failed: browser closed the devtools pipe`: Chromium could not start its sandbox. Roll
back: remove the `Environment=CHROME_SANDBOX=1` line, then run `systemctl daemon-reload` and
`systemctl restart artifact-preview`, and repeat step 6.

## Shared token

The server on CT220 and the renderer on CT222 can share a bearer token. It is defence in depth:
nftables, which accepts port 3000 only from CT220, stays the main control. With the token, a
process that reaches port 3000 in another way still cannot render.

| Side | Setting | Value comes from |
|---|---|---|
| CT220, `artifact-mcp` | `PREVIEW_RENDERER_TOKEN` | Infisical secret `/ARTIFACT_PREVIEW_RENDERER_TOKEN`, bound by the launcher at each start |
| CT222, `artifact-preview` | `RENDER_TOKEN_FILE` | the same value in the root-only file `/etc/artifact-preview/render-token`, given to the unit with `LoadCredential=` |

The token is 32 to 4096 printable ASCII characters without spaces; use 64 hexadecimal
characters. The file holds the token and at most one trailing newline. Both services check the
value at startup, refuse an invalid one, and never log it.

The token needs artifact-mcp 1.28.0 or later on CT220 and the `artifact-preview` binary from the
same release on CT222 (steps 1 to 6). CT222 has no network, so it cannot fetch the secret itself.
A renderer without a token ignores the `authorization` header (the Node renderer did too), so set
CT220 first and CT222 second. Then no render fails during the rollout.

1. On pve1, create the token with `openssl rand -hex 32` and store it in Infisical as
   `/ARTIFACT_PREVIEW_RENDERER_TOKEN` with `homelab-secrets add` and the operator profile, which
   reads the value from a private prompt. Do not put the value on a command line or in chat.
   Give the CT220 reader identity read access to it.
2. CT220: add the binding `/ARTIFACT_PREVIEW_RENDERER_TOKEN` → `PREVIEW_RENDERER_TOKEN` to the
   launcher, next to the one for `/ARTIFACT_AUDIT_LEDGER_HMAC_KEY`, and restart `artifact-mcp`.
   The server now sends the header, and the renderer ignores it. Check that
   `regenerate_artifact_preview` still gives a thumbnail.
3. CT222: write the file. On pve1, the value goes through a pipe into the container; it is not
   printed or stored on pve1 (`printf` is a shell built-in, so the value is not in any process's
   arguments):

   ```sh
   homelab-secrets --config <reader profile> run \
     --secret /ARTIFACT_PREVIEW_RENDERER_TOKEN=PREVIEW_RENDERER_TOKEN -- \
     sh -c 'printf "%s\n" "$PREVIEW_RENDERER_TOKEN" | pct exec 222 -- sh -c \
       "umask 077 && mkdir -p /etc/artifact-preview && cat > /etc/artifact-preview/render-token"'
   pct exec 222 -- stat -c '%U:%G %a %s' /etc/artifact-preview/render-token   # root:root 600 65
   ```

4. CT222: add two lines to the `[Service]` section of the drop-in (or of the unit, after step 8).
   `LoadCredential=` lets systemd read the root-only file and give the service a private copy;
   `%d` is that credentials directory.

   ```ini
   LoadCredential=render-token:/etc/artifact-preview/render-token
   Environment=RENDER_TOKEN_FILE=%d/render-token
   ```

   Then:

   ```sh
   systemctl daemon-reload
   systemctl restart artifact-preview
   systemctl is-active artifact-preview                                        # active
   RENDER_TOKEN_FILE=/etc/artifact-preview/render-token \
     PREVIEW_RENDERER_URL=http://127.0.0.1:3000 /usr/local/bin/artifact-preview smoke   # passed
   PREVIEW_RENDERER_URL=http://127.0.0.1:3000 /usr/local/bin/artifact-preview smoke     # fails: HTTP 401
   curl -fsS http://127.0.0.1:3000/health                                     # {"status":"ok"}
   ```

   `LoadCredential=` has not been tested in this unprivileged container. If the restart fails
   with a credentials error, use a file that the service user can read instead
   (`chown root:artifact-preview` and `chmod 0640`), and set
   `Environment=RENDER_TOKEN_FILE=/etc/artifact-preview/render-token` without `LoadCredential=`.
5. From CT220, run `regenerate_artifact_preview` once more. The journal on CT222 must not show
   `rejected a render request without a valid token`.

Roll back in the reverse order: remove the two lines on CT222, run `systemctl daemon-reload` and
`systemctl restart artifact-preview` (the renderer is open again), and then remove the binding on
CT220 if needed. To rotate the token, store the new value, then repeat steps 3 and 4 and restart
`artifact-mcp` on CT220 right away. Renders between the two restarts fail and keep their
placeholders; the restart of `artifact-mcp` backfills missing previews.

## Later browser upgrades

Unpack the new version into `/opt/chrome-headless-shell-<version>` as in step 3. Run step 4 with
`CHROME_HEADLESS_SHELL` set to the new directory. Then move the link and restart:

```sh
ln -sfn /opt/chrome-headless-shell-<version> /opt/chrome-headless-shell
systemctl restart artifact-preview
```

Roll back by moving the link back. Delete old directories after the new one has run for a week.
