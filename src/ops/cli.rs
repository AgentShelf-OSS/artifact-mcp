//! Argument parsing and exit codes for the operator subcommands.
//!
//! Exit codes: 0 success, 1 failure, 2 usage error, 128 + signal number when a signal stopped a
//! backup (its staging directory is removed first).

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use super::{
    backup::{
        self, BackupError, BackupObserver, BackupOptions, Cancel, DEFAULT_KEEP, VerifyOptions,
    },
    web_push_keys::VapidKeyPair,
};
use crate::config::OsRandom;

/// The server binary's usage line.
pub const USAGE: &str = "usage: artifact-mcp [healthcheck | check-action-grants FILE \
     | backup [DATA_DIR] [DEST_ROOT] [KEEP] \
     | verify-backup [--database-required] [--preview-manifest-required] DIR \
     | web-push-keys]";

const BACKUP_HELP: &str = "\
usage: artifact-mcp backup [DATA_DIR] [DEST_ROOT] [KEEP]

Write a consistent, verified backup of DATA_DIR to DEST_ROOT/backup-<UTC stamp>-<suffix>,
then keep only the newest KEEP backups.

  DATA_DIR   default: $ARTIFACT_MCP_DATA_DIR, else ./data
  DEST_ROOT  default: $ARTIFACT_MCP_BACKUP_DIR, else ./backups
  KEEP       positive integer, default 14";

const VERIFY_HELP: &str = "\
usage: artifact-mcp verify-backup [--database-required] [--preview-manifest-required] DIR

Check a backup directory against its database snapshot without changing it.

  --database-required          fail when DIR has no artifacts.db
  --preview-manifest-required  fail when DIR has no preview-sha256.json";

const WEB_PUSH_HELP: &str = "\
usage: artifact-mcp web-push-keys

Print a new VAPID key pair for Web Push reminders. Put the WEB_PUSH_VAPID_PRIVATE_KEY line in
the server environment; the server derives the public key.";

/// Run an operator subcommand, or return `None` when `args` names none.
pub async fn dispatch(args: &[OsString]) -> Option<ExitCode> {
    let (command, rest) = args.split_first()?;
    match command.to_str()? {
        "backup" => Some(backup_command(rest).await),
        "verify-backup" => Some(verify_command(rest)),
        "web-push-keys" => Some(web_push_keys_command(rest)),
        _ => None,
    }
}

fn is_help(args: &[OsString]) -> bool {
    matches!(args, [flag] if flag == "-h" || flag == "--help")
}

fn usage_error(help: &str) -> ExitCode {
    eprintln!("{help}");
    ExitCode::from(2)
}

/// `${N:-${VAR:-default}}`: an empty argument or variable counts as unset.
fn positional(args: &[OsString], index: usize, variable: &str, default: &str) -> OsString {
    args.get(index)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var_os(variable).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| OsString::from(default))
}

async fn backup_command(args: &[OsString]) -> ExitCode {
    if is_help(args) {
        println!("{BACKUP_HELP}");
        return ExitCode::SUCCESS;
    }
    if args.len() > 3 {
        return usage_error(BACKUP_HELP);
    }
    let keep = match args.get(2).filter(|value| !value.is_empty()) {
        None => Ok(DEFAULT_KEEP),
        Some(raw) => raw
            .to_str()
            .ok_or(BackupError::InvalidKeep)
            .and_then(backup::parse_keep),
    };
    let keep = match keep {
        Ok(keep) => keep,
        Err(error) => return failure(&error),
    };
    let options = BackupOptions {
        data_dir: PathBuf::from(positional(args, 0, "ARTIFACT_MCP_DATA_DIR", "./data")),
        dest_root: PathBuf::from(positional(args, 1, "ARTIFACT_MCP_BACKUP_DIR", "./backups")),
        keep,
    };

    let cancel = Arc::new(Cancel::default());
    let worker_cancel = Arc::clone(&cancel);
    let work = move || {
        let mut observer = observer(&worker_cancel);
        let report = backup::run_backup(&options, &worker_cancel, observer.as_mut())?;
        println!("{report}");
        for name in backup::prune_backups(&options.dest_root, options.keep, &worker_cancel)? {
            println!("backup: pruned {name}");
        }
        Ok(())
    };
    match run_until_signal(work, &cancel).await {
        Outcome::Finished(Ok(())) => ExitCode::SUCCESS,
        Outcome::Finished(Err(error)) => failure(&error),
        Outcome::Signalled { signal, settled } => {
            if settled {
                eprintln!("backup: stopped by signal {signal}; no incomplete staging left");
            } else {
                eprintln!(
                    "backup: stopped by a second signal; an .incomplete-* directory may remain"
                );
            }
            ExitCode::from(128_u8.saturating_add(signal))
        }
    }
}

fn failure(error: &BackupError) -> ExitCode {
    eprintln!("backup: {error}");
    ExitCode::FAILURE
}

enum Outcome {
    Finished(Result<(), BackupError>),
    /// `settled`: the worker stopped (and cleaned up) before the process exits.
    Signalled {
        signal: u8,
        settled: bool,
    },
}

