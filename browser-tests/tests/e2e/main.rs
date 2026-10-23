// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `cargo test --test e2e`: browser journeys against the artifact-mcp release binary, ported from
//! the Playwright suite in `playwright/tests`. One module per spec file; each case keeps the
//! Playwright test title and is reported as `spec :: title`:
//!
//! ```text
//! test 01-gallery :: renders and lists an artifact published into the throwaway org ... ok (310 ms)
//! ```
//!
//! Needs `CHROME_HEADLESS_SHELL` and `ARTIFACT_MCP_BIN` (both skip the run when unset, and fail it
//! when `CI` is set). Arguments (after `--`): name substrings, `--exact`, `--list`. See
//! `browser_tests::e2e` for the fixtures.

mod s01_gallery;
mod s02_viewer;
mod s03_reactions;
mod s04_feedback;
mod s05_shares;
mod s06_categories;
mod s07_tenancy;
mod s08_settings;
mod s09_csrf;
mod s10_collections;
mod s11_collection_dialogs;
mod s12_artifact_popovers;
mod s13_inline_folders;
mod s14_collection_gestures;
mod s15_mcp_collections;
mod s16_folder_create_preview;
mod s17_shared_confirmations;
mod s18_web_performance;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cases: Vec<browser_tests::e2e::Case> = [
        s01_gallery::cases(),
        s02_viewer::cases(),
        s03_reactions::cases(),
        s04_feedback::cases(),
        s05_shares::cases(),
        s06_categories::cases(),
        s07_tenancy::cases(),
        s08_settings::cases(),
        s09_csrf::cases(),
        s10_collections::cases(),
        s11_collection_dialogs::cases(),
        s12_artifact_popovers::cases(),
        s13_inline_folders::cases(),
        s14_collection_gestures::cases(),
        s15_mcp_collections::cases(),
        s16_folder_create_preview::cases(),
        s17_shared_confirmations::cases(),
        s18_web_performance::cases(),
    ]
    .concat();
    browser_tests::e2e::main(
        &cases,
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("e2e-failures"),
    )
}
