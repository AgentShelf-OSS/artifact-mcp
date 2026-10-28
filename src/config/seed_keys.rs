//! Seeded publisher keys from `ARTIFACT_API_KEYS`.

use crate::model::{ClientId, OrgId};

use super::{
    defaults::{DEFAULT_SEED_KEY_ORG, PLACEHOLDER_KEY_SECRETS},
    env::Secret,
};

/// One parsed `ARTIFACT_API_KEYS` entry. [lib/db.js:30-58]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedKey {
    /// `clientId` component.
    pub client_id: ClientId,
    /// `org` component, defaulting to `default` for two-part entries.
    pub org: OrgId,
    /// The shared secret; hashed by U04/U09 before it reaches the database.
    pub secret: Secret,
}

/// The result of parsing `ARTIFACT_API_KEYS`, including entries Node refuses to seed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SeedKeys {
    /// Entries eligible for bootstrap insertion, in declaration order.
    pub entries: Vec<SeedKey>,
    /// Client ids skipped because they used a documented placeholder secret.
    /// [lib/db.js:38,51-54]
    pub ignored_placeholders: Vec<ClientId>,
}

impl SeedKeys {
    /// Port of the `ARTIFACT_API_KEYS` parsing half of `seedKeysFromEnv`. [lib/db.js:30-58]
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let mut parsed = Self::default();
        if raw.trim().is_empty() {
            return parsed;
        }
        for entry in raw.split(',') {
            let parts: Vec<&str> = entry.split(':').map(str::trim).collect();
            let (client_id, org, secret) = if parts.len() >= 3 {
                (parts[0], parts[1], parts[2..].join(":"))
            } else if parts.len() == 2 {
                (parts[0], DEFAULT_SEED_KEY_ORG, parts[1].to_owned())
            } else {
                continue;
            };
            if client_id.is_empty() || secret.is_empty() {
                continue;
            }
            if PLACEHOLDER_KEY_SECRETS.contains(&secret.as_str()) {
                parsed.ignored_placeholders.push(ClientId::from(client_id));
                continue;
            }
            let org = if org.is_empty() {
                DEFAULT_SEED_KEY_ORG
            } else {
                org
            };
            parsed.entries.push(SeedKey {
                client_id: ClientId::from(client_id),
                org: OrgId::from(org),
                secret: Secret::new(secret),
            });
        }
        parsed
    }
}
