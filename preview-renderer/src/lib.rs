// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `artifact-preview`: a network-isolated renderer that turns hostile, untrusted HTML into a PNG.
//!
//! Each render starts a fresh `chrome-headless-shell` process tree in its own process group,
//! drives it over the DevTools protocol on a private pipe, and kills the whole group when the
//! screenshot is taken or the deadline passes. See `README.md` for the threat model.
#![deny(unsafe_code)]

mod browser;
mod cdp;
pub mod client;
pub mod config;
pub mod image_info;
pub mod jsnum;
pub mod render;
pub mod server;
mod sys;

#[cfg(test)]
mod tests;

/// Prefix of the per-render temporary directories under `$TMPDIR`.
pub use browser::TEMP_DIR_PREFIX;
