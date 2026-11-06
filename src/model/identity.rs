//! Identity and tenant value types.

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

/// The closed set of publisher-key capabilities, serialized as the stored lowercase value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Reader,
    Author,
    Collaborator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid publisher role")]
pub enum RoleParseError {
    InvalidRole,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Author => "author",
            Self::Collaborator => "collaborator",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = RoleParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reader" => Ok(Self::Reader),
            "author" => Ok(Self::Author),
            "collaborator" => Ok(Self::Collaborator),
            _ => Err(RoleParseError::InvalidRole),
        }
    }
}

impl TryFrom<&str> for Role {
    type Error = RoleParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherIdentity {
    pub client_id: ClientId,
    pub org: OrgId,
    pub label: String,
    pub role: Role,
    /// `None` identifies the legacy API-key path. OAuth service credentials always carry
    /// `Some`, including an empty set, so a token never inherits unrestricted API-key access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<BTreeSet<String>>,
}

impl PublisherIdentity {
    /// Whether this publisher key is the admin key.
    ///
    /// Admin status is derived only from `org == "admin"`, never stored as a separate flag.
    /// A settable flag would create two sources of truth and could break the publisher
    /// tenant lock, invariant 1.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip_exact_wire_strings() {
        for (role, value) in [
            (Role::Reader, "reader"),
            (Role::Author, "author"),
            (Role::Collaborator, "collaborator"),
        ] {
            assert_eq!(role.as_str(), value);
            assert_eq!(role.to_string(), value);
            assert_eq!(value.parse::<Role>(), Ok(role));
            assert_eq!(Role::try_from(value), Ok(role));
            let json = format!("\"{value}\"");
            assert_eq!(serde_json::to_string(&role).unwrap(), json);
            assert_eq!(serde_json::from_str::<Role>(&json).unwrap(), role);
        }
    }

    #[test]
    fn roles_reject_noncanonical_values() {
        for value in ["Author", "admin", "", " author", "author "] {
            assert_eq!(value.parse::<Role>(), Err(RoleParseError::InvalidRole));
            assert_eq!(Role::try_from(value), Err(RoleParseError::InvalidRole));
            let json = serde_json::to_string(value).unwrap();
            assert!(serde_json::from_str::<Role>(&json).is_err());
        }
    }
}
