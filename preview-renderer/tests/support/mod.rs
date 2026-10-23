// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The service launcher for the integration tests, plus the shared helpers.
//!
//! Browser tests need `CHROME_HEADLESS_SHELL`. Without it they are skipped with a message, unless
//! `CI` is set, in which case they fail: a CI job must never pass by skipping them.
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub mod common;
pub mod vectors;

pub use common::*;

/// A fresh directory for one service's `TMPDIR`.
pub fn scratch_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "artifact-preview-test-{label}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The `artifact-preview` binary, running as a child process on a free loopback port.
pub struct Service {
    child: Child,
    pub authority: String,
    pub tmpdir: PathBuf,
    chrome: PathBuf,
}

impl Service {
    pub fn start(chrome: &Path, label: &str, extra_env: &[(&str, &str)]) -> Self {
        let tmpdir = scratch_dir(label);
        for _ in 0..5 {
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let mut command = Command::new(env!("CARGO_BIN_EXE_artifact-preview"));
            command
                .env_clear()
                .env("PORT", port.to_string())
                .env("CHROME_HEADLESS_SHELL", chrome)
                .env("TMPDIR", &tmpdir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            for (name, value) in extra_env {
                command.env(name, value);
            }
            let mut child = command.spawn().expect("start artifact-preview");
            let authority = format!("127.0.0.1:{port}");
            // Readiness must come from THIS child. Another test's service can bind the same probed
            // port first, so a successful connect alone may reach someone else's server. The
            // service prints this line only after its own bind succeeded.
            let ready_line = format!("[artifact-preview] listening on 0.0.0.0:{port}");
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let stdout = child.stdout.take().expect("piped stdout");
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if line == ready_line {
                        let _ = ready_tx.send(());
                    }
                }
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if ready_rx.try_recv().is_ok() {
                    return Self {
                        child,
                        authority,
                        tmpdir,
                        chrome: chrome.to_owned(),
                    };
                }
                if child.try_wait().unwrap().is_some() {
                    break; // the port was taken between probe and bind; try another
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("artifact-preview did not start");
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Per-render directories currently present under this service's `TMPDIR`.
    pub fn render_dirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.tmpdir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("artifact-preview-"))
            })
            .collect()
    }

    /// Running browser processes: descendants of the service, plus any process started from the
    /// browser executable (which also catches a process that escaped and was re-parented). Callers
    /// must make sure no other test runs a browser from the same executable at the same time.
    pub fn browser_processes(&self) -> Vec<ProcessInfo> {
        let mut found: Vec<ProcessInfo> = process_tree(self.child.id())
            .into_iter()
            .filter(|process| process.pid != self.child.id())
            .collect();
        for process in chrome_processes(&self.chrome) {
            if !found.iter().any(|known| known.pid == process.pid) {
                found.push(process);
            }
        }
        found
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.tmpdir);
    }
}
