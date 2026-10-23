// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `artifact-cdp`: drive `chrome-headless-shell` over a private DevTools pipe.
//!
//! * [`pipe`]: NUL-framed, size-capped JSON messages on the `--remote-debugging-pipe` file
//!   descriptors. No TCP port, no WebSocket.
//! * [`browser`]: one browser process tree per launch, in its own process group with a private
//!   temporary profile, killed as a whole. [`LaunchOptions`] keeps the preview renderer's network
//!   lockdown as the default and lets tests opt into loopback networking.
//!
//! This crate has no protocol policy. The preview renderer (`artifact-preview`) layers its
//! request interception and page lockdown on top; the browser test harness (`browser-tests`)
//! layers pages, evaluation and console capture on top.
#![deny(unsafe_code)]

pub mod browser;
pub mod pipe;
mod sys;

pub use browser::{
    Browser, LOCKDOWN_SWITCHES, LOOPBACK_ONLY_SWITCHES, LaunchOptions, Network,
    PLAYWRIGHT_SWITCHES, launch_args,
};
pub use pipe::{
    CdpError, DEFAULT_MAX_MESSAGE_BYTES, Incoming, Pipe, PipeReceiver, PipeSender, encode_command,
};
