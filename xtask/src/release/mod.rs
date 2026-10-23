//! `cargo xtask release ...`: release evidence commands (ported from `scripts/release/*.mjs`).

use crate::Result;

/// Dispatch `cargo xtask release <subcommand>`.
pub fn run(args: &[String]) -> Result<bool> {
    Err(format!("release: not implemented yet ({args:?})").into())
}
