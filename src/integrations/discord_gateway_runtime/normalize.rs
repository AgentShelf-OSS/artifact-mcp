//! Safe normalization of Twilight gateway events and messages into inbound events, and the
//! payload fingerprint that deduplicates them.

use sha2::{Digest as _, Sha256};
use twilight_gateway::Event;
use twilight_model::channel::{Message, message::MessageType};

use crate::{
    integrations::discord_inbound::{
        DiscordAuthor, DiscordMessage, InboundEvent, InboundEventKind,
    },
    model::OrgId,
};

pub(super) fn normalize_event(
    org: &OrgId,
    session_id: String,
    sequence: u64,
    event: Event,
) -> Option<InboundEvent> {
    let event_id = sequence.to_string();

    let (kind, message, guild_id, thread_id) = match event {
        Event::MessageCreate(message) => {
            let guild_id = message.guild_id?.to_string();
            let thread_id = message.channel_id.to_string();
            let normalized = normalize_message(&message, message_version(&message));
            (
                InboundEventKind::MessageCreate,
                Some(normalized),
                guild_id,
                thread_id,
            )
        }
        Event::MessageUpdate(message) => {
            let guild_id = message.guild_id?.to_string();
            let thread_id = message.channel_id.to_string();
            let mut normalized = normalize_message(&message, message_version(&message));
            // Discord's update frame is not the canonical current message. Persist only the
            // identifiers/fingerprint, then hydrate with the bounded REST reader outside SQLite.
            normalized.content = None;
            (
                InboundEventKind::MessageUpdate,
                Some(normalized),
                guild_id,
                thread_id,
            )
        }
        Event::MessageDelete(message) => {
            let guild_id = message.guild_id?.to_string();
            let thread_id = message.channel_id.to_string();
            let normalized = DiscordMessage {
                id: message.id.to_string(),
                guild_id: guild_id.clone(),
                thread_id: thread_id.clone(),
                author: DiscordAuthor {
                    id: "deleted-author".to_owned(),
                    display: "Deleted Discord user".to_owned(),
                    is_bot: false,
                    webhook_id: None,
                },
                content: None,
                reply_to_message_id: None,
                version: current_provider_version(),
                created_at: None,
                edited_at: Some(now_timestamp()),
                supported_text: true,
            };
            (
                InboundEventKind::MessageDelete,
                Some(normalized),
                guild_id,
                thread_id,
            )
        }
        Event::ThreadUpdate(thread) => {
            let guild_id = thread.guild_id?.to_string();
            let thread_id = thread.id.to_string();
            let metadata = thread.thread_metadata.as_ref()?;
            (
                InboundEventKind::ThreadUpdate {
                    archived: metadata.archived,
                    locked: metadata.locked,
                },
                None,
                guild_id,
                thread_id,
            )
        }
        Event::ThreadDelete(thread) => (
            InboundEventKind::ThreadDelete,
            None,
            thread.guild_id.to_string(),
            thread.id.to_string(),
        ),
        _ => return None,
    };
    let fingerprint = event_fingerprint(&kind, message.as_ref(), &guild_id, &thread_id);
    Some(InboundEvent {
        event_id,
        gateway_session_id: session_id,
        org: org.clone(),
        kind,
        message,
        guild_id,
        thread_id,
        payload_fingerprint: fingerprint,
    })
}

pub(super) fn normalize_message(message: &Message, version: i64) -> DiscordMessage {
    DiscordMessage {
        id: message.id.to_string(),
        guild_id: message
            .guild_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        thread_id: message.channel_id.to_string(),
        author: DiscordAuthor {
            id: message.author.id.to_string(),
            display: message
                .author
                .global_name
                .clone()
                .unwrap_or_else(|| message.author.name.clone()),
            is_bot: message.author.bot,
            webhook_id: message.webhook_id.map(|id| id.to_string()),
        },
        content: Some(message.content.clone()),
        reply_to_message_id: message
            .reference
            .as_ref()
            .and_then(|reference| reference.message_id)
            .map(|id| id.to_string()),
        version,
        created_at: Some(message.timestamp.iso_8601().to_string()),
        edited_at: message
            .edited_timestamp
            .map(|timestamp| timestamp.iso_8601().to_string()),
        supported_text: matches!(message.kind, MessageType::Regular | MessageType::Reply),
    }
}

