use std::fmt::Write as _;

use crate::artifacts::digest::sha256_hex;
use crate::config::{RandomSource, Secret};
use crate::error::AppError;

/// `crypto.randomBytes(24)`. Rendered as 48 lowercase hex characters.
pub const SECRET_BYTES: usize = 24;

/// Maximum label length after trimming, in UTF-16 code units.
pub const KEY_LABEL_MAX_LENGTH: usize = 60;

/// `NAME_RE = /^[a-z0-9][a-z0-9._-]{1,40}$/i`
pub const CLIENT_ID_MIN_LENGTH: usize = 2;

/// See [`CLIENT_ID_MIN_LENGTH`].
pub const CLIENT_ID_MAX_LENGTH: usize = 41;

/// `ORG_RE = /^[a-z0-9][a-z0-9._-]{0,40}$/i`
pub const KEY_ORG_MAX_LENGTH: usize = 41;

/// Org assigned to a two-part `clientId:secret` seed entry
pub const DEFAULT_SEED_ORG: &str = "default";

/// Secrets that are never seeded, because they are printed in `.env.example` and the README.
pub const PLACEHOLDER_SECRETS: [&str; 2] = ["CHANGE_ME", "REPLACE_WITH_LONG_RANDOM_SECRET"];

/// `Name must be 2–41 characters: …`. The dash is U+2013 (EN DASH).
pub const INVALID_CLIENT_ID_MESSAGE: &str =
    "Name must be 2–41 characters: letters, numbers, dot, dash, underscore.";

/// `Org must be …`
pub const INVALID_ORG_MESSAGE: &str = "Org must be letters, numbers, dot, dash, or underscore.";
pub const INVALID_KEY_ROLE_MESSAGE: &str = "Role must be reader, author, or collaborator.";

/// `/^[a-z0-9][a-z0-9._-]{1,40}$/i`
#[must_use]
pub fn is_valid_client_id(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    let rest = characters.clone().count();
    first.is_ascii_alphanumeric()
        && (1..=CLIENT_ID_MAX_LENGTH - 1).contains(&rest)
        && characters.all(is_name_character)
}

/// `/^[a-z0-9][a-z0-9._-]{0,40}$/i`
///
/// Identical to `orgs::is_valid_org_name`; both files carry the same regex and both are ported
/// against their own source line.
#[must_use]
pub fn is_valid_key_org(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && characters.clone().count() < KEY_ORG_MAX_LENGTH
        && characters.all(is_name_character)
}

fn is_name_character(value: char) -> bool {
    value.is_ascii_alphanumeric() || matches!(value, '.' | '_' | '-')
}

/// `A key named "<id>" already exists.`
#[must_use]
pub fn duplicate_key_message(client_id: &str) -> String {
    format!("A key named \"{client_id}\" already exists.")
}

/// `sha256Hex(secret)`, the only representation of a secret that is persisted.
///
/// Reuses the shared SHA-256 implementation so the project has exactly
/// one implementation of that primitive.
#[must_use]
pub fn key_hash(secret: &Secret) -> String {
    sha256_hex(secret.expose().as_bytes())
}

/// `crypto.randomBytes(24).toString("hex")`
///
/// # Errors
/// [`AppError::Internal`] when the entropy source fails.
pub fn generate_secret(random: &dyn RandomSource) -> Result<Secret, AppError> {
    let mut bytes = [0_u8; SECRET_BYTES];
    random.fill_bytes(&mut bytes)?;
    let mut hex = String::with_capacity(SECRET_BYTES * 2);
    for byte in bytes {
        // `Buffer.toString("hex")` is lowercase and zero padded.
        write!(hex, "{byte:02x}")
            .map_err(|error| crate::error::internal("generate secret", error))?;
    }
    Ok(Secret::new(hex))
}

/// Reports a SQLite failure without leaking the statement's parameters.
pub(super) fn database_failure(operation: &str, error: &rusqlite::Error) -> AppError {
    tracing::error!(operation, error = %error, "publisher key persistence failed");
    AppError::Internal
}
