// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Service configuration from the environment.
//!
//! `PORT`, `RENDER_TIMEOUT_MS` and `MAX_BODY_BYTES` keep the Node renderer's names, defaults and
//! parsing rules (see [`crate::jsnum`]). `CHROME_HEADLESS_SHELL` is new and required: the service
//! refuses to start without an absolute path to an executable `chrome-headless-shell`.
//! `CHROME_SANDBOX` is optional and strict: only `1`, `true`, `0` and `false` are accepted.
//! `RENDER_TOKEN` or `RENDER_TOKEN_FILE` optionally sets the shared bearer token that
//! `POST /render` then requires (see [`RenderToken`]).

use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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

/// Name of the variable that keeps Chromium's own sandbox (no `--no-sandbox`).
pub const SANDBOX_ENV: &str = "CHROME_SANDBOX";

/// Name of the variable that holds the shared bearer token.
pub const TOKEN_ENV: &str = "RENDER_TOKEN";
/// Name of the variable that holds the path of a file with the shared bearer token.
pub const TOKEN_FILE_ENV: &str = "RENDER_TOKEN_FILE";
/// Shortest accepted token, in bytes.
pub const MIN_TOKEN_BYTES: usize = 32;
/// Longest accepted token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub render_timeout: Duration,
    pub max_body_bytes: usize,
    pub chrome: PathBuf,
    /// `CHROME_SANDBOX`: start the browser without `--no-sandbox`. Off by default; it needs
    /// unprivileged user namespaces, and Chromium refuses it when it runs as root.
    pub chrome_sandbox: bool,
    /// `RENDER_TOKEN` or `RENDER_TOKEN_FILE`. When set, `POST /render` requires
    /// `authorization: Bearer <token>`. `None` keeps the open contract.
    pub render_token: Option<RenderToken>,
}

/// The shared bearer token between the server and the renderer. `Debug` never shows it, and no
/// error message contains it.
#[derive(Clone, PartialEq, Eq)]
pub struct RenderToken(String);

impl RenderToken {
    /// The token. Callers must not log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The `authorization` header value that carries this token.
    pub fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }

    /// `RENDER_TOKEN` or `RENDER_TOKEN_FILE` from the process environment.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|name| {
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
        })
    }

    /// `RENDER_TOKEN` or `RENDER_TOKEN_FILE` through `lookup`. An empty value counts as unset.
    /// Setting both is an error. The file is read once, here; one trailing newline is removed.
    /// The token must be [`MIN_TOKEN_BYTES`] to [`MAX_TOKEN_BYTES`] printable ASCII characters
    /// without spaces.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        let value = lookup(TOKEN_ENV).filter(|value| !value.is_empty());
        let file = lookup(TOKEN_FILE_ENV).filter(|value| !value.is_empty());
        match (value, file) {
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(ConfigError(format!(
                "set {TOKEN_ENV} or {TOKEN_FILE_ENV}, not both"
            ))),
            (Some(value), None) => Self::validated(TOKEN_ENV, value.into_bytes()).map(Some),
            (None, Some(path)) => {
                let bytes = read_token_file(Path::new(&path))?;
                Self::validated(TOKEN_FILE_ENV, bytes).map(Some)
            }
        }
    }

    fn validated(source: &str, bytes: Vec<u8>) -> Result<Self, ConfigError> {
        let printable = bytes.iter().all(|byte| byte.is_ascii_graphic());
        if !(MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&bytes.len()) || !printable {
            return Err(ConfigError(format!(
                "{source}: the token must be {MIN_TOKEN_BYTES} to {MAX_TOKEN_BYTES} printable \
                 ASCII characters without spaces"
            )));
        }
        // All bytes are ASCII, so this cannot fail.
        String::from_utf8(bytes)
            .map(Self)
            .map_err(|_| ConfigError(format!("{source}: the token is not ASCII")))
    }
}

impl fmt::Debug for RenderToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RenderToken(<redacted>)")
    }
}

