//! Owned by U01 (sol) — identity and tenant value types.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

string_id!(ArtifactId);
string_id!(ClientId);
string_id!(EmailAddress);
string_id!(FeedbackId);
string_id!(OrgId);
string_id!(ShareToken);
string_id!(WebhookId);

/// SQLite timestamps remain opaque strings so parity does not normalize their formatting.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherIdentity {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: String,
    /// `None` identifies the legacy API-key path. OAuth service credentials always carry
    /// `Some`, including an empty set, so a token never inherits unrestricted API-key access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<BTreeSet<String>>,
}

impl PublisherIdentity {
    /// Whether this publisher key is the admin key.
    ///
    /// DERIVED, never stored. Node has no admin column on `api_keys`; `auth.org === "admin"` is the
    /// entire rule (`lib/access.js:35`, `lib/auth.js:25`). This was a settable `is_admin: bool`
    /// field until U05 and U06 — the two security-critical units — independently reported that a
    /// settable flag creates two sources of truth for an admin decision, and that a future unit
    /// populating it from a wider rule would silently break the publisher tenant-lock (invariant 1).
    /// U06's policy already ignored the field and pinned that with a test setting it wrongly on
    /// purpose. Making it a method removes the possibility entirely.
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.org.0 == "admin"
    }

    /// Whether this identity came from a scoped OAuth access token.
    #[must_use]
    pub const fn is_oauth(&self) -> bool {
        self.scopes.is_some()
    }

    /// API keys retain their role-based compatibility contract; OAuth tokens require an exact
    /// granted scope for each protected operation.
    #[must_use]
    pub fn has_scope(&self, required: &str) -> bool {
        self.scopes
            .as_ref()
            .is_none_or(|scopes| scopes.contains(required))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewer {
    pub email: Option<EmailAddress>,
    pub org: Option<OrgId>,
    pub is_admin: bool,
}
