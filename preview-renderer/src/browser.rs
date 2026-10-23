// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The renderer's launch policy on top of the shared launcher in `artifact-cdp`.
//!
//! Every render starts a fresh browser with [`Hardening::launch_options`]: the network lockdown
//! switches, a private `artifact-preview-*` temporary directory, and the renderer's own message
//! cap. The launcher itself (process group, `PR_SET_PDEATHSIG`, cleanup) lives in
//! `cdp/src/browser.rs`.

use artifact_cdp::{LaunchOptions, Network};

pub(crate) use artifact_cdp::Browser;

/// Prefix of every per-render temporary directory, under `$TMPDIR` (default `/tmp`).
pub const TEMP_DIR_PREFIX: &str = "artifact-preview-";

/// Prefix of the launcher's diagnostics on stderr.
const LOG_LABEL: &str = "[artifact-preview]";

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

    /// Launch options for one render. `network_flags` selects [`Network::LockedDown`]; tests
    /// that prove the other layers on their own turn it off ([`Network::Unrestricted`]).
    pub(crate) fn launch_options(self, max_message_bytes: usize) -> LaunchOptions {
        LaunchOptions {
            network: if self.network_flags {
                Network::LockedDown
            } else {
                Network::Unrestricted
            },
            max_message_bytes,
            ..LaunchOptions::new(TEMP_DIR_PREFIX, LOG_LABEL)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::*;

    /// The arguments the renderer starts Chrome with for one render.
    fn launch_args(profile_dir: &Path, hardening: Hardening) -> Vec<OsString> {
        artifact_cdp::launch_args(
            profile_dir,
            &hardening.launch_options(artifact_cdp::DEFAULT_MAX_MESSAGE_BYTES),
        )
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

    /// The exact production launch line, written out independently of the constants that build
    /// it. Any change to a renderer flag, its order, or the network lockdown must change this
    /// list on purpose.
    const PINNED_PRODUCTION_ARGS: &[&str] = &[
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
        "--disable-features=AvoidUnnecessaryBeforeUnloadCheckSync,BoundaryEventDispatchTracksNodeRemoval,DestroyProfileOnBrowserClose,DialMediaRouteProvider,GlobalMediaControls,HttpsUpgrades,LensOverlay,MediaRouter,PaintHolding,ThirdPartyStoragePartitioning,Translate,AutoDeElevate,RenderDocument,OptimizationHints,msForceBrowserSignIn,msEdgeUpdateLaunchServicesPreferredVersion",
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
        "--proxy-server=127.0.0.1:9",
        "--proxy-bypass-list=<-loopback>",
        "--host-resolver-rules=MAP * ~NOTFOUND",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--user-data-dir=/tmp/p",
    ];

    /// The four network lockdown switches, which only `Hardening::network_flags` adds.
    const LOCKDOWN_PREFIXES: &[&str] = &[
        "--proxy-server=",
        "--proxy-bypass-list=",
        "--host-resolver-rules=",
        "--force-webrtc-ip-handling-policy=",
    ];

    #[test]
    fn production_launch_args_are_pinned_byte_for_byte() {
        let expected: Vec<OsString> = PINNED_PRODUCTION_ARGS.iter().map(OsString::from).collect();
        assert_eq!(launch_args(Path::new("/tmp/p"), Hardening::FULL), expected);
        let expected_without: Vec<OsString> = PINNED_PRODUCTION_ARGS
            .iter()
            .filter(|arg| {
                !LOCKDOWN_PREFIXES
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
            })
            .map(OsString::from)
            .collect();
        let without = Hardening {
            network_flags: false,
            ..Hardening::FULL
        };
        assert_eq!(launch_args(Path::new("/tmp/p"), without), expected_without);
    }

    #[test]
    fn renderer_options_keep_the_private_prefix_and_the_message_cap() {
        let options = Hardening::FULL.launch_options(1234);
        assert_eq!(options.temp_dir_prefix, "artifact-preview-");
        assert_eq!(options.log_label, "[artifact-preview]");
        assert_eq!(options.max_message_bytes, 1234);
        assert!(options.extra_args.is_empty());
        assert!(!options.inherit_stderr);
    }
}
