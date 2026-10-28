//! Injected sources of random bytes.

use std::{fmt, sync::Mutex};

use crate::error::AppError;

/// Injected source of random bytes.
///
/// Fallible on purpose: an OS entropy failure must surface as [`AppError::Internal`]
/// rather than a panic, and every caller already returns `Result<_, AppError>`.
pub trait RandomSource: Send + Sync + fmt::Debug {
    /// Fill `dest` with random bytes.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the underlying entropy source fails.
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError>;
}

/// Production randomness backed by the operating system CSPRNG.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        getrandom::fill(dest).map_err(|_| AppError::Internal)
    }
}

/// Deterministic randomness for tests: a seeded xorshift64 stream.
#[derive(Debug)]
pub struct SeededRandom {
    state: Mutex<u64>,
}

impl SeededRandom {
    /// Create a stream from `seed`. A zero seed is remapped because xorshift fixes zero.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: Mutex::new(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            }),
        }
    }
}

impl Default for SeededRandom {
    fn default() -> Self {
        Self::new(0x2026_0720_0000_0001)
    }
}

impl RandomSource for SeededRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for slot in dest.iter_mut() {
            let mut next = *state;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            *state = next;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an eight-bit slice of the state is the intended output"
            )]
            let byte = (next >> 24) as u8;
            *slot = byte;
        }
        Ok(())
    }
}

/// Randomness that replays a fixed byte script, cycling when exhausted.
///
/// Useful for pinning one exact generated identifier in a test.
#[derive(Debug)]
pub struct ScriptedRandom {
    script: Vec<u8>,
    cursor: Mutex<usize>,
}

impl ScriptedRandom {
    /// Create a source replaying `script`. An empty script yields zero bytes.
    #[must_use]
    pub fn new(script: impl Into<Vec<u8>>) -> Self {
        Self {
            script: script.into(),
            cursor: Mutex::new(0),
        }
    }
}

impl RandomSource for ScriptedRandom {
    fn fill_bytes(&self, dest: &mut [u8]) -> Result<(), AppError> {
        if self.script.is_empty() {
            dest.fill(0);
            return Ok(());
        }
        let mut cursor = self
            .cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for slot in dest.iter_mut() {
            *slot = self.script[*cursor % self.script.len()];
            *cursor = cursor.wrapping_add(1);
        }
        Ok(())
    }
}
