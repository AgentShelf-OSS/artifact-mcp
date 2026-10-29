//! Owned by U09 (terra) — publisher key persistence.
//!
//! Node oracle: `lib/keys.js` (create/list/revoke) and `seedKeysFromEnv` in `lib/db.js:30-59`
//! (the bootstrap path). Both are ported literally, including error strings, because
//! `/settings/keys` returns a thrown message verbatim as `400 {"error": …}`
//! ([lib/app.js:277-279]).
//!
//! # The secret exists for exactly one response
//!
//! A publisher key's secret is generated, hashed, and shown once; only `sha256Hex(secret)` is
//! ever written to `api_keys.key_hash` ([lib/keys.js:38-39]). Inside this module the raw value
//! never exists as a bare `String`: it is a [`Secret`](crate::config::Secret), whose `Debug`/`Display` are redacted, and
//! it is unwrapped only when the frozen [`CreatedPublisherKey`](crate::model::CreatedPublisherKey) is built for the caller. No error,
//! log line, or database column can therefore carry it.
//!
//! **Contract-delta request:** `CreatedPublisherKey.secret` is a plain `String` on a struct that
//! derives `Debug` (`src/model/admin.rs:23-29`), so `{:?}` on the *frozen model* prints the live
//! secret. `config::Secret` already exists for exactly this purpose. Changing the field type is a
//! U01-owned edit; U09 raises it rather than making it.
//!
//! # Bootstrap seeding
//!
//! `ARTIFACT_API_KEYS` seeding is `INSERT … ON CONFLICT(client_id) DO NOTHING`: it may create a
//! key, never update or un-revoke one, so keys managed in Settings stay authoritative
//! ([lib/db.js:28-29]). Documented placeholder secrets are refused outright ([lib/db.js:36-38]) —
//! copying `.env.example` unchanged must not leave a publicly known key live on `/mcp`.

mod operations;
mod store;
mod validation;

pub use operations::*;
pub use store::KeyStore;
pub use validation::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Secret, SeededRandom};
    use crate::error::AppError;
    use crate::model::{ClientId, CreatePublisherKey, OrgId};
    use rusqlite::Connection;

    #[test]
    fn accepts_only_node_shaped_client_ids() {
        assert!(is_valid_client_id("ab"));
        assert!(is_valid_client_id("A.b_c-9"));
        assert!(is_valid_client_id(&format!("a{}", "b".repeat(40))));
        assert!(!is_valid_client_id(&format!("a{}", "b".repeat(41))));
        assert!(!is_valid_client_id("a"));
        assert!(!is_valid_client_id(""));
        assert!(!is_valid_client_id(".ab"));
        assert!(!is_valid_client_id("a b"));
    }

    #[test]
    fn accepts_single_character_orgs_but_not_single_character_names() {
        assert!(is_valid_key_org("a"));
        assert!(!is_valid_client_id("a"));
    }

    #[test]
    fn generates_a_48_character_lowercase_hex_secret() {
        let secret = generate_secret(&SeededRandom::new(7)).expect("generate");
        assert_eq!(secret.expose().len(), SECRET_BYTES * 2);
        assert!(
            secret
                .expose()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn never_renders_a_secret_through_debug_or_display() {
        let secret = generate_secret(&SeededRandom::new(11)).expect("generate");
        let rendered = format!("{secret:?} {secret}");
        assert!(!rendered.contains(secret.expose()));
        assert_eq!(rendered, "Secret(<redacted>) <redacted>");
    }

    #[test]
    fn hashes_the_secret_exactly_as_node_does() {
        // `crypto.createHash("sha256").update("abc").digest("hex")`
        assert_eq!(
            key_hash(&Secret::new("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn owner_verification_precedes_client_id_validation() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE org_email_members (email TEXT, org TEXT);
             CREATE TABLE org_domains (domain TEXT, org TEXT);",
        )
        .expect("identity schema");
        let request = CreatePublisherKey {
            client_id: ClientId("x".to_owned()),
            org: OrgId("default".to_owned()),
            label: String::new(),
            role: String::new(),
            owner_email: Some("missing@example.com".to_owned()),
        };
        assert_eq!(
            create_key(&conn, &request, &SeededRandom::new(1)),
            Err(AppError::Validation(
                "Owner must be a verified member of this organization.".to_owned()
            ))
        );
    }
}
