//! Operator commands built into the server binary.
//!
//! Production runs a static binary (and a distroless image) with no shell, Python, `sqlite3`, or
//! Node, so the maintenance tools an operator runs on the data host live here:
//!
//! - [`backup`]: `artifact-mcp backup` and `artifact-mcp verify-backup`.
//! - [`web_push_keys`]: `artifact-mcp web-push-keys`.
//! - [`cli`]: argument parsing and process exit codes for these subcommands.

pub mod backup;
pub mod cli;
pub mod web_push_keys;
