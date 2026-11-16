//! Operator commands: Cloudflare Access setup, the Anthropic MCP
//! tunnel, live-data artifact publishing, and the OCI durability benchmark.
//!
//! Flags, environment variables, output, and exit status are stable operator interfaces: success
//! exits 0 and every failure prints its own message and exits 1
//! (`Ok(false)`), never the `xtask:` prefix and exit 2 of the maintenance commands.

mod benchmark;
mod cf_access;
mod http;
mod js;
mod publish;
mod tunnel;

#[cfg(test)]
#[path = "../../tests/ops_support/mock_http.rs"]
mod mock_http;

use std::io;

use crate::Result;

/// Dispatch one operator command.
pub fn run(command: &str, args: &[String]) -> Result<bool> {
    Ok(match command {
        "cf-access-setup" => cf_access::main(args),
        "tunnel" => tunnel::main(args),
        "publish-data-artifact" => publish::main(args),
        "benchmark-durability" => benchmark::main(args),
        other => return Err(format!("unknown operator command {other}").into()),
    })
}

/// Frozen filesystem error message for `syscall` on `path`, used by operator output checks.
pub(crate) fn node_fs_error(error: &io::Error, syscall: &str, path: &str) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => {
            format!("ENOENT: no such file or directory, {syscall} '{path}'")
        }
        io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{path}'"),
        io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".to_owned(),
        _ => format!("{error}, {syscall} '{path}'"),
    }
}

/// Read an environment variable as lossy UTF-8 for the operator command interface.
pub(crate) fn env_var(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}