/// Run `work` on a blocking thread. On SIGINT, SIGTERM, or SIGHUP, cancel it and wait until it
/// has removed its staging directory; a second signal stops waiting. The handlers are installed
/// before the work starts, so no signal can skip the cleanup.
async fn run_until_signal<F>(work: F, cancel: &Cancel) -> Outcome
where
    F: FnOnce() -> Result<(), BackupError> + Send + 'static,
{
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut interrupt = signal(SignalKind::interrupt()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();
    let mut worker = tokio::task::spawn_blocking(work);
    let received = tokio::select! {
        joined = &mut worker => return Outcome::Finished(joined_result(joined)),
        () = next(&mut terminate) => 15,
        () = next(&mut interrupt) => 2,
        () = next(&mut hangup) => 1,
    };
    cancel.cancel();
    tokio::select! {
        joined = &mut worker => match joined_result(joined) {
            // The run completed before it saw the cancellation.
            Ok(()) => Outcome::Finished(Ok(())),
            Err(_) => Outcome::Signalled { signal: received, settled: true },
        },
        () = next(&mut terminate) => Outcome::Signalled { signal: received, settled: false },
        () = next(&mut interrupt) => Outcome::Signalled { signal: received, settled: false },
        () = next(&mut hangup) => Outcome::Signalled { signal: received, settled: false },
    }
}

fn joined_result(
    joined: Result<Result<(), BackupError>, tokio::task::JoinError>,
) -> Result<(), BackupError> {
    joined.unwrap_or_else(|error| {
        Err(BackupError::Io {
            action: "backup worker failed".to_owned(),
            source: std::io::Error::other(error.to_string()),
        })
    })
}

async fn next(signal: &mut Option<tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            let _ = signal.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(debug_assertions))]
fn observer(_cancel: &Arc<Cancel>) -> Box<dyn BackupObserver + Send> {
    Box::new(backup::WarningsToStderr)
}

/// Debug builds only: `ARTIFACT_MCP_BACKUP_TEST_PAUSE=<file>` makes the run create `<file>` at
/// the checkpoint named by `ARTIFACT_MCP_BACKUP_TEST_PHASE` (`before-snapshot` or
/// `after-snapshot`, the default) and wait until the file is removed or the run is cancelled.
/// The signal-cleanup test uses it; release builds ignore both variables.
#[cfg(debug_assertions)]
fn observer(cancel: &Arc<Cancel>) -> Box<dyn BackupObserver + Send> {
    let Some(file) = std::env::var_os("ARTIFACT_MCP_BACKUP_TEST_PAUSE") else {
        return Box::new(backup::WarningsToStderr);
    };
    let phase = std::env::var("ARTIFACT_MCP_BACKUP_TEST_PHASE")
        .unwrap_or_else(|_| "after-snapshot".to_owned());
    Box::new(TestPause {
        file: PathBuf::from(file),
        phase,
        cancel: Arc::clone(cancel),
    })
}

#[cfg(debug_assertions)]
struct TestPause {
    file: PathBuf,
    phase: String,
    cancel: Arc<Cancel>,
}

#[cfg(debug_assertions)]
impl BackupObserver for TestPause {
    fn reached(&mut self, point: backup::Checkpoint<'_>) -> Result<(), BackupError> {
        let matches = matches!(
            (self.phase.as_str(), point),
            ("before-snapshot", backup::Checkpoint::BeforeSnapshot)
                | ("after-snapshot", backup::Checkpoint::AfterSnapshot { .. })
        );
        if !matches {
            return Ok(());
        }
        std::fs::write(&self.file, b"").map_err(|source| BackupError::Io {
            action: "cannot create the test pause file".to_owned(),
            source,
        })?;
        while self.file.exists() {
            self.cancel.check()?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(())
    }
}

fn verify_command(args: &[OsString]) -> ExitCode {
    if is_help(args) {
        println!("{VERIFY_HELP}");
        return ExitCode::SUCCESS;
    }
    let mut options = VerifyOptions::default();
    let mut directory: Option<&OsStr> = None;
    for arg in args {
        match arg.to_str() {
            Some("--database-required") => options.database_required = true,
            Some("--preview-manifest-required") => options.preview_manifest_required = true,
            Some(flag) if flag.starts_with('-') && flag.len() > 1 => {
                return usage_error(VERIFY_HELP);
            }
            _ if directory.is_none() => directory = Some(arg),
            _ => return usage_error(VERIFY_HELP),
        }
    }
    let Some(directory) = directory else {
        return usage_error(VERIFY_HELP);
    };
    let directory = PathBuf::from(directory);
    match backup::verify_backup(&directory, options) {
        Ok(summary) => {
            println!("verify-backup: {} ok ({summary})", directory.display());
            ExitCode::SUCCESS
        }
        Err(error) => failure(&error),
    }
}

fn web_push_keys_command(args: &[OsString]) -> ExitCode {
    if is_help(args) {
        println!("{WEB_PUSH_HELP}");
        return ExitCode::SUCCESS;
    }
    if !args.is_empty() {
        return usage_error(WEB_PUSH_HELP);
    }
    match VapidKeyPair::generate(&OsRandom) {
        Ok(pair) => {
            print!("{}", pair.render());
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("web-push-keys: the operating system random source failed");
            ExitCode::FAILURE
        }
    }
}
