//! Frozen canonical encodings and field admission. These bytes are the HMAC inputs; any change
//! here is a protocol change.

use crate::config::is_valid_webhook_id;
use crate::error::AppError;

use super::schema::{AuditContext, AuditEvent};
use super::{DOMAIN, KEY_ID, VERSION};

#[derive(Clone, Copy)]
pub(super) struct ReceiptMacInput<'a> {
    pub(super) correlation_id: &'a str,
    pub(super) durability_intent_id: &'a str,
    pub(super) state: &'a str,
    pub(super) event_id: &'a str,
    pub(super) context: &'a AuditContext,
    pub(super) event: &'a AuditEvent,
    pub(super) key_id: &'a str,
    pub(super) canonical_version: i64,
}

pub(super) fn id(value: &str, required: bool) -> Result<String, AppError> {
    let value = value.trim();
    if (required && value.is_empty())
        || value.len() > 100
        || !value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
    {
        return Err(AppError::Validation(
            "audit fields must be server-controlled identifiers".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn bounded(value: &str, maximum: usize) -> Result<String, AppError> {
    let value = value.trim();
    if value.len() > maximum || value.contains('\0') {
        return Err(AppError::Validation("invalid audit context".to_owned()));
    }
    Ok(value.to_owned())
}

pub(super) fn canonical(
    event_id: &str,
    context: &AuditContext,
    event: &AuditEvent,
    occurred_at: &str,
) -> Result<Vec<u8>, AppError> {
    if !matches!(context.actor_type.as_str(), "api_key" | "viewer" | "system")
        || !matches!(
            context.source.as_str(),
            "mcp" | "browser" | "maintenance" | "reconciliation"
        )
        || !matches!(
            event.result.as_str(),
            "success" | "denied" | "failure" | "recovered"
        )
    {
        return Err(AppError::Validation(
            "invalid server-derived audit context or result".to_owned(),
        ));
    }
    let target_id = event.target_id.trim();
    let target_type = event.target_type.to_ascii_lowercase();
    if !target_id.is_empty() && target_type.contains("share") {
        return Err(AppError::Validation(
            "audit events must not retain share tokens".to_owned(),
        ));
    }
    if !target_id.is_empty() && target_type.contains("webhook") && !is_valid_webhook_id(target_id) {
        return Err(AppError::Validation(
            "audit events must not retain unvalidated webhook identifiers".to_owned(),
        ));
    }
    let revision = event
        .revision
        .map_or_else(String::new, |value| value.to_string());
    let fields = [
        id(event_id, true)?,
        id(KEY_ID, true)?,
        bounded(&context.tenant, 80)?,
        bounded(&context.actor_type, 20)?,
        bounded(&context.actor_id, 160)?,
        bounded(&context.actor_role, 40)?,
        id(&event.operation, true)?,
        id(&event.target_type, true)?,
        id(&event.target_id, false)?,
        id(&event.result, true)?,
        id(&event.classification, false)?,
        bounded(&context.source, 20)?,
        bounded(&context.request_id, 120)?,
        id(&revision, false)?,
        occurred_at.trim().to_owned(),
    ];
    let mut output = vec![VERSION];
    for field in fields {
        let bytes = field.as_bytes();
        output.extend_from_slice(
            &(u32::try_from(bytes.len()).map_err(|_| AppError::Internal)?).to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

/// Frozen receipt snapshot format. Pending rows bind an empty event id; finalization atomically
/// binds the terminal state and assigned event id along with every original reservation field.
pub(super) fn canonical_receipt_bytes(input: ReceiptMacInput<'_>) -> Result<Vec<u8>, AppError> {
    // Reuse the event admission policy so a receipt cannot persist data the terminal ledger
    // would reject. Receipt identifiers have their own syntax (`:` is expected) but remain
    // bounded server-controlled values.
    let _ = canonical("receipt", input.context, input.event, "")?;
    if input.correlation_id.is_empty()
        || input.correlation_id.len() > 240
        || input.correlation_id.contains('\0')
        || input.durability_intent_id.is_empty()
        || input.durability_intent_id.len() > 240
        || input.durability_intent_id.contains('\0')
    {
        return Err(AppError::Validation(
            "invalid audit receipt identifier".to_owned(),
        ));
    }
    if !matches!(input.state, "pending" | "finalized")
        || (input.state == "pending" && !input.event_id.is_empty())
        || (input.state == "finalized" && input.event_id.is_empty())
    {
        return Err(AppError::Validation(
            "invalid audit receipt state".to_owned(),
        ));
    }
    let event_id = id(input.event_id, input.state == "finalized")?;
    let version = u8::try_from(input.canonical_version).map_err(|_| AppError::Internal)?;
    let revision = input
        .event
        .revision
        .map_or_else(String::new, |value| value.to_string());
    let canonical_version = input.canonical_version.to_string();
    let fields = [
        input.correlation_id,
        input.durability_intent_id,
        input.state,
        event_id.as_str(),
        input.context.tenant.as_str(),
        input.context.actor_type.as_str(),
        input.context.actor_id.as_str(),
        input.context.actor_role.as_str(),
        input.context.source.as_str(),
        input.context.request_id.as_str(),
        input.event.operation.as_str(),
        input.event.target_type.as_str(),
        input.event.target_id.as_str(),
        input.event.result.as_str(),
        input.event.classification.as_str(),
        revision.as_str(),
        input.key_id,
        canonical_version.as_str(),
    ];
    let mut output = vec![version];
    for field in fields {
        let bytes = field.as_bytes();
        output.extend_from_slice(
            &(u32::try_from(bytes.len()).map_err(|_| AppError::Internal)?).to_be_bytes(),
        );
        output.extend_from_slice(bytes);
    }
    Ok(output)
}

pub(super) fn receipt_message(input: ReceiptMacInput<'_>) -> Result<Vec<u8>, AppError> {
    let canonical = canonical_receipt_bytes(input)?;
    let mut message = Vec::with_capacity(DOMAIN.len() + b"receipt\0".len() + canonical.len());
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(b"receipt\0");
    message.extend_from_slice(&canonical);
    Ok(message)
}