pub(super) fn message_version(message: &Message) -> i64 {
    message
        .edited_timestamp
        .unwrap_or(message.timestamp)
        .as_micros()
}

fn current_provider_version() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_micros()).ok())
        .unwrap_or(i64::MAX)
}

fn now_timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "provider-delete".to_owned())
}

fn event_fingerprint(
    kind: &InboundEventKind,
    message: Option<&DiscordMessage>,
    guild_id: &str,
    thread_id: &str,
) -> String {
    let mut digest = Sha256::new();
    digest.update(match kind {
        InboundEventKind::MessageCreate => b"message_create".as_slice(),
        InboundEventKind::MessageUpdate => b"message_update".as_slice(),
        InboundEventKind::MessageDelete => b"message_delete".as_slice(),
        InboundEventKind::ThreadUpdate { .. } => b"thread_update".as_slice(),
        InboundEventKind::ThreadDelete => b"thread_delete".as_slice(),
    });
    digest.update([0]);
    digest.update(guild_id.as_bytes());
    digest.update([0]);
    digest.update(thread_id.as_bytes());
    if let Some(message) = message {
        digest.update([0]);
        digest.update(message.id.as_bytes());
        digest.update(message.version.to_be_bytes());
        if let Some(content) = message.content.as_deref() {
            digest.update(Sha256::digest(content.as_bytes()));
        }
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::event_fingerprint;
    use crate::integrations::discord_inbound::{DiscordAuthor, DiscordMessage, InboundEventKind};

    fn message(id: &str, version: i64, content: Option<&str>) -> DiscordMessage {
        DiscordMessage {
            id: id.to_owned(),
            guild_id: String::new(),
            thread_id: String::new(),
            author: DiscordAuthor {
                id: String::new(),
                display: String::new(),
                is_bot: false,
                webhook_id: None,
            },
            content: content.map(str::to_owned),
            reply_to_message_id: None,
            version,
            created_at: None,
            edited_at: None,
            supported_text: true,
        }
    }

    /// The fingerprint is stored as `payload_fingerprint` and deduplicates gateway events across
    /// releases, so its bytes are pinned. The expected values were computed independently
    /// (Python `hashlib`) from the same field order.
    #[test]
    fn event_fingerprint_matches_the_pinned_digests() {
        let (guild, thread) = ("223456789012345678", "323456789012345678");
        assert_eq!(
            event_fingerprint(
                &InboundEventKind::MessageCreate,
                Some(&message(
                    "423456789012345678",
                    1_767_225_600_000_000,
                    Some("hello")
                )),
                guild,
                thread,
            ),
            "13244d9a86ae491cad99c314cfdb45fd9dbf32622728709e9bf2f69fb732d448"
        );
        assert_eq!(
            event_fingerprint(
                &InboundEventKind::MessageUpdate,
                Some(&message("423456789012345678", 1_767_225_600_000_001, None)),
                guild,
                thread,
            ),
            "2f6d6fbe424dfec01f2b2e77dc7f20644537e86b1b708f763ad89a63bc95081c"
        );
        assert_eq!(
            event_fingerprint(&InboundEventKind::ThreadDelete, None, guild, thread),
            "437e5a797c83a49c590b955a7ba4274d08f9b953a19291197c7c7f8a21c094b0"
        );
        // Empty content still hashes the content digest, unlike absent content.
        assert_eq!(
            event_fingerprint(
                &InboundEventKind::MessageDelete,
                Some(&message("m", -1, Some(""))),
                "g",
                "t",
            ),
            "28291d93ce0503b50ebb4bc1ddafe83d858156c06156451295f4f536cbe84f3e"
        );
    }
}
