//! Operator commands (ported from `scripts/*.mjs`): Cloudflare Access setup, the Anthropic MCP
//! tunnel, live-data artifact publishing, and the OCI durability benchmark.

use crate::Result;

/// Dispatch one operator command.
pub fn run(command: &str, args: &[String]) -> Result<bool> {
    Err(format!("{command}: not implemented yet ({args:?})").into())
}
