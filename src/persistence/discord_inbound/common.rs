use crate::error::AppError;

pub(super) const MAX_PROVIDER_ID_BYTES: usize = 128;
pub(super) const MAX_SESSION_ID_BYTES: usize = 256;
pub(super) const MAX_SAFE_ERROR_BYTES: usize = 64;
pub(super) const MAX_FETCH_ATTEMPTS: i64 = 20;

pub(super) fn internal(error: impl std::fmt::Display) -> AppError {
    tracing::error!(error = %error, "discord inbound persistence failed");
    AppError::Internal
}
