use super::*;

/// Pure inbound application service.  It has no credential and no outbound enqueue capability,
/// which structurally prevents inbound content being echoed back to Discord.
#[derive(Clone, Debug)]
pub struct InboundProcessor {
    state: InboundIntegrationState,
}

impl InboundProcessor {
    #[must_use]
    pub const fn new(state: InboundIntegrationState) -> Self {
        Self { state }
    }

    pub fn set_state(&mut self, state: InboundIntegrationState) {
        self.state = state;
    }

    pub fn process(
        &self,
        authorization: Option<&TwoWayThreadAuthorization>,
        event: InboundEvent,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        if inbox.has_event(&event) {
            return InboundResult::Duplicate;
        }
        let result = self.process_once(authorization, &event, inbox, rest);
        // A partial update that could not be hydrated is retryable, not a terminal inbox receipt.
        // Recording it would turn the retry into `Duplicate` and permanently lose the edit.
        if result != InboundResult::NeedsFetch {
            inbox.record_event(&event, result);
        }
        result
    }

    fn process_once(
        &self,
        authorization: Option<&TwoWayThreadAuthorization>,
        event: &InboundEvent,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        if !self.state.enabled {
            return InboundResult::Ignored(IgnoreReason::Disabled);
        }
        let Some(binding) = authorization else {
            return InboundResult::Ignored(IgnoreReason::Unmapped);
        };
        if event.org != binding.org {
            return InboundResult::Rejected(RejectReason::CrossTenant);
        }
        if event.guild_id != binding.guild_id {
            return InboundResult::Rejected(RejectReason::WrongGuild);
        }
        if event.thread_id != binding.thread_id {
            return InboundResult::Rejected(RejectReason::WrongThread);
        }
        if !binding.enabled {
            return InboundResult::Ignored(IgnoreReason::Disabled);
        }

        match &event.kind {
            InboundEventKind::ThreadDelete => {
                inbox.mark_thread_degraded(ThreadDegradedReason::Deleted);
                InboundResult::Degraded(ThreadDegradedReason::Deleted)
            }
            InboundEventKind::ThreadUpdate { archived, locked } if *archived || *locked => {
                inbox.mark_thread_degraded(ThreadDegradedReason::ArchivedOrLocked);
                InboundResult::Degraded(ThreadDegradedReason::ArchivedOrLocked)
            }
            InboundEventKind::ThreadUpdate { .. } => {
                InboundResult::Ignored(IgnoreReason::Unsupported)
            }
            InboundEventKind::MessageCreate => self.create(binding, event.message.as_ref(), inbox),
            InboundEventKind::MessageUpdate => {
                self.update(binding, event.message.as_ref(), inbox, rest)
            }
            InboundEventKind::MessageDelete => self.delete(event.message.as_ref(), inbox),
        }
    }

    fn create(
        &self,
        binding: &TwoWayThreadAuthorization,
        message: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
    ) -> InboundResult {
        let Some(message) = message else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        if let Some(result) = filter_author(binding, message) {
            return result;
        }
        if !message.supported_text {
            return InboundResult::Ignored(IgnoreReason::Unsupported);
        }
        let Some(body) = valid_body(message.content.as_deref()) else {
            return InboundResult::Ignored(IgnoreReason::EmptyContent);
        };
        if inbox.find_message(&message.id).is_some() {
            return InboundResult::Duplicate;
        }
        let parent_id = message.reply_to_message_id.as_deref().and_then(|id| {
            inbox.find_message(id).map(|feedback| {
                feedback
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| feedback.id.clone())
            })
        });
        inbox.put_feedback(InboundFeedback {
            id: FeedbackId::from(format!("discord:{}", message.id)),
            artifact_id: binding.artifact_id.clone(),
            org: binding.org.clone(),
            parent_id,
            author: FeedbackAuthor::Discord {
                external_author_id: message.author.id.clone(),
                external_author_display: message.author.display.clone(),
            },
            body,
            external_message_id: message.id.clone(),
            provider_version: message.version,
            external_created_at: message.created_at.clone(),
            external_edited_at: message.edited_at.clone(),
            external_deleted_at: None,
        });
        InboundResult::Applied
    }

    fn update(
        &self,
        binding: &TwoWayThreadAuthorization,
        partial: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
        rest: &dyn DiscordRestPort,
    ) -> InboundResult {
        let Some(partial) = partial else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        if let Some(result) = filter_author(binding, partial) {
            return result;
        }
        let message = match partial.content.as_ref() {
            Some(_) => partial.clone(),
            None => match rest.fetch_message(&partial.guild_id, &partial.thread_id, &partial.id) {
                Ok(Some(full)) => full,
                Ok(None) | Err(RestError::NotFound) => {
                    return InboundResult::Ignored(IgnoreReason::UnknownMessage);
                }
                Err(RestError::RateLimited | RestError::Unavailable | RestError::Forbidden) => {
                    return InboundResult::NeedsFetch;
                }
            },
        };
        let Some(existing) = inbox.find_message(&message.id).cloned() else {
            return InboundResult::Ignored(IgnoreReason::UnknownMessage);
        };
        if message.version <= existing.provider_version {
            return InboundResult::Ignored(IgnoreReason::Stale);
        }
        let Some(body) = valid_body(message.content.as_deref()) else {
            return InboundResult::Ignored(IgnoreReason::EmptyContent);
        };
        inbox.put_feedback(InboundFeedback {
            body,
            provider_version: message.version,
            external_edited_at: message.edited_at.clone(),
            ..existing
        });
        InboundResult::Applied
    }

    fn delete(
        &self,
        message: Option<&DiscordMessage>,
        inbox: &mut dyn InboundEventInbox,
    ) -> InboundResult {
        let Some(message) = message else {
            return InboundResult::Rejected(RejectReason::InvalidEvent);
        };
        let Some(existing) = inbox.find_message(&message.id).cloned() else {
            return InboundResult::Ignored(IgnoreReason::UnknownMessage);
        };
        if message.version <= existing.provider_version {
            return InboundResult::Ignored(IgnoreReason::Stale);
        }
        inbox.put_feedback(InboundFeedback {
            body: DELETION_TOMBSTONE.to_owned(),
            provider_version: message.version,
            external_deleted_at: message
                .edited_at
                .clone()
                .or_else(|| Some("provider-delete".into())),
            ..existing
        });
        InboundResult::Applied
    }
}

fn filter_author(
    binding: &TwoWayThreadAuthorization,
    message: &DiscordMessage,
) -> Option<InboundResult> {
    if message.author.is_bot || message.author.id == binding.configured_bot_user_id {
        return Some(InboundResult::Ignored(IgnoreReason::Bot));
    }
    if message.author.webhook_id.is_some() {
        return Some(InboundResult::Ignored(IgnoreReason::Webhook));
    }
    None
}

fn valid_body(value: Option<&str>) -> Option<String> {
    let body = value?.trim();
    if body.is_empty() || body.len() > MAX_INBOUND_BODY_BYTES {
        None
    } else {
        Some(body.to_owned())
    }
}
