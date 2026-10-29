//! Optional HTML preview renderer configuration.

use std::io::Read as _;

use crate::error::AppError;

use super::{
    defaults::{
        DEFAULT_PREVIEW_CACHE_ENTRIES, DEFAULT_PREVIEW_MAX_PNG_BYTES, DEFAULT_PREVIEW_TIMEOUT_MS,
        DEFAULT_PREVIEW_VIEWPORT_HEIGHT, DEFAULT_PREVIEW_VIEWPORT_WIDTH,
        MAX_PREVIEW_RENDERER_TOKEN_BYTES, MIN_PREVIEW_RENDERER_TOKEN_BYTES,
    },
    env::{EnvSource, Secret},
    parse::{invalid, positive_integer, present},
};

/// Optional HTML preview renderer configuration. [lib/preview.js:38-49], [lib/thumbnails.js:66-72]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewConfig {
    /// `PREVIEW_RENDERER_URL` normalized to the `render` endpoint, or `None` when disabled.
    pub renderer_endpoint: Option<String>,
    /// `PREVIEW_RENDER_TIMEOUT_MS`
    pub timeout_ms: u64,
    /// `PREVIEW_VIEWPORT` width component.
    pub viewport_width: u64,
    /// `PREVIEW_VIEWPORT` height component.
    pub viewport_height: u64,
    /// In-process render cache entries (not environment-driven). [lib/preview.js:8]
    pub cache_entries: u64,
    /// `PREVIEW_MAX_PNG_BYTES`
    pub max_png_bytes: u64,
    /// `PREVIEW_RENDERER_TOKEN` or `PREVIEW_RENDERER_TOKEN_FILE`: the shared bearer token sent as
    /// `authorization: Bearer <token>` with every render request. `None` sends no header. The
    /// value is redacted from `Debug` and never logged.
    pub renderer_token: Option<Secret>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            renderer_endpoint: None,
            timeout_ms: DEFAULT_PREVIEW_TIMEOUT_MS,
            viewport_width: DEFAULT_PREVIEW_VIEWPORT_WIDTH,
            viewport_height: DEFAULT_PREVIEW_VIEWPORT_HEIGHT,
            cache_entries: DEFAULT_PREVIEW_CACHE_ENTRIES,
            max_png_bytes: DEFAULT_PREVIEW_MAX_PNG_BYTES,
            renderer_token: None,
        }
    }
}

impl PreviewConfig {
    /// `true` once a usable renderer endpoint is configured. [lib/preview.js:53]
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.renderer_endpoint.is_some()
    }

    pub(super) fn from_source(env: &dyn EnvSource) -> Result<Self, AppError> {
        let renderer_endpoint = match present(env, "PREVIEW_RENDERER_URL") {
            Some(raw) => Some(renderer_endpoint(&raw)?),
            None => None,
        };
        let (viewport_width, viewport_height) = parse_viewport(present(env, "PREVIEW_VIEWPORT"))?;
        Ok(Self {
            renderer_endpoint,
            timeout_ms: positive_integer(
                env,
                "PREVIEW_RENDER_TIMEOUT_MS",
                DEFAULT_PREVIEW_TIMEOUT_MS,
            )?,
            viewport_width,
            viewport_height,
            cache_entries: DEFAULT_PREVIEW_CACHE_ENTRIES,
            max_png_bytes: positive_integer(
                env,
                "PREVIEW_MAX_PNG_BYTES",
                DEFAULT_PREVIEW_MAX_PNG_BYTES,
            )?,
            renderer_token: parse_preview_renderer_token(env)?,
        })
    }
}

