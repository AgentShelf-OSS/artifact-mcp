//! Memory tuning for the mimalloc global allocator (ADR 0025).
//!
//! The production binary uses mimalloc instead of musl's malloc, which made MCP tool calls 2–4×
//! more CPU-costly (issue #164). With mimalloc's defaults, resident memory rose to about 156 MiB
//! idle and 607 MiB at peak. Two options bring it back to about 31 MiB idle and 80–107 MiB at
//! peak, while keeping most of the CPU gain:
//!
//! - `MIMALLOC_ARENA_EAGER_COMMIT=0`: commit arena memory on demand, so a purge can decommit it.
//! - `MIMALLOC_PURGE_DELAY=0`: return freed memory to the OS at once instead of after 1 s.
//!
//! mimalloc reads its options from the environment when the allocator starts, before `main`.
//! The first arena already exists by then, and mimalloc v3 has no compile-time default for the
//! purge delay. So the binary checks the environment first thing in `main` and, if the tuning is
//! missing, replaces itself with a copy of the same executable that has it. [`MARKER`] stops a
//! second pass. A value the operator set is never changed.

use std::{
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt as _,
    process::Command,
};

/// The tuned mimalloc options and their values.
pub const TUNING: [(&str, &str); 2] = [
    ("MIMALLOC_ARENA_EAGER_COMMIT", "0"),
    ("MIMALLOC_PURGE_DELAY", "0"),
];

/// Set on the replacement process so it never tries to replace itself again.
pub const MARKER: &str = "ARTIFACT_MCP_ALLOCATOR_TUNED";

/// The variables to add before a re-exec, or `None` when the process can start as it is.
///
/// Returns `None` when [`MARKER`] is present or every tuning variable is already set. Otherwise
/// returns only the missing tuning variables, plus [`MARKER`].
pub fn missing_tuning(
    lookup: impl Fn(&str) -> Option<OsString>,
) -> Option<Vec<(&'static str, &'static str)>> {
    if lookup(MARKER).is_some() {
        return None;
    }
    let mut missing = TUNING
        .into_iter()
        .filter(|(name, _)| lookup(name).is_none())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return None;
    }
    missing.push((MARKER, "1"));
    Some(missing)
}

/// Replace the current process with a tuned copy of itself when the tuning is missing.
///
/// Call this first in `main`, before any thread or runtime starts. It returns only when no
/// re-exec is needed or the re-exec failed. A failure is reported on stderr, because the log
/// subscriber does not exist yet, and the process keeps running with mimalloc's defaults.
pub fn reexec_with_tuning() {
    let Some(missing) = missing_tuning(|name| std::env::var_os(name)) else {
        return;
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("allocator tuning skipped: cannot locate the executable: {error}");
            return;
        }
    };
    let mut args = std::env::args_os();
    let arg0 = args
        .next()
        .unwrap_or_else(|| executable.clone().into_os_string());
    let error = Command::new(&executable)
        .arg0(arg0)
        .args(args)
        .envs(
            missing
                .iter()
                .map(|(name, value)| (OsStr::new(name), OsStr::new(value))),
        )
        .exec();
    eprintln!("allocator tuning skipped: re-exec failed: {error}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lookup<'a>(env: &'a BTreeMap<&str, &str>) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| env.get(name).map(OsString::from)
    }

    #[test]
    fn empty_environment_adds_every_tuning_variable_and_the_marker() {
        let env = BTreeMap::new();
        assert_eq!(
            missing_tuning(lookup(&env)),
            Some(vec![
                ("MIMALLOC_ARENA_EAGER_COMMIT", "0"),
                ("MIMALLOC_PURGE_DELAY", "0"),
                (MARKER, "1"),
            ])
        );
    }

    #[test]
    fn operator_values_are_kept_and_only_missing_ones_are_added() {
        let env = BTreeMap::from([("MIMALLOC_PURGE_DELAY", "250")]);
        assert_eq!(
            missing_tuning(lookup(&env)),
            Some(vec![("MIMALLOC_ARENA_EAGER_COMMIT", "0"), (MARKER, "1")])
        );
    }

    #[test]
    fn fully_tuned_environment_needs_no_reexec() {
        let env = BTreeMap::from([
            ("MIMALLOC_ARENA_EAGER_COMMIT", "1"),
            ("MIMALLOC_PURGE_DELAY", "10"),
        ]);
        assert_eq!(missing_tuning(lookup(&env)), None);
    }

    #[test]
    fn marker_prevents_a_second_reexec() {
        let env = BTreeMap::from([(MARKER, "1")]);
        assert_eq!(missing_tuning(lookup(&env)), None);
    }
}
