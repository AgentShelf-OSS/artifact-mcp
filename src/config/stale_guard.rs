//! Stale-base guard (ADR-0023) configuration.

use super::{defaults::MAX_SAFE_INTEGER, env::EnvSource, parse::present};

/// `STALE_GUARD` default floor of lost segments.
pub const DEFAULT_STALE_GUARD_MIN_SEGMENTS: u64 = 20;
/// `STALE_GUARD_MIN_SHARE` default share of newer segments.
pub const DEFAULT_STALE_GUARD_MIN_SHARE: f64 = 0.10;

/// `STALE_GUARD=warn|enforce|off`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StaleGuardMode {
    #[default]
    Warn,
    Enforce,
    Off,
}

impl StaleGuardMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Enforce => "enforce",
            Self::Off => "off",
        }
    }
}

/// ADR-0023 stale-base guard settings.
#[derive(Clone, Copy, Debug)]
pub struct StaleGuardConfig {
    /// `STALE_GUARD` — default `warn`.
    pub mode: StaleGuardMode,
    /// `STALE_GUARD_MIN_SEGMENTS` — positive integer, default 20.
    pub min_segments: u64,
    /// `STALE_GUARD_MIN_SHARE` — float in (0, 1], default 0.10.
    pub min_share: f64,
}

impl PartialEq for StaleGuardConfig {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.min_segments == other.min_segments
            && self.min_share.to_bits() == other.min_share.to_bits()
    }
}

// `min_share` is always a finite, validated number, so bitwise equality is total.
impl Eq for StaleGuardConfig {}

impl Default for StaleGuardConfig {
    fn default() -> Self {
        Self {
            mode: StaleGuardMode::Warn,
            min_segments: DEFAULT_STALE_GUARD_MIN_SEGMENTS,
            min_share: DEFAULT_STALE_GUARD_MIN_SHARE,
        }
    }
}

impl StaleGuardConfig {
    /// Node parity: an unusable value falls back silently to its default; it never fails startup.
    pub(super) fn from_source(env: &dyn EnvSource) -> Self {
        let mode = match present(env, "STALE_GUARD")
            .map(|raw| raw.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("enforce") => StaleGuardMode::Enforce,
            Some("off") => StaleGuardMode::Off,
            _ => StaleGuardMode::Warn,
        };
        let min_segments = present(env, "STALE_GUARD_MIN_SEGMENTS")
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
            .unwrap_or(DEFAULT_STALE_GUARD_MIN_SEGMENTS);
        let min_share = present(env, "STALE_GUARD_MIN_SHARE")
            .and_then(|raw| raw.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1.0)
            .unwrap_or(DEFAULT_STALE_GUARD_MIN_SHARE);
        Self {
            mode,
            min_segments,
            min_share,
        }
    }
}
