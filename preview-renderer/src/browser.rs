// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! One `chrome-headless-shell` process tree per render.
//!
//! [`Browser::launch`] creates a private temporary directory (profile, `HOME` and `TMPDIR` for the
//! browser), starts the browser as the leader of a new process group with the DevTools pipe on
//! fds 3 and 4, and returns the parent's pipe ends. [`Browser::shutdown`] sends `SIGKILL` to the
//! whole group, reaps the leader, waits until no member of the group is still running, and removes
//! the directory. `Drop` repeats the kill and the removal on a best-effort basis in case
//! `shutdown` never ran (a panic, or a cancelled task).
//!
//! The browser runs without Chromium's own sandbox (`--no-sandbox`), as it did under Playwright
//! (`chromiumSandbox: false`): that sandbox needs user namespaces or a setuid helper, which the
//! hardened container and systemd unit remove. The container or unit is the isolation boundary.
//!
//! `PR_SET_PDEATHSIG` fires when the *thread* that spawned the browser exits. The service runs a
//! current-thread tokio runtime, so that thread is the main thread and lives as long as the process.

use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::net::unix::pipe;

use crate::cdp::Pipe;
use crate::sys;

/// Prefix of every per-render temporary directory, under `$TMPDIR` (default `/tmp`).
pub const TEMP_DIR_PREFIX: &str = "artifact-preview-";

/// How long [`Browser::shutdown`] waits for the killed group to finish exiting.
const GROUP_EXIT_WAIT: Duration = Duration::from_secs(2);

/// Variables passed through from the service to the browser. Everything else is cleared, so the
/// browser never inherits secrets or proxy settings from the service environment.
const PASSTHROUGH_ENV: &[&str] = &["LANG", "LANGUAGE", "LC_ALL", "TZ"];

/// Network defences. Production always uses [`Hardening::FULL`]; tests turn single layers off to
/// prove that each remaining layer blocks traffic on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hardening {
    /// Pause every request with the DevTools `Fetch` domain and fail all but `data:` subresources.
    pub(crate) intercept_requests: bool,
    /// `Network.emulateNetworkConditions` offline, as Playwright's `offline: true`.
    pub(crate) offline: bool,
    /// Launch flags: an unreachable proxy for all traffic, no DNS, no non-proxied WebRTC UDP.
    pub(crate) network_flags: bool,
}

impl Hardening {
    pub(crate) const FULL: Self = Self {
        intercept_requests: true,
        offline: true,
        network_flags: true,
    };
}

/// Chromium switches. The first group is Playwright 1.60's `chromiumSwitches()` list plus the
/// arguments its `defaultArgs()` adds for a headless, non-persistent launch with
/// `chromiumSandbox: false`, so pages render as they did under the Node renderer.
const PLAYWRIGHT_SWITCHES: &[&str] = &[
    "--disable-field-trial-config",
    "--disable-background-networking",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-back-forward-cache",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-component-update",
    "--no-default-browser-check",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-edgeupdater",
    "--disable-extensions",
    "--disable-features=AvoidUnnecessaryBeforeUnloadCheckSync,BoundaryEventDispatchTracksNodeRemoval,\
DestroyProfileOnBrowserClose,DialMediaRouteProvider,GlobalMediaControls,HttpsUpgrades,LensOverlay,MediaRouter,\
PaintHolding,ThirdPartyStoragePartitioning,Translate,AutoDeElevate,RenderDocument,OptimizationHints,\
msForceBrowserSignIn,msEdgeUpdateLaunchServicesPreferredVersion",
    "--enable-features=CDPScreenshotNewSurface",
    "--allow-pre-commit-input",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--password-store=basic",
    "--use-mock-keychain",
    "--no-service-autorun",
    "--export-tagged-pdf",
    "--disable-search-engine-choice-screen",
    "--unsafely-disable-devtools-self-xss-warnings",
    "--edge-skip-compat-layer-relaunch",
    "--disable-infobars",
    "--disable-sync",
    "--enable-unsafe-swiftshader",
    "--headless",
    "--hide-scrollbars",
    "--mute-audio",
    "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4",
    "--no-sandbox",
    "--no-startup-window",
    "--remote-debugging-pipe",
];

