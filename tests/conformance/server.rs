// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
// Rust production-server driver: start the real server binary against a caller-owned DATA_DIR
// and PORT, wait for /health, and stop it cleanly (SIGTERM, then SIGKILL after 3 s).

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::http::{self, Request};

const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_GRACE: Duration = Duration::from_secs(3);

pub struct Server {
    child: Option<Child>,
    port: u16,
    logs: Arc<Mutex<Vec<u8>>>,
    pumps: Vec<JoinHandle<()>>,
}

fn describe_exit(status: ExitStatus) -> String {
    let code = status
        .code()
        .map_or_else(|| "null".to_owned(), |code| code.to_string());
    let signal = status
        .signal()
        .map_or_else(|| "null".to_owned(), |signal| format!("signal {signal}"));
    format!("code={code} signal={signal}")
}

fn pump(mut source: impl Read + Send + 'static, logs: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        while let Ok(read) = source.read(&mut buffer) {
            if read == 0 {
                break;
            }
            logs.lock()
                .expect("server log buffer")
                .extend_from_slice(&buffer[..read]);
        }
    })
}

/// Poll /health until it returns 200 or the deadline passes.
pub fn wait_for_health(
    port: u16,
    mut exited: impl FnMut() -> Option<String>,
) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    let mut last_error = String::from("no attempt completed");
    while Instant::now() < deadline {
        if let Some(exit) = exited() {
            return Err(format!("Rust server exited before health: {exit}"));
        }
        let request = Request {
            method: "GET",
            path: "/health",
            headers: &[],
            body: None,
        };
        match http::send(port, &request, HEALTH_REQUEST_TIMEOUT) {
            Ok(response) if response.status == 200 => return Ok(()),
            Ok(response) => last_error = format!("/health returned {}", response.status),
            Err(error) => last_error = error,
        }
        thread::sleep(HEALTH_INTERVAL);
    }
    Err(format!(
        "Server did not become healthy at http://127.0.0.1:{port}: {last_error}"
    ))
}

impl Server {
    /// Start the binary with only PATH and HOME inherited, plus `env`, DATA_DIR and PORT.
    /// A spawn failure, an early exit, or a missed health deadline returns the server output.
    pub fn start(
        binary: &Path,
        cwd: &Path,
        data_dir: &Path,
        port: u16,
        env: &[(String, String)],
    ) -> Result<Self, String> {
        let mut command = Command::new(binary);
        command.current_dir(cwd).env_clear();
        for inherited in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(inherited) {
                command.env(inherited, value);
            }
        }
        for (name, value) in env {
            command.env(name, value);
        }
        command
            .env("DATA_DIR", data_dir)
            .env("PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start {}: {error}", binary.display()))?;
        let logs = Arc::new(Mutex::new(Vec::new()));
        let mut pumps = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            pumps.push(pump(stdout, Arc::clone(&logs)));
        }
        if let Some(stderr) = child.stderr.take() {
            pumps.push(pump(stderr, Arc::clone(&logs)));
        }
        let mut server = Self {
            child: Some(child),
            port,
            logs,
            pumps,
        };
        let health = {
            let child = server.child.as_mut().expect("child just started");
            wait_for_health(port, || child.try_wait().ok().flatten().map(describe_exit))
        };
        if let Err(error) = health {
            server.stop();
            let output = server.logs();
            let output = output.trim();
            return Err(if output.is_empty() {
                error
            } else {
                format!("{error}\nRust server output:\n{output}")
            });
        }
        Ok(server)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn logs(&self) -> String {
        String::from_utf8_lossy(&self.logs.lock().expect("server log buffer")).into_owned()
    }

    /// Stop cleanly. Idempotent.
    pub fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let terminated = Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            let deadline = Instant::now() + STOP_GRACE;
            loop {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if !terminated || Instant::now() >= deadline {
                    let _ = child.kill();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = child.wait();
        }
        for pump in self.pumps.drain(..) {
            let _ = pump.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}
