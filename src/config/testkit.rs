//! Deterministic adapter bundle for fixtures.

use std::path::Path;

use super::{AppConfig, clock::FixedClock, id_source::SequentialIdSource, random::SeededRandom};

/// The deterministic adapter bundle later units compose into their fixtures.
///
/// Holding the three sources together keeps every native test reproducible without each
/// unit re-deriving its own fakes.
#[derive(Debug, Default)]
pub struct Testkit {
    /// Default configuration, ready to be pointed at a temporary data directory.
    pub config: AppConfig,
    /// A clock pinned to 2026-01-01T00:00:00Z.
    pub clock: FixedClock,
    /// A counter-backed id factory.
    pub ids: SequentialIdSource,
    /// A seeded, reproducible random byte stream.
    pub random: SeededRandom,
}

impl Testkit {
    /// A testkit whose configuration points at `data_dir`.
    #[must_use]
    pub fn with_data_dir(data_dir: &Path) -> Self {
        let mut kit = Self::default();
        kit.config.data_dir = data_dir.to_path_buf();
        kit
    }
}