/// Defence in depth beyond request interception, applied to the whole browser: every frame,
/// popup and worker, and the traffic the DevTools `Fetch` domain never sees. Testing showed
/// interception plus offline emulation (the Node renderer's only in-browser defences) let
/// WebSocket handshakes, WebRTC STUN (UDP) and TURN (TCP) packets, and speculation-rules
/// prefetch and prerender requests reach a canary on loopback and on a LAN address.
const NETWORK_SWITCHES: &[&str] = &[
    // Send all HTTP(S), WebSocket and FTP traffic to a proxy that is not listening (port 9,
    // "discard"). Nothing listens there in the container's private network namespace or on CT222.
    "--proxy-server=127.0.0.1:9",
    // Chromium bypasses the proxy for loopback by default. `<-loopback>` removes that implicit
    // rule, so 127.0.0.1, ::1 and localhost also go to the dead proxy.
    "--proxy-bypass-list=<-loopback>",
    // Resolve no host name at all: no DNS queries (a DNS exfiltration channel) and no direct
    // connections by name if anything ever bypassed the proxy.
    "--host-resolver-rules=MAP * ~NOTFOUND",
    // WebRTC may only use UDP through a proxy, and there is no usable proxy, so STUN, TURN and
    // peer connections cannot send packets.
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
];

/// The launch arguments for one render.
pub(crate) fn launch_args(profile_dir: &Path, hardening: Hardening) -> Vec<OsString> {
    let mut args: Vec<OsString> = PLAYWRIGHT_SWITCHES.iter().map(OsString::from).collect();
    if hardening.network_flags {
        args.extend(NETWORK_SWITCHES.iter().map(OsString::from));
    }
    let mut user_data_dir = OsString::from("--user-data-dir=");
    user_data_dir.push(profile_dir);
    args.push(user_data_dir);
    args
}

pub(crate) struct Browser {
    child: Option<tokio::process::Child>,
    /// The leader's pid, which is also the process group id.
    pgid: u32,
    /// `Some` until the directory has been removed.
    temp_dir: Option<PathBuf>,
    killed: bool,
    pub(crate) pipe: Pipe,
}

impl Browser {
    /// Create the temporary directory and start the browser. Nothing is left behind on error.
    pub(crate) fn launch(
        executable: &Path,
        hardening: Hardening,
        max_message_bytes: usize,
    ) -> io::Result<Self> {
        let temp_dir = create_private_temp_dir()?;
        match Self::spawn(executable, hardening, max_message_bytes, &temp_dir) {
            Ok((child, pgid, pipe)) => Ok(Self {
                child: Some(child),
                pgid,
                temp_dir: Some(temp_dir),
                killed: false,
                pipe,
            }),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&temp_dir);
                Err(error)
            }
        }
    }

    fn spawn(
        executable: &Path,
        hardening: Hardening,
        max_message_bytes: usize,
        temp_dir: &Path,
    ) -> io::Result<(tokio::process::Child, u32, Pipe)> {
        let home = temp_dir.join("home");
        let tmp = temp_dir.join("tmp");
        let profile = temp_dir.join("profile");
        for dir in [&home, &tmp, &profile] {
            std::fs::create_dir(dir)?;
        }

        // Commands flow parent -> browser through the first pipe, replies and events flow back
        // through the second. `std::io::pipe` creates both ends with `O_CLOEXEC`.
        let (browser_reads, parent_writes) = std::io::pipe()?;
        let (parent_reads, browser_writes) = std::io::pipe()?;

        let mut command = tokio::process::Command::new(executable);
        command
            .args(launch_args(&profile, hardening))
            .env_clear()
            .env("HOME", &home)
            .env("TMPDIR", &tmp)
            .current_dir(temp_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(false);
        for name in PASSTHROUGH_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }

        let child = sys::spawn_with_devtools_pipe(
            &mut command,
            OwnedFd::from(browser_reads),
            OwnedFd::from(browser_writes),
        )?;
        let Some(pgid) = child.id() else {
            return Err(io::Error::other("browser exited before its pid was known"));
        };
        let browser = BrowserGuard {
            child: Some(child),
            pgid,
        };
        let sender = pipe::Sender::from_owned_fd(OwnedFd::from(parent_writes))?;
        let receiver = pipe::Receiver::from_owned_fd(OwnedFd::from(parent_reads))?;
        let (child, pgid) = browser.disarm();
        Ok((child, pgid, Pipe::new(sender, receiver, max_message_bytes)))
    }

    /// The process group id (the leader's pid), for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn pgid(&self) -> u32 {
        self.pgid
    }

    /// Kill the whole process tree, reap the leader, wait for the group to exit, remove the
    /// temporary directory. Never fails; problems are logged.
    pub(crate) async fn shutdown(mut self) {
        self.kill();
        if let Some(mut child) = self.child.take()
            && let Err(error) = child.wait().await
        {
            eprintln!(
                "[artifact-preview] could not reap browser {}: {error}",
                self.pgid
            );
        }
        let started = Instant::now();
        while group_has_running_members(self.pgid) {
            if started.elapsed() > GROUP_EXIT_WAIT {
                eprintln!(
                    "[artifact-preview] browser process group {} still running after SIGKILL",
                    self.pgid
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.remove_temp_dir();
    }

    fn kill(&mut self) {
        if self.killed || self.child.is_none() {
            return;
        }
        // Only signal while the leader is unreaped (`self.child` is `Some`): see
        // `sys::kill_process_group` for why that keeps the group id from being recycled.
        if let Err(error) = sys::kill_process_group(self.pgid) {
            eprintln!(
                "[artifact-preview] could not kill browser process group {}: {error}",
                self.pgid
            );
        }
        self.killed = true;
    }

    fn remove_temp_dir(&mut self) {
        if let Some(dir) = self.temp_dir.take()
            && let Err(error) = std::fs::remove_dir_all(&dir)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "[artifact-preview] could not remove {}: {error}",
                dir.display()
            );
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Normal path: `shutdown` already did all of this. Here the leader may still be running,
        // so kill first; tokio reaps a dropped child in the background.
        self.kill();
        self.remove_temp_dir();
    }
}

/// Kills the browser group if `spawn` fails after the process started.
struct BrowserGuard {
    child: Option<tokio::process::Child>,
    pgid: u32,
}

impl BrowserGuard {
    fn disarm(mut self) -> (tokio::process::Child, u32) {
        let child = self
            .child
            .take()
            .expect("guard holds the child until disarmed");
        (child, self.pgid)
    }
}

impl Drop for BrowserGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = sys::kill_process_group(self.pgid);
        }
    }
}

