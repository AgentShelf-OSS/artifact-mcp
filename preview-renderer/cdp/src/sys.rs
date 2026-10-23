// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The only module that uses `unsafe`: process-level system calls that the standard library and
//! tokio do not expose.
//!
//! * [`spawn_with_devtools_pipe`] installs the two DevTools pipe ends as fds 3 and 4 in the child
//!   (`--remote-debugging-pipe` reads commands from fd 3 and writes replies to fd 4) and arms
//!   `PR_SET_PDEATHSIG`, so the browser dies if the process (strictly, the thread) that launched
//!   it dies.
//! * [`kill_process_group`] sends `SIGKILL` to a whole process group.
//!
//! Every block carries a `// SAFETY:` comment. The rest of the crate is `#![deny(unsafe_code)]`.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// First fd number used for the parent-side duplicates. Anything at or above 5 avoids fds 3 and 4.
const HIGH_FD_FLOOR: libc::c_int = 10;

/// Spawn `command` with `child_reads` as fd 3 and `child_writes` as fd 4.
///
/// The caller must already have configured `command.process_group(0)` and its stdio. Both fds are
/// closed in this process before returning, so the parent sees EOF on its own pipe ends when the
/// browser exits.
pub(crate) fn spawn_with_devtools_pipe(
    command: &mut tokio::process::Command,
    child_reads: OwnedFd,
    child_writes: OwnedFd,
) -> io::Result<tokio::process::Child> {
    // Move both fds to numbers >= 10 first. `dup2(fd, 3)` would clobber the other pipe end if it
    // happened to be fd 3, and `dup2(3, 3)` is a no-op that leaves `FD_CLOEXEC` set, so the browser
    // would start without its pipe.
    let reads = duplicate_above_floor(&child_reads)?;
    let writes = duplicate_above_floor(&child_writes)?;
    drop(child_reads);
    drop(child_writes);
    let reads_fd = reads.as_raw_fd();
    let writes_fd = writes.as_raw_fd();
    let parent_pid = libc::pid_t::try_from(std::process::id()).map_err(io::Error::other)?;

    // SAFETY: `pre_exec` runs the closure in the forked child before `execve`. The closure only
    // calls async-signal-safe functions (`dup2`, `prctl`, `getppid`) and allocates nothing.
    // `reads_fd` and `writes_fd` stay open in the parent until `spawn` returns, so they are valid
    // in the child. `dup2` clears `FD_CLOEXEC` on the targets 3 and 4, so exactly those two pipe
    // ends survive `execve`; the sources keep `FD_CLOEXEC` and close.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(reads_fd, 3) == -1 || libc::dup2(writes_fd, 4) == -1 {
                return Err(io::Error::last_os_error());
            }
            // If this service dies (crash, OOM kill), the kernel sends SIGKILL to the browser. The
            // browser's own children exit when their IPC channel to it closes.
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            ) == -1
            {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died between `fork` and `prctl`; then the signal never comes.
            if libc::getppid() != parent_pid {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
    let child = command.spawn();
    drop(reads);
    drop(writes);
    child
}

fn duplicate_above_floor(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` only reads `fd`, which `OwnedFd` guarantees is open.
    let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, HIGH_FD_FLOOR) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fcntl` returned a new open fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Send `SIGKILL` to every process in process group `pgid`.
///
/// `pgid` must be the pid of a child spawned with `process_group(0)` that has not been reaped yet:
/// an unreaped leader keeps the group id reserved, so the signal cannot reach a recycled group.
/// Values of 0 and 1 are refused, because `kill(0)` targets this service's own group and
/// `kill(-1)` targets every process the service may signal.
pub(crate) fn kill_process_group(pgid: u32) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(pgid).map_err(io::Error::other)?;
    if pgid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to signal process group 0 or 1",
        ));
    }
    // SAFETY: `kill` has no memory-safety preconditions; the argument checks above prevent the
    // broadcast forms.
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        // ESRCH: every member already exited.
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}
