//! Environment sources and the redacted secret wrapper.

use std::{collections::BTreeMap, fmt};

#[cfg(doc)]
use super::AppConfig;

/// Read-only environment lookup.
///
/// Configuration is parsed through this seam so native tests never mutate the process
/// environment. `std::env::set_var` is `unsafe` in edition 2024 and would race across
/// cargo's parallel test threads; a map-backed source is both safe and deterministic.
pub trait EnvSource {
    /// Return the raw value for `key`, exactly as the process would see it.
    fn get(&self, key: &str) -> Option<String>;
}

/// Production source backed by the real process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Deterministic in-memory environment used by tests and by [`AppConfig::defaults`].
#[derive(Clone, Debug, Default)]
pub struct MapEnv {
    entries: BTreeMap<String, String>,
}

impl MapEnv {
    /// An environment with no variables set at all.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builder-style setter.
    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.entries.insert(key.to_owned(), value.to_owned());
        self
    }
}

impl<K, V> FromIterator<(K, V)> for MapEnv
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self {
            entries: iter
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.get(key).cloned()
    }
}

/// A configured secret whose `Debug`/`Display` output is always redacted.
///
/// Blueprint A8 requires that error and startup logs never contain bearer keys or
/// webhook credentials; wrapping them here makes leaking one require an explicit
/// [`Secret::expose`] call.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the secret value. Callers must not log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}