/// The token file's bytes without one trailing newline (`\n` or `\r\n`). Reads at most a few
/// bytes more than the longest token, so a wrong path cannot make startup read a huge file.
fn read_token_file(path: &Path) -> Result<Vec<u8>, ConfigError> {
    let cannot_read = |error: std::io::Error| {
        ConfigError(format!(
            "{TOKEN_FILE_ENV} ({}) cannot be read: {error}",
            path.display()
        ))
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(cannot_read)?
        .take(MAX_TOKEN_BYTES as u64 + 3)
        .read_to_end(&mut bytes)
        .map_err(cannot_read)?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    Ok(bytes)
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
        let chrome_sandbox = sandbox_from(lookup(SANDBOX_ENV).as_deref())?;
        let render_token = RenderToken::from_lookup(&lookup)?;
        Ok(Self {
            port,
            render_timeout: Duration::from_millis(render_timeout_ms),
            max_body_bytes: usize::try_from(max_body_bytes).unwrap_or(usize::MAX),
            chrome,
            chrome_sandbox,
            render_token,
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

/// `CHROME_SANDBOX`: unset or empty is off. Any value other than `1`, `true`, `0` or `false`
/// stops the service at startup, so a typo cannot silently leave the sandbox off.
fn sandbox_from(value: Option<&str>) -> Result<bool, ConfigError> {
    match value {
        None | Some("" | "0" | "false") => Ok(false),
        Some("1" | "true") => Ok(true),
        Some(other) => Err(ConfigError(format!(
            "{SANDBOX_ENV} must be 1, true, 0 or false, got {other:?}"
        ))),
    }
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
        assert!(!config.chrome_sandbox, "the sandbox is off by default");
    }

    #[test]
    fn chrome_sandbox_accepts_four_values_and_rejects_the_rest() {
        let sandbox = |value: &str| {
            Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh"), (SANDBOX_ENV, value)]))
                .map(|config| config.chrome_sandbox)
        };
        assert_eq!(sandbox("1"), Ok(true));
        assert_eq!(sandbox("true"), Ok(true));
        assert_eq!(sandbox("0"), Ok(false));
        assert_eq!(sandbox("false"), Ok(false));
        assert_eq!(sandbox(""), Ok(false), "empty is unset");
        for bad in ["yes", "on", "TRUE", "True", "2", " 1", "1 ", "true\n", "-1"] {
            let error = sandbox(bad).unwrap_err();
            assert!(
                error.0.starts_with("CHROME_SANDBOX must be"),
                "{bad:?}: {error}"
            );
        }
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

    const TOKEN: &str = "0123456789abcdef0123456789ABCDEF-token";

    fn token_from(pairs: &[(&str, &str)]) -> Result<Option<RenderToken>, ConfigError> {
        RenderToken::from_lookup(lookup(pairs))
    }

    fn token_file(name: &str, contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "artifact-preview-token-test-{}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn no_token_is_the_default_and_empty_means_unset() {
        let config = Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh")])).unwrap();
        assert_eq!(config.render_token, None);
        assert_eq!(
            token_from(&[(TOKEN_ENV, ""), (TOKEN_FILE_ENV, "")]),
            Ok(None)
        );
    }

    #[test]
    fn render_token_is_validated() {
        let config =
            Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh"), (TOKEN_ENV, TOKEN)])).unwrap();
        assert_eq!(config.render_token.unwrap().expose(), TOKEN);
        let shortest = "x".repeat(MIN_TOKEN_BYTES);
        let longest = "~".repeat(MAX_TOKEN_BYTES);
        assert!(token_from(&[(TOKEN_ENV, &shortest)]).unwrap().is_some());
        assert!(token_from(&[(TOKEN_ENV, &longest)]).unwrap().is_some());
        let too_short = "x".repeat(MIN_TOKEN_BYTES - 1);
        let too_long = "x".repeat(MAX_TOKEN_BYTES + 1);
        let with_space = format!("{} {}", &TOKEN[..20], &TOKEN[20..]);
        let with_newline = format!("{TOKEN}\n");
        let with_tab = format!("{TOKEN}\t");
        let non_ascii = format!("{TOKEN}é");
        for bad in [
            too_short.as_str(),
            &too_long,
            &with_space,
            &with_newline,
            &with_tab,
            &non_ascii,
        ] {
            let error = token_from(&[(TOKEN_ENV, bad)]).unwrap_err();
            assert_eq!(
                error.0,
                "RENDER_TOKEN: the token must be 32 to 4096 printable ASCII characters without spaces"
            );
            assert!(
                !error.0.contains(&bad[..10]),
                "the error must not echo the token"
            );
        }
    }

    #[test]
    fn render_token_and_render_token_file_are_mutually_exclusive() {
        let path = token_file("both", TOKEN.as_bytes());
        let error = token_from(&[(TOKEN_ENV, TOKEN), (TOKEN_FILE_ENV, path.to_str().unwrap())])
            .unwrap_err();
        assert_eq!(error.0, "set RENDER_TOKEN or RENDER_TOKEN_FILE, not both");
        assert!(
            Config::from_lookup(lookup(&[
                (CHROME_ENV, "/bin/sh"),
                (TOKEN_ENV, TOKEN),
                (TOKEN_FILE_ENV, path.to_str().unwrap()),
            ]))
            .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn render_token_file_is_read_once_with_one_trailing_newline_removed() {
        for (name, contents) in [
            ("plain", TOKEN.to_owned()),
            ("lf", format!("{TOKEN}\n")),
            ("crlf", format!("{TOKEN}\r\n")),
        ] {
            let path = token_file(name, contents.as_bytes());
            let token = token_from(&[(TOKEN_FILE_ENV, path.to_str().unwrap())])
                .unwrap()
                .unwrap();
            assert_eq!(token.expose(), TOKEN, "{name}");
            std::fs::remove_file(path).unwrap();
        }
        for (name, contents) in [
            ("two-newlines", format!("{TOKEN}\n\n")),
            ("leading-space", format!(" {TOKEN}")),
            ("too-long", "x".repeat(MAX_TOKEN_BYTES + 1)),
            ("huge", "x".repeat(1 << 20)),
            ("empty", String::new()),
            ("newline-only", "\n".to_owned()),
        ] {
            let path = token_file(name, contents.as_bytes());
            let error = token_from(&[(TOKEN_FILE_ENV, path.to_str().unwrap())]).unwrap_err();
            assert!(
                error.0.starts_with("RENDER_TOKEN_FILE: the token must be"),
                "{name}: {error}"
            );
            assert!(
                !error.0.contains(TOKEN),
                "{name}: the error must not echo the token"
            );
            std::fs::remove_file(path).unwrap();
        }
        let error = token_from(&[(TOKEN_FILE_ENV, "/nonexistent/render-token")]).unwrap_err();
        assert!(
            error
                .0
                .starts_with("RENDER_TOKEN_FILE (/nonexistent/render-token) cannot be read"),
            "{error}"
        );
    }

    #[test]
    fn render_token_never_appears_in_debug_output() {
        let config =
            Config::from_lookup(lookup(&[(CHROME_ENV, "/bin/sh"), (TOKEN_ENV, TOKEN)])).unwrap();
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(TOKEN), "{rendered}");
        assert!(rendered.contains("RenderToken(<redacted>)"), "{rendered}");
        assert_eq!(
            config.render_token.unwrap().header_value(),
            format!("Bearer {TOKEN}")
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
