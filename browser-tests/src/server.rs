// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! The artifact-mcp server under test: the release binary from `ARTIFACT_MCP_BIN`, started on a
//! free loopback port with its own temporary data directory and the CI `browser` job's
//! environment (trusted access headers, an admin email, raised ingress limits, test audit and
//! webhook keys). Nothing from the caller's environment reaches it except `PATH`, `TMPDIR`,
//! `LANG`, `TZ`, `RUST_LOG` and `RUST_BACKTRACE`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Error;
use crate::http::Client;

/// The administrator identity of the test server (`ADMIN_EMAILS`).
pub const ADMIN_EMAIL: &str = "admin@example.test";

/// The 32-byte test key the CI job uses for `AUDIT_LEDGER_HMAC_KEY` and `WEBHOOK_ENC_KEY`.
pub const TEST_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// Prefix of the server's temporary directories under `$TMPDIR`.
pub const SERVER_DIR_PREFIX: &str = "artifact-mcp-e2e-";

const PASSED_THROUGH: &[&str] = &["PATH", "TMPDIR", "LANG", "TZ", "RUST_LOG", "RUST_BACKTRACE"];

/// The server binary from `ARTIFACT_MCP_BIN`. `Ok(None)` (skip, after printing why) when it is
/// not set and `CI` is not set; an error when `CI` is set and it is missing, or when it does not
/// name a file.
pub fn binary_from_env() -> Result<Option<PathBuf>, String> {
    match std::env::var_os("ARTIFACT_MCP_BIN") {
        Some(path) if !path.is_empty() => {
            let path = PathBuf::from(path);
            if path.is_file() {
                Ok(Some(path))
            } else {
                Err(format!(
                    "ARTIFACT_MCP_BIN={} is not a file; build it with `cargo build --release --locked`",
                    path.display()
                ))
            }
        }
        _ if std::env::var_os("CI").is_some() => Err(
            "ARTIFACT_MCP_BIN must be set in CI; end-to-end tests may not be skipped there".into(),
        ),
        _ => {
            eprintln!("skipping end-to-end tests: ARTIFACT_MCP_BIN is not set");
            Ok(None)
        }
    }
}

/// How to start the server.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub binary: PathBuf,
    pub admin_email: String,
    /// Extra variables, after (and overriding) the defaults.
    pub env: Vec<(String, String)>,
    /// How long to wait for `/health`.
    pub startup_timeout: Duration,
}

impl ServerOptions {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            admin_email: ADMIN_EMAIL.to_owned(),
            env: Vec::new(),
            startup_timeout: Duration::from_secs(60),
        }
    }
}

/// A running server. Dropping it kills the process and removes its data directory.
pub struct Server {
    child: Option<Child>,
    port: u16,
    base_url: String,
    dir: PathBuf,
}

fn free_port() -> Result<u16, Error> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|error| Error::Launch(format!("no free loopback port: {error}")))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| Error::Launch(format!("no free loopback port: {error}")))
}

fn temp_dir() -> Result<PathBuf, Error> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    for attempt in 0..100 {
        let dir = std::env::temp_dir().join(format!(
            "{SERVER_DIR_PREFIX}{}-{nanos}-{attempt}",
            std::process::id()
        ));
        if std::fs::create_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    Err(Error::Launch(
        "could not create a temporary directory".into(),
    ))
}

impl Server {
    /// Start the server and wait until `/health` answers. A port taken between choosing and
    /// binding it is retried with another port.
    pub async fn start(options: &ServerOptions) -> Result<Self, Error> {
        let mut last = String::new();
        for _ in 0..3 {
            let dir = temp_dir()?;
            let port = free_port()?;
            let mut server = Self {
                child: None,
                port,
                base_url: format!("http://127.0.0.1:{port}"),
                dir,
            };
            match server.spawn(options).await {
                Ok(()) => return Ok(server),
                Err(error) => {
                    last = format!("{error}\n--- server log ---\n{}", server.log_tail(60));
                    server.stop().await;
                }
            }
        }
        Err(Error::Launch(format!(
            "{} did not start: {last}",
            options.binary.display()
        )))
    }

    async fn spawn(&mut self, options: &ServerOptions) -> Result<(), Error> {
        let data = self.dir.join("data");
        std::fs::create_dir_all(&data)
            .map_err(|error| Error::Launch(format!("{}: {error}", data.display())))?;
        let log = File::create(self.log_path())
            .map_err(|error| Error::Launch(format!("server log: {error}")))?;
        let log_err = log
            .try_clone()
            .map_err(|error| Error::Launch(format!("server log: {error}")))?;
        let mut command = Command::new(&options.binary);
        command.env_clear();
        for name in PASSED_THROUGH {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let port = self.port.to_string();
        let ingress = [
            "INGRESS_READS_PER_WINDOW",
            "INGRESS_MUTATIONS_PER_WINDOW",
            "INGRESS_MCP_PER_WINDOW",
            "INGRESS_UPLOADS_PER_WINDOW",
            "INGRESS_FEEDBACK_PER_WINDOW",
            "INGRESS_STATE_PER_WINDOW",
            "INGRESS_ADMIN_PER_WINDOW",
            "INGRESS_VERIFIED_VIEWERS_PER_WINDOW",
            "INGRESS_SHARES_PER_WINDOW",
            "INGRESS_AUTH_FAILURES_PER_WINDOW",
        ];
        command
            .env("DATA_DIR", &data)
            .env("PORT", &port)
            .env("LISTEN_HOST", "127.0.0.1")
            .env("TRUST_ACCESS_HEADERS", "1")
            .env("ADMIN_EMAILS", &options.admin_email)
            .env("NODE_ENV", "test")
            .env("PUBLIC_BASE_URL", &self.base_url)
            .env("PREVIEW_RENDERER_URL", "")
            .env("AUDIT_LEDGER_HMAC_KEY", TEST_KEY)
            .env("WEBHOOK_ENC_KEY", TEST_KEY);
        for name in ingress {
            command.env(name, "10000");
        }
        for (name, value) in &options.env {
            command.env(name, value);
        }
        command
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        let child = command
            .spawn()
            .map_err(|error| Error::Launch(format!("{}: {error}", options.binary.display())))?;
        self.child = Some(child);
        let client = Client::new(&self.base_url, &[]);
        let deadline = tokio::time::Instant::now() + options.startup_timeout;
        loop {
            if let Some(child) = &mut self.child
                && let Ok(Some(status)) = child.try_wait()
            {
                return Err(Error::Launch(format!("the server exited: {status}")));
            }
            if let Ok(Ok(response)) =
                tokio::time::timeout(Duration::from_secs(2), client.get("/health")).await
                && response.ok()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout {
                    what: "the server's /health".into(),
                    after: options.startup_timeout,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The server's stdout and stderr.
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("server.log")
    }

    /// The last `lines` lines of the server log.
    pub fn log_tail(&self, lines: usize) -> String {
        let text = std::fs::read_to_string(self.log_path()).unwrap_or_default();
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    /// Whether the process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }

    /// Kill the server, wait for it and remove its directory.
    pub async fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        remove_dir(&self.dir);
    }
}

fn remove_dir(dir: &Path) {
    if dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(SERVER_DIR_PREFIX))
    {
        let _ = std::fs::remove_dir_all(dir);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}
