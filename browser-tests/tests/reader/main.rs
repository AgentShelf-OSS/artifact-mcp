// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `cargo test --test reader`: the reader regression suite, ported from the standalone programs
//! `playwright/tests/reader-*.cjs` and their runner `playwright/reader-runner.mjs`. One module per
//! old program, with the same case name and the same assertions; each case reports its old
//! `PASS` line (or JSON summary):
//!
//! ```text
//! test reader-sections [chromium] ... ok (812 ms)
//!     reader bridge browser fixtures passed: TOC seek, fingerprint rejection, section bounds
//! ```
//!
//! `chromium` cases drive `assets/reader-bridge.js` (and `reader-ereader.js`) in plain pages;
//! `rust` cases run the viewer's reader against the release binary with a stub speech worker.
//! Needs `CHROME_HEADLESS_SHELL` and `ARTIFACT_MCP_BIN` (both skip the run when unset, and fail it
//! when `CI` is set). Arguments (after `--`): name substrings, `--exact`, `--list`. See
//! `browser_tests::reader` for the fixture.

mod common;
mod controls;
mod forward;
mod mixed_content;
mod pacing;
mod place_sync;
mod player_scopes;
mod prefetch;
mod pronunciation;
mod read_ahead;
mod scopes;
mod sections;
mod voice_cast;
mod wav;
mod word_highlights;

use std::path::PathBuf;
use std::process::ExitCode;

use browser_tests::reader::Case;
use browser_tests::reader_case;

fn main() -> ExitCode {
    // The old runner's order.
    let cases: Vec<Case> = vec![
        reader_case!("reader-mixed-content", "chromium", mixed_content::run),
        reader_case!("reader-scopes", "chromium", scopes::run),
        reader_case!("reader-sections", "chromium", sections::run),
        reader_case!("reader-word-highlights", "chromium", word_highlights::run),
        reader_case!("reader-controls", "rust", controls::run),
        reader_case!("reader-player-scopes", "rust", player_scopes::run),
        reader_case!("reader-prefetch", "rust", prefetch::run),
        reader_case!("reader-pronunciation", "rust", pronunciation::run),
        reader_case!("reader-pacing", "rust", pacing::run),
        reader_case!("reader-wav", "rust", wav::run),
        reader_case!("reader-voice-cast", "rust", voice_cast::run),
        reader_case!("reader-place-sync", "rust", place_sync::run),
        reader_case!("reader-forward", "rust", forward::run),
        reader_case!("reader-read-ahead", "rust", read_ahead::run),
    ];
    browser_tests::reader::main(
        &cases,
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reader-failures"),
    )
}