/// `PREVIEW_RENDERER_TOKEN` or `PREVIEW_RENDERER_TOKEN_FILE`, with the renderer's `RENDER_TOKEN`
/// rules: setting both is an error; the file is read once, here, and one trailing newline
/// (`\n` or `\r\n`) is removed; the token must be 32 to 4096 printable ASCII characters without
/// spaces. Values are never trimmed otherwise, so both sides compare the same bytes. No error
/// message contains the token.
fn parse_preview_renderer_token(env: &dyn EnvSource) -> Result<Option<Secret>, AppError> {
    const VALUE: &str = "PREVIEW_RENDERER_TOKEN";
    const FILE: &str = "PREVIEW_RENDERER_TOKEN_FILE";
    let (source, bytes) = match (present(env, VALUE), present(env, FILE)) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(AppError::Validation(format!(
                "Set {VALUE} or {FILE}, not both."
            )));
        }
        (Some(value), None) => (VALUE, value.into_bytes()),
        (None, Some(path)) => (FILE, read_preview_renderer_token_file(FILE, &path)?),
    };
    if !(MIN_PREVIEW_RENDERER_TOKEN_BYTES..=MAX_PREVIEW_RENDERER_TOKEN_BYTES).contains(&bytes.len())
        || !bytes.iter().all(u8::is_ascii_graphic)
    {
        return Err(AppError::Validation(format!(
            "{source} must hold {MIN_PREVIEW_RENDERER_TOKEN_BYTES} to \
             {MAX_PREVIEW_RENDERER_TOKEN_BYTES} printable ASCII characters without spaces."
        )));
    }
    String::from_utf8(bytes)
        .map(|token| Some(Secret::new(token)))
        .map_err(|_| AppError::Validation(format!("{source} must be ASCII.")))
}

/// The file's bytes without one trailing newline. Reads at most a few bytes more than the longest
/// token, so a wrong path cannot make startup read a huge file.
fn read_preview_renderer_token_file(key: &str, path: &str) -> Result<Vec<u8>, AppError> {
    let cannot_read = |error: std::io::Error| {
        AppError::Validation(format!("{key} ({path}) cannot be read: {error}"))
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(cannot_read)?
        .take(MAX_PREVIEW_RENDERER_TOKEN_BYTES as u64 + 3)
        .read_to_end(&mut bytes)
        .map_err(cannot_read)?;
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    Ok(bytes)
}

/// Port of `rendererEndpoint(value)` — [lib/preview.js:25-36]
fn renderer_endpoint(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    let with_slash = if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    };
    let base = url::Url::parse(&with_slash)
        .map_err(|_| invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"))?;
    if !matches!(base.scheme(), "http" | "https") {
        return Err(invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"));
    }
    base.join("render")
        .map(String::from)
        .map_err(|_| invalid("PREVIEW_RENDERER_URL", trimmed, "an http(s) URL"))
}

/// Port of `parseViewport(value)` — [lib/preview.js:16-23]
///
/// Node silently falls back to `1200x630` for a malformed value; a present but malformed
/// value fails closed here instead.
fn parse_viewport(raw: Option<String>) -> Result<(u64, u64), AppError> {
    let Some(raw) = raw else {
        return Ok((
            DEFAULT_PREVIEW_VIEWPORT_WIDTH,
            DEFAULT_PREVIEW_VIEWPORT_HEIGHT,
        ));
    };
    let trimmed = raw.trim();
    let malformed = || {
        invalid(
            "PREVIEW_VIEWPORT",
            trimmed,
            "formatted as \"<width>x<height>\"",
        )
    };
    let lowered = trimmed.to_ascii_lowercase();
    let (width, height) = lowered.split_once('x').ok_or_else(malformed)?;
    if width.is_empty()
        || height.is_empty()
        || !width.bytes().all(|byte| byte.is_ascii_digit())
        || !height.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(malformed());
    }
    let width: u64 = width.parse().map_err(|_| malformed())?;
    let height: u64 = height.parse().map_err(|_| malformed())?;
    // `positiveInteger(match[1], 1200)` restores the default for a zero component.
    Ok((
        if width == 0 {
            DEFAULT_PREVIEW_VIEWPORT_WIDTH
        } else {
            width
        },
        if height == 0 {
            DEFAULT_PREVIEW_VIEWPORT_HEIGHT
        } else {
            height
        },
    ))
}
