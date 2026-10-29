use super::*;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// The wire/column name of an event, matching `EVENTS` in `lib/webhooks.js:9`.
#[must_use]
pub const fn event_name(event: &WebhookEvent) -> &'static str {
    match *event {
        WebhookEvent::Published => "published",
        WebhookEvent::Updated => "updated",
        WebhookEvent::Restored => "restored",
        WebhookEvent::Deleted => "deleted",
        WebhookEvent::Feedback => "feedback",
        WebhookEvent::Resolved => "resolved",
    }
}

/// Inverse of [`event_name`]; unknown names are dropped, exactly as `rowEvents` filters them.
/// [lib/webhooks.js:35-37]
#[must_use]
pub fn event_from_name(value: &str) -> Option<WebhookEvent> {
    match value {
        "published" => Some(WebhookEvent::Published),
        "updated" => Some(WebhookEvent::Updated),
        "restored" => Some(WebhookEvent::Restored),
        "deleted" => Some(WebhookEvent::Deleted),
        "feedback" => Some(WebhookEvent::Feedback),
        "resolved" => Some(WebhookEvent::Resolved),
        _ => None,
    }
}

/// Port of `rowEvents(row)` — split on `,` and drop anything not in `EVENTS`.
/// [lib/webhooks.js:35-37]
#[must_use]
pub fn parse_stored_events(column: &str) -> Vec<WebhookEvent> {
    column.split(',').filter_map(event_from_name).collect()
}

/// Port of `parseEvents(value, { defaultAll })` — [lib/webhooks.js:25-33].
///
/// The typed model removes two of Node's four outcomes by construction: a non-array and an unknown
/// event name cannot be represented by `Option<&[WebhookEvent]>`, so the
/// `"Webhook events must be an array."` and `"Unknown webhook event: …"` throws are unreachable
/// here. What remains — the `defaultAll` behaviour and order-preserving de-duplication — is
/// reproduced exactly.
#[must_use]
pub fn normalize_events(events: Option<&[WebhookEvent]>, default_all: bool) -> Vec<WebhookEvent> {
    let Some(requested) = events else {
        return if default_all {
            EVENTS.to_vec()
        } else {
            Vec::new()
        };
    };
    let mut unique: Vec<WebhookEvent> = Vec::with_capacity(requested.len());
    for event in requested {
        if !unique.contains(event) {
            unique.push(event.clone());
        }
    }
    unique
}

/// The `events` column value for a normalized set.
#[must_use]
pub fn encode_events(events: &[WebhookEvent]) -> String {
    events
        .iter()
        .map(event_name)
        .collect::<Vec<&str>>()
        .join(",")
}
