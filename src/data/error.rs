//! Stable live-data failures shared by the service and its transports.
use crate::persistence::db::DbPool;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("not_found")]
    NotFound,
    #[error("stale_version")]
    StaleVersion,
    #[error("source_conflict")]
    SourceConflict,
    #[error("source_in_use")]
    SourceInUse,
    #[error("binding_in_use")]
    BindingInUse,
    #[error("org_in_use")]
    OrgInUse,
    #[error("operator_read_only")]
    OperatorReadOnly,
    #[error("data_unavailable")]
    Unavailable,
    #[error("test_unavailable")]
    TestUnavailable,
    #[error("bad_params")]
    BadParams,
    #[error("too_large")]
    TooLarge,
    #[error("invalid_source")]
    InvalidSource,
    #[error("invalid_data_sources")]
    InvalidDataSources,
    #[error("missing_reference")]
    MissingReference,
    #[error("credential_reference_not_allowed")]
    CredentialReferenceNotAllowed,
    #[error("unknown_org")]
    UnknownOrg,
    #[error("config_error")]
    ConfigError,
    #[error("data_unavailable")]
    Internal,
}

impl DataError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::StaleVersion => "stale_version",
            Self::SourceConflict => "source_conflict",
            Self::SourceInUse => "source_in_use",
            Self::BindingInUse => "binding_in_use",
            Self::OrgInUse => "org_in_use",
            Self::OperatorReadOnly => "operator_read_only",
            Self::Unavailable => "data_unavailable",
            Self::TestUnavailable => "test_unavailable",
            Self::BadParams => "bad_params",
            Self::TooLarge => "too_large",
            Self::InvalidSource => "invalid_source",
            Self::InvalidDataSources => "invalid_data_sources",
            Self::MissingReference => "missing_reference",
            Self::CredentialReferenceNotAllowed => "credential_reference_not_allowed",
            Self::UnknownOrg => "unknown_org",
            Self::ConfigError => "config_error",
            Self::Internal => "data_unavailable",
        }
    }
    pub(crate) fn internal(operation: &'static str, error: impl std::fmt::Display) -> Self {
        tracing::error!(operation, %error, "live data operation failed");
        Self::Internal
    }
}

// Keep typed domain failures inside the blocking closure; the persistence adapter handles
// pool and task failures without converting a domain conflict through a reason string.
pub(super) async fn interact<T, F>(pool: &DbPool, work: F) -> Result<T, DataError>
where
    T: Send + 'static,
    F: FnOnce(&mut rusqlite::Connection) -> Result<T, DataError> + Send + 'static,
{
    crate::persistence::db::interact(pool, move |connection| Ok(work(connection)))
        .await
        .map_err(|error| DataError::internal("interact", error))?
}

impl DataError {
    pub(crate) fn stream_reason(&self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::NotFound
            | Self::StaleVersion
            | Self::SourceConflict
            | Self::SourceInUse
            | Self::BindingInUse
            | Self::OrgInUse
            | Self::OperatorReadOnly
            | Self::Unavailable
            | Self::TestUnavailable
            | Self::BadParams
            | Self::InvalidSource
            | Self::InvalidDataSources
            | Self::MissingReference
            | Self::CredentialReferenceNotAllowed
            | Self::UnknownOrg
            | Self::ConfigError
            | Self::Internal => "data_unavailable",
        }
    }
}

impl DataError {
    pub(crate) fn query_reason(&self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::BadParams => "bad_params",
            Self::NotFound => "not_found",
            Self::StaleVersion
            | Self::SourceConflict
            | Self::SourceInUse
            | Self::BindingInUse
            | Self::OrgInUse
            | Self::OperatorReadOnly
            | Self::Unavailable
            | Self::TestUnavailable
            | Self::InvalidSource
            | Self::InvalidDataSources
            | Self::MissingReference
            | Self::CredentialReferenceNotAllowed
            | Self::UnknownOrg
            | Self::ConfigError
            | Self::Internal => "data_unavailable",
        }
    }
}

#[cfg(test)]
pub(crate) fn error_cases() -> [(DataError, &'static str); 18] {
    [
        (DataError::NotFound, "not_found"),
        (DataError::StaleVersion, "stale_version"),
        (DataError::SourceConflict, "source_conflict"),
        (DataError::SourceInUse, "source_in_use"),
        (DataError::BindingInUse, "binding_in_use"),
        (DataError::OrgInUse, "org_in_use"),
        (DataError::OperatorReadOnly, "operator_read_only"),
        (DataError::Unavailable, "data_unavailable"),
        (DataError::TestUnavailable, "test_unavailable"),
        (DataError::BadParams, "bad_params"),
        (DataError::TooLarge, "too_large"),
        (DataError::InvalidSource, "invalid_source"),
        (DataError::InvalidDataSources, "invalid_data_sources"),
        (DataError::MissingReference, "missing_reference"),
        (
            DataError::CredentialReferenceNotAllowed,
            "credential_reference_not_allowed",
        ),
        (DataError::UnknownOrg, "unknown_org"),
        (DataError::ConfigError, "config_error"),
        (DataError::Internal, "data_unavailable"),
    ]
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_reason_tokens_and_display() {
        for (error, reason) in error_cases() {
            assert_eq!(error.reason(), reason);
            assert_eq!(error.to_string(), reason);
            assert_eq!(
                error.stream_reason(),
                if reason == "too_large" {
                    "too_large"
                } else {
                    "data_unavailable"
                }
            );
            assert_eq!(
                error.query_reason(),
                if matches!(reason, "too_large" | "bad_params" | "not_found") {
                    reason
                } else {
                    "data_unavailable"
                }
            );
        }
    }
    #[tokio::test]
    async fn database_adapter_preserves_domain_conflicts() {
        let manager = r2d2_sqlite::SqliteConnectionManager::memory();
        let pool = r2d2::Pool::builder().max_size(1).build(manager).unwrap();
        let result: Result<(), DataError> = interact(&pool, |_| Err(DataError::StaleVersion)).await;
        assert!(matches!(result, Err(DataError::StaleVersion)));
    }
}
