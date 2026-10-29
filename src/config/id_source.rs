//! Generated identifier factories.

use std::fmt;
#[cfg(any(test, feature = "test-util"))]
use std::sync::Mutex;

use crate::{
    error::AppError,
    model::{ArtifactId, FeedbackId, ShareToken, WebhookId},
};

use super::{
    ids::*,
    random::{OsRandom, RandomSource},
};

/// Injected identifier factory covering every generated id shape in the system.
pub trait IdSource: Send + Sync + fmt::Debug {
    /// A new 12-character artifact id. [lib/store.js:30]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn artifact_id(&self) -> Result<ArtifactId, AppError>;

    /// A new 24-character public share token. [lib/shares.js:6]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn share_token(&self) -> Result<ShareToken, AppError>;

    /// A new 16-character feedback id. [lib/feedback.js:10]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn feedback_id(&self) -> Result<FeedbackId, AppError>;

    /// A new 12-character webhook id. [lib/webhooks.js:11]
    ///
    /// # Errors
    /// Propagates a [`RandomSource`] failure as [`AppError::Internal`].
    fn webhook_id(&self) -> Result<WebhookId, AppError>;
}

/// nanoid's masked rejection sampler, reproduced so alphabets stay uniformly distributed.
///
/// # Errors
/// Propagates a [`RandomSource`] failure, or reports an unusable alphabet/length as
/// [`AppError::Internal`].
pub fn generate_id(
    random: &dyn RandomSource,
    alphabet: &str,
    length: usize,
) -> Result<String, AppError> {
    let symbols = alphabet.as_bytes();
    if symbols.is_empty() || !alphabet.is_ascii() || length == 0 {
        return Err(AppError::Internal);
    }
    // nanoid derives its mask from the next power of two, then rejects out-of-range draws.
    let mask =
        u8::try_from(symbols.len().next_power_of_two() - 1).map_err(|_| AppError::Internal)?;
    let mut out = String::with_capacity(length);
    let mut buffer = [0_u8; 64];
    while out.len() < length {
        random.fill_bytes(&mut buffer)?;
        for byte in buffer {
            if out.len() == length {
                break;
            }
            if let Some(&symbol) = symbols.get((byte & mask) as usize) {
                out.push(char::from(symbol));
            }
        }
    }
    Ok(out)
}

/// Production id factory: nanoid alphabets over an injected [`RandomSource`].
#[derive(Debug)]
pub struct NanoIdSource<R: RandomSource> {
    random: R,
}

impl<R: RandomSource> NanoIdSource<R> {
    /// Wrap `random` as an [`IdSource`].
    #[must_use]
    pub const fn new(random: R) -> Self {
        Self { random }
    }
}

impl Default for NanoIdSource<OsRandom> {
    fn default() -> Self {
        Self::new(OsRandom)
    }
}

impl<R: RandomSource> IdSource for NanoIdSource<R> {
    fn artifact_id(&self) -> Result<ArtifactId, AppError> {
        generate_id(&self.random, ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH).map(ArtifactId)
    }

    fn share_token(&self) -> Result<ShareToken, AppError> {
        generate_id(&self.random, SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH).map(ShareToken)
    }

    fn feedback_id(&self) -> Result<FeedbackId, AppError> {
        generate_id(&self.random, FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH).map(FeedbackId)
    }

    fn webhook_id(&self) -> Result<WebhookId, AppError> {
        generate_id(&self.random, WEBHOOK_ID_ALPHABET, WEBHOOK_ID_LENGTH).map(WebhookId)
    }
}

/// Deterministic id factory: a monotonic counter rendered in each target alphabet.
///
/// Output is unique, ordered, and always satisfies the real validators, so fixtures can
/// pin exact identifiers without pinning a random byte stream.
#[derive(Debug, Default)]
#[cfg(any(test, feature = "test-util"))]
pub struct SequentialIdSource {
    counter: Mutex<u64>,
}

#[cfg(any(test, feature = "test-util"))]
impl SequentialIdSource {
    /// Start the counter at `start` so independent fixtures cannot collide.
    #[must_use]
    pub const fn starting_at(start: u64) -> Self {
        Self {
            counter: Mutex::new(start),
        }
    }

    fn next(&self) -> u64 {
        let mut guard = self
            .counter
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = *guard;
        *guard = guard.wrapping_add(1);
        value
    }

    fn render(&self, alphabet: &str, length: usize) -> Result<String, AppError> {
        let symbols = alphabet.as_bytes();
        let first = *symbols.first().ok_or(AppError::Internal)?;
        let radix = symbols.len() as u64;
        let mut value = self.next();
        let mut digits = vec![first; length];
        let mut index = length;
        while index > 0 {
            index -= 1;
            let symbol = *symbols
                .get(usize::try_from(value % radix).map_err(|_| AppError::Internal)?)
                .ok_or(AppError::Internal)?;
            digits[index] = symbol;
            value /= radix;
            if value == 0 {
                break;
            }
        }
        String::from_utf8(digits).map_err(|_| AppError::Internal)
    }
}

#[cfg(any(test, feature = "test-util"))]
impl IdSource for SequentialIdSource {
    fn artifact_id(&self) -> Result<ArtifactId, AppError> {
        self.render(ARTIFACT_ID_ALPHABET, ARTIFACT_ID_LENGTH)
            .map(ArtifactId)
    }

    fn share_token(&self) -> Result<ShareToken, AppError> {
        self.render(SHARE_TOKEN_ALPHABET, SHARE_TOKEN_LENGTH)
            .map(ShareToken)
    }

    fn feedback_id(&self) -> Result<FeedbackId, AppError> {
        self.render(FEEDBACK_ID_ALPHABET, FEEDBACK_ID_LENGTH)
            .map(FeedbackId)
    }

    fn webhook_id(&self) -> Result<WebhookId, AppError> {
        self.render(WEBHOOK_ID_ALPHABET, WEBHOOK_ID_LENGTH)
            .map(WebhookId)
    }
}