/// A new directory under `$TMPDIR`, mode 0700, with a name nobody else can predict and claim
/// first (`create_dir` fails rather than reusing an existing path).
fn create_private_temp_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    for _ in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "{TEMP_DIR_PREFIX}{}-{count}-{nanos:x}",
            std::process::id()
        ));
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary directory",
    ))
}

/// True while any process in group `pgid` is still running. Zombies do not count: they hold no
/// memory or files and their parent (the init process) reaps them.
pub(crate) fn group_has_running_members(pgid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { return false };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            return false;
        };
        matches!(parse_stat(&stat), Some((state, group)) if group == pgid && state != 'Z' && state != 'X')
    })
}

/// `(state, pgrp)` from `/proc/<pid>/stat`. The command name in field 2 may contain spaces and
/// parentheses, so parsing starts after the last `)`.
fn parse_stat(stat: &str) -> Option<(char, u32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((state, pgrp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_handles_hostile_command_names() {
        assert_eq!(parse_stat("42 (chrome) S 1 42 42 0 -1"), Some(('S', 42)));
        assert_eq!(parse_stat("42 (a) b) (c) Z 1 77 42 0 -1"), Some(('Z', 77)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn launch_args_add_network_switches_only_when_hardened() {
        let full = launch_args(Path::new("/tmp/p"), Hardening::FULL);
        assert!(full.iter().any(|arg| arg == "--proxy-server=127.0.0.1:9"));
        assert!(full.iter().any(|arg| arg == "--no-sandbox"));
        assert!(full.iter().any(|arg| arg == "--remote-debugging-pipe"));
        assert!(full.iter().any(|arg| arg == "--user-data-dir=/tmp/p"));
        assert!(
            !full
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("--remote-debugging-port"))
        );
        let without = launch_args(
            Path::new("/tmp/p"),
            Hardening {
                network_flags: false,
                ..Hardening::FULL
            },
        );
        assert!(
            !without
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("--proxy-server"))
        );
    }

    #[test]
    fn temp_dirs_are_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;
        let first = create_private_temp_dir().unwrap();
        let second = create_private_temp_dir().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            std::fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir(first).unwrap();
        std::fs::remove_dir(second).unwrap();
    }
}
