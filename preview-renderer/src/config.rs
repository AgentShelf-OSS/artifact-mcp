// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Service configuration from the environment.
//!
//! `PORT`, `RENDER_TIMEOUT_MS` and `MAX_BODY_BYTES` keep the Node renderer's names, defaults and
//! parsing rules (see [`crate::jsnum`]). `CHROME_HEADLESS_SHELL` is new and required: the service
//! refuses to start without an absolute path to an executable `chrome-headless-shell`.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use crate::jsnum::env_positive_integer;

pub const DEFAULT_PORT: u64 = 3000;
pub const DEFAULT_RENDER_TIMEOUT_MS: u64 = 7000;
pub const DEFAULT_MAX_BODY_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_WIDTH: u32 = 1200;
pub const DEFAULT_HEIGHT: u32 = 630;
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;
/// The device scale factor every render uses, so a `width`×`height` request yields a PNG of
/// `2·width`×`2·height` pixels.
pub const DEVICE_SCALE_FACTOR: u32 = 2;

/// Name of the variable that holds the browser path.
pub const CHROME_ENV: &str = "CHROME_HEADLESS_SHELL";

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub render_timeout: Duration,
    pub max_body_bytes: usize,
    pub chrome: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Read the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| {
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
        })
    }

    /// Read configuration through `lookup`, which returns a variable's value or `None` if unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let port = port_from(lookup("PORT").as_deref())?;
        let render_timeout_ms = env_positive_integer(
            lookup("RENDER_TIMEOUT_MS").as_deref(),
            DEFAULT_RENDER_TIMEOUT_MS,
        );
        let max_body_bytes =
            env_positive_integer(lookup("MAX_BODY_BYTES").as_deref(), DEFAULT_MAX_BODY_BYTES);
        let chrome = chrome_from(lookup(CHROME_ENV))?;
        Ok(Self {
            port,
            render_timeout: Duration::from_millis(render_timeout_ms),
            max_body_bytes: usize::try_from(max_body_bytes).unwrap_or(usize::MAX),
            chrome,
        })
    }

    /// Node's `server.requestTimeout`: the whole request must arrive within this time.
    pub fn request_timeout(&self) -> Duration {
        self.render_timeout
            .saturating_add(Duration::from_millis(2000))
    }

    /// Node's `server.headersTimeout`.
    pub fn headers_timeout(&self) -> Duration {
        self.render_timeout
            .saturating_add(Duration::from_millis(3000))
    }
}

/// `PORT` with the Node parsing rules. Node failed at `listen` for a port above 65535; this fails
/// at startup instead.
pub fn port_from(value: Option<&str>) -> Result<u16, ConfigError> {
    let port = env_positive_integer(value, DEFAULT_PORT);
    u16::try_from(port)
        .map_err(|_| ConfigError(format!("PORT must be between 1 and 65535, got {port}")))
}

fn chrome_from(value: Option<String>) -> Result<PathBuf, ConfigError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Err(ConfigError(format!(
            "{CHROME_ENV} must be set to the absolute path of chrome-headless-shell"
        )));
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(ConfigError(format!(
            "{CHROME_ENV} must be an absolute path, got {}",
            path.display()
        )));
    }
    let metadata = std::fs::metadata(&path).map_err(|error| {
        ConfigError(format!(
            "{CHROME_ENV} ({}) cannot be read: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ConfigError(format!(
            "{CHROME_ENV} ({}) is not an executable file",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn defaults_match_the_node_renderer() {
        let config = Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh")])).unwrap();
        assert_eq!(config.port, 3000);
        assert_eq!(config.render_timeout, Duration::from_millis(7000));
        assert_eq!(config.max_body_bytes, 5 * 1024 * 1024);
        assert_eq!(config.request_timeout(), Duration::from_millis(9000));
        assert_eq!(config.headers_timeout(), Duration::from_millis(10_000));
    }

    #[test]
    fn values_use_javascript_number_rules() {
        let config = Config::from_lookup(lookup(&[
            (CHROME_ENV, "/bin/sh"),
            ("PORT", " 0x1F90 "),
            ("RENDER_TIMEOUT_MS", "2.5"),
            ("MAX_BODY_BYTES", "1e3"),
        ]))
        .unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(
            config.render_timeout,
            Duration::from_millis(7000),
            "a fraction falls back"
        );
        assert_eq!(config.max_body_bytes, 1000);
    }

    #[test]
    fn invalid_port_fails_fast() {
        assert!(
            Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh"), ("PORT", "70000")])).is_err()
        );
        assert_eq!(
            port_from(Some("-1")),
            Ok(3000),
            "a negative port falls back like Node"
        );
    }

    #[test]
    fn chrome_path_is_required_absolute_and_executable() {
        assert!(Config::from_lookup(lookup(&[])).is_err());
        assert!(Config::from_lookup(lookup(&[(CHROME_ENV, "")])).is_err());
        assert!(Config::from_lookup(lookup(&[(CHROME_ENV, "sh")])).is_err());
        assert!(
            Config::from_lookup(lookup(&[(
                CHROME_ENV,
                "/nonexistent/chrome-headless-shell"
            )]))
            .is_err()
        );
        assert!(Config::from_lookup(lookup(&[(CHROME_ENV, "/etc/hostname")])).is_err());
        assert!(Config::from_lookup(lookup(&[(CHROME_ENV, "/")])).is_err());
    }
}
