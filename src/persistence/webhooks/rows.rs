use super::*;

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One `org_webhooks` row.
///
/// `url` is whatever the column holds — the masked display value for an encrypted row, the real
/// URL for a plaintext one — so this type is treated as secret-bearing throughout.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookRow {
    /// Row identifier.
    pub id: WebhookId,
    /// Owning organization.
    pub org: OrgId,
    url: String,
    encrypted: Option<EncryptedUrl>,
    /// Operator-supplied label, already truncated at write time.
    pub label: String,
    /// Subscribed events, unknown names dropped.
    pub events: Vec<WebhookEvent>,
    /// `created_at` column.
    pub created_at: Timestamp,
    /// Last successful delivery.
    pub last_ok_at: Option<Timestamp>,
    /// Last recorded delivery failure.
    pub last_error: Option<String>,
}

impl WebhookRow {
    /// The three at-rest URL columns as U04's value type.
    #[must_use]
    pub fn stored_url(&self) -> StoredWebhookUrl {
        StoredWebhookUrl {
            url: self.url.clone(),
            encrypted: self.encrypted.clone(),
        }
    }

    /// `true` when this row's URL is encrypted at rest.
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.encrypted.is_some()
    }

    /// Port of `publicRow(row)` — the only webhook shape allowed into an HTTP response.
    /// [lib/webhooks.js:59-69]
    #[must_use]
    pub fn public(&self) -> WebhookSummary {
        WebhookSummary {
            id: self.id.clone(),
            label: self.label.clone(),
            events: self.events.clone(),
            url: mask_url(&self.url),
            last_ok_at: self.last_ok_at.clone(),
            last_error: self.last_error.clone(),
        }
    }

    /// Port of `deliveryRow(row)` — [lib/webhooks.js:121-124].
    ///
    /// A row with no ciphertext is returned as-is (the live plaintext configuration). A row *with*
    /// ciphertext requires the key: with no key configured this fails closed rather than handing
    /// back the masked column as if it were a URL.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when the row is encrypted and the key is missing or the
    /// record does not authenticate. The error carries no row content.
    pub fn delivery(&self, protection: &WebhookUrlProtection) -> Result<WebhookDelivery, AppError> {
        Ok(WebhookDelivery {
            id: self.id.clone(),
            org: self.org.clone(),
            url: protection.reveal(&self.stored_url())?,
            label: self.label.clone(),
            events: self.events.clone(),
        })
    }

    pub(super) fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let cipher: Option<String> = row.get("url_cipher")?;
        let nonce: Option<String> = row.get("url_nonce")?;
        let tag: Option<String> = row.get("url_tag")?;
        // Node keys the encrypted branch on `url_cipher` alone (`if (!row?.url_cipher) return row`),
        // so a row missing a sibling column is still treated as encrypted and fails to decrypt
        // rather than silently falling back to the masked column.
        let encrypted = cipher.map(|ciphertext| EncryptedUrl {
            ciphertext,
            nonce: nonce.unwrap_or_default(),
            tag: tag.unwrap_or_default(),
        });
        Ok(Self {
            id: WebhookId(row.get("id")?),
            org: OrgId(row.get("org")?),
            url: row.get("url")?,
            encrypted,
            label: row.get("label")?,
            events: parse_stored_events(&row.get::<_, String>("events")?),
            created_at: Timestamp(row.get("created_at")?),
            last_ok_at: row.get::<_, Option<String>>("last_ok_at")?.map(Timestamp),
            last_error: row.get("last_error")?,
        })
    }
}

/// Redacted by construction: the `url` column can hold a live secret in the plaintext
/// configuration, so it never reaches a formatter.
impl std::fmt::Debug for WebhookRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebhookRow")
            .field("id", &self.id)
            .field("org", &self.org)
            .field("url", &"<redacted>")
            .field("encrypted", &self.encrypted.is_some())
            .field("label", &self.label)
            .field("events", &self.events)
            .field("created_at", &self.created_at)
            .field("last_ok_at", &self.last_ok_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}
