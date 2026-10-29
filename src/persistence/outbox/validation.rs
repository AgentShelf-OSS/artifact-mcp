use crate::error::AppError;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    DELIVERY_KIND_DISCUSSION_MESSAGE, DELIVERY_KIND_DISCUSSION_THREAD,
    DELIVERY_KIND_DISCUSSION_TOMBSTONE, DELIVERY_KIND_EVENT, EnqueueDelivery, MAX_BUCKET_KEY_BYTES,
    MAX_DEPENDENCY_ID_BYTES, MAX_ORDERING_KEY_BYTES, MAX_PAYLOAD_BYTES, MAX_SECRET_REF_BYTES,
    MAX_TARGET_KEY_BYTES, internal,
};

pub(super) fn validate(input: &EnqueueDelivery) -> Result<(), AppError> {
    if input.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(AppError::PayloadTooLarge);
    }
    if [
        input.event_id.as_str(),
        input.tenant.as_str(),
        input.event_type.as_str(),
        input.target_key.as_str(),
        input.secret_ref.as_str(),
        input.delivery_kind.as_str(),
        input.ordering_key.as_str(),
    ]
    .iter()
    .any(|v| v.trim().is_empty() || v.contains('\0'))
        || !opaque_target_key(&input.target_key)
        || !opaque_ordering_key(&input.ordering_key)
    {
        return Err(AppError::Validation(
            "outbox identity fields are required".into(),
        ));
    }
    if input
        .depends_on_outbox_id
        .as_deref()
        .is_some_and(|id| !opaque_dependency_id(id))
    {
        return Err(AppError::Validation("invalid outbox dependency".into()));
    }
    match input.delivery_kind.as_str() {
        DELIVERY_KIND_EVENT
            if opaque_secret_ref(&input.secret_ref)
                && input.ordering_key == input.target_key
                && input.depends_on_outbox_id.is_none() => {}
        DELIVERY_KIND_DISCUSSION_THREAD if discussion_delivery_identity(input) => {}
        DELIVERY_KIND_DISCUSSION_MESSAGE | DELIVERY_KIND_DISCUSSION_TOMBSTONE
            if discussion_delivery_identity(input) && input.depends_on_outbox_id.is_some() => {}
        _ => {
            return Err(AppError::Validation(
                "invalid discussion delivery contract".into(),
            ));
        }
    }
    Ok(())
}

fn discussion_delivery_identity(input: &EnqueueDelivery) -> bool {
    !input.target_key.starts_with("discussion:")
        && opaque_target_key(&input.target_key)
        && input
            .secret_ref
            .strip_prefix("discussion:")
            .is_some_and(|connection_id| connection_id == input.target_key)
        && opaque_discussion_ref(&input.secret_ref)
        && input.ordering_key.starts_with("discussion:")
}

/// A root may wait on the exact publication notification selected by a notification-thread
/// connection. Replies and tombstones may wait only on their exact generation's root-create job.
/// These two shapes prevent cross-tenant dependencies, arbitrary chains, and threads racing ahead
/// of the notification message they attach to.
pub(super) fn validate_dependency_contract(
    conn: &Connection,
    input: &EnqueueDelivery,
) -> Result<(), AppError> {
    let Some(dependency_id) = input.depends_on_outbox_id.as_deref() else {
        return Ok(());
    };
    if input.delivery_kind == DELIVERY_KIND_DISCUSSION_THREAD {
        let valid: bool = conn
            .query_row(
                "SELECT EXISTS(\
                   SELECT 1 \
                     FROM provider_delivery_outbox o \
                     JOIN org_discord_discussion_connections c \
                       ON c.notification_webhook_id = o.target_key \
                    WHERE o.id = ?1 \
                      AND o.provider = 'discord' \
                      AND o.delivery_kind = 'event' \
                      AND o.event_type = 'published' \
                      AND o.tenant = ?2 \
                      AND c.id = ?3 \
                      AND c.org = ?2 \
                      AND c.strategy = 'notification_thread'\
                 )",
                params![dependency_id, input.tenant, input.target_key],
                |row| row.get(0),
            )
            .map_err(internal)?;
        return if valid {
            Ok(())
        } else {
            Err(AppError::Validation(
                "invalid discussion delivery dependency".into(),
            ))
        };
    }
    let predecessor: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT tenant, target_key, secret_ref, ordering_key
               FROM provider_delivery_outbox
              WHERE id = ?1
                AND delivery_kind = 'discussion_thread'",
            [dependency_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((tenant, target, secret, ordering)) = predecessor else {
        return Err(AppError::Validation(
            "invalid discussion delivery dependency".into(),
        ));
    };
    if tenant != input.tenant
        || target != input.target_key
        || secret != input.secret_ref
        || ordering != input.ordering_key
    {
        return Err(AppError::Validation(
            "invalid discussion delivery dependency".into(),
        ));
    }
    Ok(())
}
fn opaque_discussion_ref(value: &str) -> bool {
    value.strip_prefix("discussion:").is_some_and(|id| {
        !id.is_empty()
            && id.len() <= MAX_SECRET_REF_BYTES.saturating_sub("discussion:".len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    })
}
/// Rate-limit state is keyed by a non-secret provider reference. Event enqueue remains stricter:
/// it accepts only `webhook:` references, while a PBI-079 discussion delivery uses its immutable
/// `discussion:<connection-id>` reference to retain bucket throttling across attempts.
pub(super) fn opaque_provider_secret_ref(value: &str) -> bool {
    opaque_secret_ref(value) || opaque_discussion_ref(value)
}

pub(super) fn opaque_target_key(value: &str) -> bool {
    value.len() <= MAX_TARGET_KEY_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
fn opaque_secret_ref(value: &str) -> bool {
    let Some(identifier) = value.strip_prefix("webhook:") else {
        return false;
    };
    value.len() <= MAX_SECRET_REF_BYTES
        && !identifier.is_empty()
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
pub(super) fn opaque_bucket_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_BUCKET_KEY_BYTES
        && !value.contains(['\0', '\r', '\n'])
        && !value.to_ascii_lowercase().starts_with("http://")
        && !value.to_ascii_lowercase().starts_with("https://")
        && !value.to_ascii_lowercase().contains("/api/webhooks/")
}
fn opaque_ordering_key(value: &str) -> bool {
    value.len() <= MAX_ORDERING_KEY_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}
fn opaque_dependency_id(value: &str) -> bool {
    value.len() <= MAX_DEPENDENCY_ID_BYTES
        && !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_validation_checks_payload_size_before_identity_fields() {
        let input = EnqueueDelivery {
            event_id: String::new(),
            tenant: String::new(),
            event_type: String::new(),
            target_key: String::new(),
            secret_ref: String::new(),
            payload: vec![0; MAX_PAYLOAD_BYTES + 1],
            payload_sha256: None,
            durability_intent_id: None,
            delivery_kind: String::new(),
            ordering_key: String::new(),
            depends_on_outbox_id: None,
        };

        assert!(matches!(validate(&input), Err(AppError::PayloadTooLarge)));
    }
}
