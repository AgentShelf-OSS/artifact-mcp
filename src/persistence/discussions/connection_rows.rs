//! Organization-level discussion connection rows and the existence and authority checks that
//! guard replacing or removing them.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{
    internal,
    types::{DiscussionConnectionStrategy, DiscussionConnectionSummary},
};
use crate::{
    error::AppError,
    model::{OrgId, Timestamp},
    persistence::{migrations::EncryptedUrl, webhooks::mask_url},
    security::crypto::StoredWebhookUrl,
};

pub(super) const CONNECTION_COLUMNS: &str = "id, org, url, url_cipher, url_nonce, url_tag, label, created_at, \
                                  updated_at, last_ok_at, last_error, strategy, \
                                  notification_webhook_id, channel_id, guild_id, \
                                  notification_provider_webhook_id";
pub(super) const CONNECTION_ID_ALPHABET: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

pub(super) fn new_connection_id() -> String {
    format!("discussion-{}", nanoid::nanoid!(21, CONNECTION_ID_ALPHABET))
}

#[derive(Clone)]
pub(super) struct DiscussionConnectionRow {
    pub(super) id: String,
    pub(super) org: OrgId,
    pub(super) url: String,
    pub(super) encrypted: Option<EncryptedUrl>,
    pub(super) label: String,
    pub(super) created_at: Timestamp,
    pub(super) updated_at: Timestamp,
    pub(super) last_ok_at: Option<Timestamp>,
    pub(super) last_error: Option<String>,
    pub(super) strategy: DiscussionConnectionStrategy,
    pub(super) notification_webhook_id: Option<String>,
    pub(super) channel_id: Option<String>,
    pub(super) guild_id: Option<String>,
    pub(super) notification_provider_webhook_id: Option<String>,
}

impl DiscussionConnectionRow {
    pub(super) fn stored_url(&self) -> StoredWebhookUrl {
        StoredWebhookUrl {
            url: self.url.clone(),
            encrypted: self.encrypted.clone(),
        }
    }

    pub(super) fn summary(self) -> DiscussionConnectionSummary {
        let destination = match self.strategy {
            DiscussionConnectionStrategy::ForumWebhook => mask_url(&self.url),
            DiscussionConnectionStrategy::NotificationThread => {
                self.channel_id.as_deref().map_or_else(
                    || "Discord notification channel".to_owned(),
                    |id| format!("Discord channel {id}"),
                )
            }
        };
        DiscussionConnectionSummary {
            id: self.id,
            org: self.org,
            label: self.label,
            destination,
            strategy: self.strategy,
            notification_webhook_id: self.notification_webhook_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            last_ok_at: self.last_ok_at,
            last_error: self.last_error,
        }
    }

    pub(super) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let cipher: Option<String> = row.get("url_cipher")?;
        let nonce: Option<String> = row.get("url_nonce")?;
        let tag: Option<String> = row.get("url_tag")?;
        Ok(Self {
            id: row.get("id")?,
            org: OrgId(row.get("org")?),
            url: row.get("url")?,
            encrypted: cipher.map(|ciphertext| EncryptedUrl {
                ciphertext,
                nonce: nonce.unwrap_or_default(),
                tag: tag.unwrap_or_default(),
            }),
            label: row.get("label")?,
            created_at: Timestamp(row.get("created_at")?),
            updated_at: Timestamp(row.get("updated_at")?),
            last_ok_at: row.get::<_, Option<String>>("last_ok_at")?.map(Timestamp),
            last_error: row.get("last_error")?,
            strategy: DiscussionConnectionStrategy::parse(row.get("strategy")?)?,
            notification_webhook_id: row.get("notification_webhook_id")?,
            channel_id: row.get("channel_id")?,
            guild_id: row.get("guild_id")?,
            notification_provider_webhook_id: row.get("notification_provider_webhook_id")?,
        })
    }
}

pub(super) fn delivery_url_for_connection(
    conn: &Connection,
    row: &DiscussionConnectionRow,
) -> Result<StoredWebhookUrl, AppError> {
    if row.strategy == DiscussionConnectionStrategy::ForumWebhook {
        return Ok(row.stored_url());
    }
    let webhook_id = row
        .notification_webhook_id
        .as_deref()
        .ok_or(AppError::Internal)?;
    conn.query_row(
        "SELECT url, url_cipher, url_nonce, url_tag FROM org_webhooks \
         WHERE id = ?1 AND org = ?2",
        params![webhook_id, row.org.0],
        |source| {
            let cipher: Option<String> = source.get(1)?;
            let nonce: Option<String> = source.get(2)?;
            let tag: Option<String> = source.get(3)?;
            Ok(StoredWebhookUrl {
                url: source.get(0)?,
                encrypted: cipher.map(|ciphertext| EncryptedUrl {
                    ciphertext,
                    nonce: nonce.unwrap_or_default(),
                    tag: tag.unwrap_or_default(),
                }),
            })
        },
    )
    .optional()
    .map_err(internal)?
    .ok_or(AppError::Internal)
}

pub(super) fn connection_row(
    conn: &Connection,
    org: &str,
) -> Result<Option<DiscussionConnectionRow>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {CONNECTION_COLUMNS} FROM org_discord_discussion_connections WHERE org = ?1"
        ),
        [org],
        DiscussionConnectionRow::from_row,
    )
    .optional()
    .map_err(internal)
}

pub(super) fn connection_row_by_id(
    conn: &Connection,
    id: &str,
    org: &str,
) -> Result<Option<DiscussionConnectionRow>, AppError> {
    conn.query_row(
        &format!(
            "SELECT {CONNECTION_COLUMNS} FROM org_discord_discussion_connections WHERE id = ?1 AND org = ?2"
        ),
        params![id, org],
        DiscussionConnectionRow::from_row,
    )
    .optional()
    .map_err(internal)
}

pub(super) fn org_exists(conn: &Connection, org: &str) -> Result<bool, AppError> {
    conn.query_row("SELECT 1 FROM orgs WHERE name = ?1", [org], |_| Ok(()))
        .optional()
        .map(|row: Option<()>| row.is_some())
        .map_err(internal)
}

pub(super) fn notification_webhook_is_eligible(
    conn: &Connection,
    org: &str,
    webhook_id: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM org_webhooks \
         WHERE id = ?1 AND org = ?2 \
           AND instr(',' || events || ',', ',published,') > 0)",
        params![webhook_id, org],
        |row| row.get(0),
    )
    .map_err(internal)
}

pub(super) fn valid_discord_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(super) fn has_bound_connection_authority(
    conn: &Connection,
    org: &str,
) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_discussions WHERE org = ?1 AND connection_id IS NOT NULL) \
         OR EXISTS(SELECT 1 FROM discussion_message_links WHERE org = ?1)",
        [org],
        |row| row.get(0),
    ).map_err(internal)
}
