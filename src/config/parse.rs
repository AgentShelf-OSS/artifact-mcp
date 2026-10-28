//! Shared environment parsing helpers.

use crate::error::AppError;

use super::{defaults::MAX_SAFE_INTEGER, env::EnvSource};

pub(super) fn invalid(key: &str, raw: &str, expectation: &str) -> AppError {
    AppError::Validation(format!("{key} must be {expectation} (got \"{raw}\")"))
}

/// Fetch a variable, treating "absent" and "empty after trimming" identically.
///
/// Node reaches the same conclusion for both: `"" || fallback` and
/// `Number.isSafeInteger(Number("")) && 0 > 0` both select the fallback.
pub(super) fn present(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key).filter(|value| !value.trim().is_empty())
}

/// Port of `nonEmptyString(value, fallback)` — [lib/config.js:8-11]
pub(super) fn non_empty_string(env: &dyn EnvSource, key: &str, default: &str) -> String {
    present(env, key).map_or_else(|| default.to_owned(), |value| value.trim().to_owned())
}

/// Port of `value || fallback` for strings Node does **not** trim. [server.js:28], [lib/db.js:8]
pub(super) fn raw_string(env: &dyn EnvSource, key: &str, default: &str) -> String {
    env.get(key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Port of `positiveInteger(value, fallback)` — [lib/config.js:3-6]
///
/// Accepts exactly the decimal integers Node accepts; rejects (rather than silently
/// discarding) anything else that is actually present.
pub(super) fn positive_integer(
    env: &dyn EnvSource,
    key: &str,
    default: u64,
) -> Result<u64, AppError> {
    let Some(raw) = present(env, key) else {
        return Ok(default);
    };
    let trimmed = raw.trim();
    let parsed: u64 = trimmed
        .parse()
        .map_err(|_| invalid(key, trimmed, "a positive integer"))?;
    if parsed == 0 || parsed > MAX_SAFE_INTEGER {
        return Err(invalid(key, trimmed, "a positive safe integer"));
    }
    Ok(parsed)
}

/// Port of `process.env.X === "1"` — [lib/identity.js:16,57,68]
pub(super) fn flag(env: &dyn EnvSource, key: &str) -> bool {
    env.get(key).is_some_and(|value| value == "1")
}

pub(super) fn enabled_unless_zero(env: &dyn EnvSource, key: &str) -> Result<bool, AppError> {
    match present(env, key).as_deref().map(str::trim) {
        None | Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(raw) => Err(invalid(key, raw, "\"0\" or \"1\"")),
    }
}

/// Port of the `bytes` package's `parse()` used by `express.json({ limit })`.
///
/// `bytes.parse` matches `/^((-|\+)?(\d+(?:\.\d+)?)) *(kb|mb|gb|tb|pb)$/i` and otherwise
/// falls back to `parseInt(val, 10)`, then floors `unit * value`. Only the accepting
/// branches are reproduced; unparsable values fail closed.
pub(super) fn parse_byte_size(key: &str, raw: &str) -> Result<u64, AppError> {
    let trimmed = raw.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let (number, multiplier) = split_byte_size(&lowered);
    let value: f64 = number
        .parse()
        .map_err(|_| invalid(key, trimmed, "a byte size such as \"8mb\" or \"1048576\""))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(invalid(key, trimmed, "a positive byte size"));
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte-size multipliers are exact in f64 up to 1 PiB"
    )]
    let bytes = (value * multiplier as f64).floor();
    #[expect(
        clippy::cast_precision_loss,
        reason = "MAX_SAFE_INTEGER is exactly representable in f64"
    )]
    let ceiling = MAX_SAFE_INTEGER as f64;
    if bytes < 1.0 || bytes > ceiling {
        return Err(invalid(key, trimmed, "a positive byte size"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bytes is a floored, positive value bounded by MAX_SAFE_INTEGER"
    )]
    let bytes = bytes as u64;
    Ok(bytes)
}

pub(super) fn split_byte_size(lowered: &str) -> (&str, u64) {
    const UNITS: [(&str, u64); 6] = [
        ("pb", 1024 * 1024 * 1024 * 1024 * 1024),
        ("tb", 1024 * 1024 * 1024 * 1024),
        ("gb", 1024 * 1024 * 1024),
        ("mb", 1024 * 1024),
        ("kb", 1024),
        ("b", 1),
    ];
    for (suffix, multiplier) in UNITS {
        if let Some(head) = lowered.strip_suffix(suffix) {
            return (head.trim_end(), multiplier);
        }
    }
    (lowered, 1)
}

pub(super) fn disabled_unless_one(env: &dyn EnvSource, key: &str) -> Result<bool, AppError> {
    match present(env, key).as_deref().map(str::trim) {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(raw) => Err(invalid(key, raw, "\"0\" or \"1\"")),
    }
}
