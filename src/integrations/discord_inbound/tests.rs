use super::*;

fn binding() -> TwoWayThreadAuthorization {
    TwoWayThreadAuthorization {
        org: OrgId::from("acme"),
        artifact_id: ArtifactId::from("artifact-a"),
        guild_id: "guild-a".into(),
        thread_id: "thread-a".into(),
        enabled: true,
        configured_bot_user_id: "bot-a".into(),
        configured_webhook_id: Some("hook-a".into()),
    }
}
fn message(id: &str, body: Option<&str>, version: i64) -> DiscordMessage {
    DiscordMessage {
        id: id.into(),
        guild_id: "guild-a".into(),
        thread_id: "thread-a".into(),
        author: DiscordAuthor {
            id: "human-a".into(),
            display: "A Person".into(),
            is_bot: false,
            webhook_id: None,
        },
        content: body.map(str::to_owned),
        reply_to_message_id: None,
        version,
        created_at: Some("2026-07-30T00:00:00Z".into()),
        edited_at: None,
        supported_text: true,
    }
}
fn event(id: &str, kind: InboundEventKind, message: Option<DiscordMessage>) -> InboundEvent {
    InboundEvent {
        event_id: id.into(),
        gateway_session_id: "session-a".into(),
        org: OrgId::from("acme"),
        kind,
        message,
        guild_id: "guild-a".into(),
        thread_id: "thread-a".into(),
        payload_fingerprint: format!("hash-{id}"),
    }
}
fn processor() -> InboundProcessor {
    InboundProcessor::new(InboundIntegrationState {
        enabled: true,
        health: GatewayHealth::Ready,
    })
}

#[test]
fn characterization_cross_tenant_precedes_wrong_guild_thread_and_disabled_binding() {
    let mut binding = binding();
    binding.enabled = false;
    let mut incoming = event("priority", InboundEventKind::MessageCreate, None);
    incoming.org = OrgId::from("other");
    incoming.guild_id = "wrong".into();
    incoming.thread_id = "wrong".into();
    let mut inbox = MemoryInbox::default();
    assert_eq!(
        processor().process(
            Some(&binding),
            incoming,
            &mut inbox,
            &FakeDiscordRest::default()
        ),
        InboundResult::Rejected(RejectReason::CrossTenant)
    );
    assert_eq!(
        inbox.results,
        vec![(
            "hash-priority".into(),
            InboundResult::Rejected(RejectReason::CrossTenant)
        )]
    );
    assert!(inbox.feedback.is_empty());
}

#[test]
fn characterization_retryable_fetch_is_not_recorded_but_not_found_is() {
    let rest = FakeDiscordRest::default();
    let mut inbox = MemoryInbox::default();
    let incoming = event(
        "update",
        InboundEventKind::MessageUpdate,
        Some(message("m", None, 2)),
    );
    rest.fail("guild-a", "thread-a", "m", RestError::Unavailable);
    assert_eq!(
        processor().process(Some(&binding()), incoming.clone(), &mut inbox, &rest),
        InboundResult::NeedsFetch
    );
    assert!(inbox.results.is_empty());
    rest.fail("guild-a", "thread-a", "m", RestError::NotFound);
    assert_eq!(
        processor().process(Some(&binding()), incoming.clone(), &mut inbox, &rest),
        InboundResult::Ignored(IgnoreReason::UnknownMessage)
    );
    assert_eq!(
        processor().process(Some(&binding()), incoming, &mut inbox, &rest),
        InboundResult::Duplicate
    );
    assert_eq!(inbox.results.len(), 1);
}

#[test]
fn human_create_is_external_identity_once_and_never_has_an_email_projection() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    let create = event(
        "create-1",
        InboundEventKind::MessageCreate,
        Some(message("message-1", Some("hello"), 1)),
    );
    assert_eq!(
        processor().process(Some(&binding()), create.clone(), &mut inbox, &rest),
        InboundResult::Applied
    );
    assert_eq!(
        processor().process(Some(&binding()), create, &mut inbox, &rest),
        InboundResult::Duplicate
    );
    let feedback = inbox.feedback.get("message-1").unwrap();
    assert_eq!(feedback.author.verified_viewer_email(), None);
    assert_eq!(feedback.parent_id, None);
    assert_eq!(inbox.feedback.len(), 1);
}

#[test]
fn reply_to_a_reply_normalizes_to_the_top_level_parent() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    let p = processor();
    p.process(
        Some(&binding()),
        event(
            "root",
            InboundEventKind::MessageCreate,
            Some(message("root", Some("root"), 1)),
        ),
        &mut inbox,
        &rest,
    );
    let mut reply = message("reply", Some("reply"), 1);
    reply.reply_to_message_id = Some("root".into());
    p.process(
        Some(&binding()),
        event("reply", InboundEventKind::MessageCreate, Some(reply)),
        &mut inbox,
        &rest,
    );
    let mut nested = message("nested", Some("nested"), 1);
    nested.reply_to_message_id = Some("reply".into());
    assert_eq!(
        p.process(
            Some(&binding()),
            event("nested", InboundEventKind::MessageCreate, Some(nested)),
            &mut inbox,
            &rest
        ),
        InboundResult::Applied
    );
    assert_eq!(
        inbox.feedback["nested"].parent_id,
        Some(FeedbackId::from("discord:root"))
    );
}

#[test]
fn filters_automated_unmapped_disabled_and_cross_tenant_events_without_content_retention() {
    let rest = FakeDiscordRest::default();
    let mut inbox = MemoryInbox::default();
    let p = processor();
    let mut bot = message("bot", Some("secret text"), 1);
    bot.author.is_bot = true;
    assert_eq!(
        p.process(
            Some(&binding()),
            event("bot", InboundEventKind::MessageCreate, Some(bot)),
            &mut inbox,
            &rest
        ),
        InboundResult::Ignored(IgnoreReason::Bot)
    );
    assert_eq!(
        p.process(
            None,
            event(
                "none",
                InboundEventKind::MessageCreate,
                Some(message("m", Some("secret text"), 1))
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Ignored(IgnoreReason::Unmapped)
    );
    let mut foreign = event(
        "foreign",
        InboundEventKind::MessageCreate,
        Some(message("f", Some("secret text"), 1)),
    );
    foreign.org = OrgId::from("other");
    assert_eq!(
        p.process(Some(&binding()), foreign, &mut inbox, &rest),
        InboundResult::Rejected(RejectReason::CrossTenant)
    );
    assert!(inbox.feedback.is_empty());
}

#[test]
fn configured_webhook_messages_are_ignored_and_never_loop_back_as_feedback() {
    let rest = FakeDiscordRest::default();
    let mut inbox = MemoryInbox::default();
    let mut webhook = message("webhook", Some("outbound echo"), 1);
    webhook.author.webhook_id = Some("hook-a".into());
    assert_eq!(
        processor().process(
            Some(&binding()),
            event("webhook", InboundEventKind::MessageCreate, Some(webhook)),
            &mut inbox,
            &rest
        ),
        InboundResult::Ignored(IgnoreReason::Webhook)
    );
    assert!(inbox.feedback.is_empty());
}

#[test]
fn crash_between_feedback_and_inbox_commit_recovers_without_losing_or_duplicating_create() {
    let rest = FakeDiscordRest::default();
    let mut inbox = MemoryInbox::default();
    // Simulate a pre-transaction legacy failure: feedback/link committed but the event receipt
    // did not. The real SQL adapter is required to make these one transaction; this proves a
    // replay also converges safely while an operator recovers such a partial state.
    let message = message("m", Some("survives replay"), 1);
    let feedback = InboundFeedback {
        id: FeedbackId::from("discord:m"),
        artifact_id: ArtifactId::from("artifact-a"),
        org: OrgId::from("acme"),
        parent_id: None,
        author: FeedbackAuthor::Discord {
            external_author_id: "human-a".into(),
            external_author_display: "A Person".into(),
        },
        body: "survives replay".into(),
        external_message_id: "m".into(),
        provider_version: 1,
        external_created_at: None,
        external_edited_at: None,
        external_deleted_at: None,
    };
    inbox.put_feedback(feedback);
    assert_eq!(
        processor().process(
            Some(&binding()),
            event(
                "replayed-create",
                InboundEventKind::MessageCreate,
                Some(message)
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Duplicate
    );
    assert_eq!(inbox.feedback.len(), 1);
    assert_eq!(inbox.results.len(), 1);
}

#[test]
fn partial_update_fetches_current_message_and_out_of_order_update_cannot_regress_body() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    let p = processor();
    p.process(
        Some(&binding()),
        event(
            "create",
            InboundEventKind::MessageCreate,
            Some(message("m", Some("old"), 1)),
        ),
        &mut inbox,
        &rest,
    );
    let mut full = message("m", Some("new"), 3);
    full.edited_at = Some("2026-07-30T01:00:00Z".into());
    rest.put(full);
    assert_eq!(
        p.process(
            Some(&binding()),
            event(
                "update-new",
                InboundEventKind::MessageUpdate,
                Some(message("m", None, 3))
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Applied
    );
    assert_eq!(
        p.process(
            Some(&binding()),
            event(
                "update-old",
                InboundEventKind::MessageUpdate,
                Some(message("m", Some("stale"), 2))
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Ignored(IgnoreReason::Stale)
    );
    assert_eq!(inbox.feedback["m"].body, "new");
}

#[test]
fn delete_tombstones_without_retaining_body_and_thread_loss_only_degrades_sync() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    let p = processor();
    p.process(
        Some(&binding()),
        event(
            "create",
            InboundEventKind::MessageCreate,
            Some(message("m", Some("remove me"), 1)),
        ),
        &mut inbox,
        &rest,
    );
    let mut deleted = message("m", None, 2);
    deleted.edited_at = Some("2026-07-30T02:00:00Z".into());
    assert_eq!(
        p.process(
            Some(&binding()),
            event("delete", InboundEventKind::MessageDelete, Some(deleted)),
            &mut inbox,
            &rest
        ),
        InboundResult::Applied
    );
    assert_eq!(inbox.feedback["m"].body, DELETION_TOMBSTONE);
    assert_eq!(
        p.process(
            Some(&binding()),
            event("thread-delete", InboundEventKind::ThreadDelete, None),
            &mut inbox,
            &rest
        ),
        InboundResult::Degraded(ThreadDegradedReason::Deleted)
    );
    assert_eq!(inbox.feedback.len(), 1);
}

#[test]
fn locked_or_archived_thread_degrades_but_preserves_canonical_feedback() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    let p = processor();
    p.process(
        Some(&binding()),
        event(
            "create",
            InboundEventKind::MessageCreate,
            Some(message("m", Some("persist"), 1)),
        ),
        &mut inbox,
        &rest,
    );
    assert_eq!(
        p.process(
            Some(&binding()),
            event(
                "locked",
                InboundEventKind::ThreadUpdate {
                    archived: false,
                    locked: true
                },
                None
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Degraded(ThreadDegradedReason::ArchivedOrLocked)
    );
    assert_eq!(inbox.feedback["m"].body, "persist");
}

#[test]
fn fake_gateway_records_identify_resume_reconnect_invalid_session_and_shutdown_contract() {
    let gateway = FakeDiscordGateway::scripted([
        GatewayFrame::Hello {
            heartbeat_interval_ms: 45_000,
        },
        GatewayFrame::Ready {
            session_id: "session".into(),
            resume_gateway_url: "wss://resume".into(),
        },
        GatewayFrame::Reconnect,
        GatewayFrame::InvalidSession { resumable: false },
        GatewayFrame::Closed,
    ]);
    gateway.send(GatewayCommand::Identify {
        intents: 1 | 1 << 9 | 1 << 15,
    }); // GUILDS, GUILD_MESSAGES, MESSAGE_CONTENT
    gateway.send(GatewayCommand::Heartbeat { sequence: Some(9) });
    gateway.send(GatewayCommand::Resume {
        session_id: "session".into(),
        sequence: 9,
    });
    gateway.send(GatewayCommand::Reconnect);
    gateway.send(GatewayCommand::Identify {
        intents: 1 | 1 << 9 | 1 << 15,
    }); // invalid-session false re-identifies
    gateway.send(GatewayCommand::Shutdown);
    assert!(matches!(
        gateway.next_frame(),
        Some(GatewayFrame::Hello { .. })
    ));
    assert_eq!(gateway.commands().len(), 6);
}

#[test]
fn gateway_lifecycle_caches_sequence_resumes_reconnects_on_missed_ack_and_reidentifies() {
    let mut lifecycle = GatewayLifecycle::new(1 | 1 << 9 | 1 << 15);
    assert_eq!(
        lifecycle.on_frame(&GatewayFrame::Hello {
            heartbeat_interval_ms: 45_000
        }),
        Some(GatewayCommand::Identify {
            intents: 1 | 1 << 9 | 1 << 15
        })
    );
    assert_eq!(
        lifecycle.on_frame(&GatewayFrame::Ready {
            session_id: "session".into(),
            resume_gateway_url: "wss://resume".into()
        }),
        None
    );
    let dispatch = event(
        "dispatch",
        InboundEventKind::MessageCreate,
        Some(message("m", Some("body"), 1)),
    );
    let _ = lifecycle.on_frame(&GatewayFrame::Dispatch {
        sequence: 9,
        event: Box::new(dispatch),
    });
    assert_eq!(
        lifecycle.heartbeat_due(),
        GatewayCommand::Heartbeat { sequence: Some(9) }
    );
    assert_eq!(lifecycle.heartbeat_due(), GatewayCommand::Reconnect);
    assert_eq!(
        lifecycle.on_frame(&GatewayFrame::Hello {
            heartbeat_interval_ms: 45_000
        }),
        Some(GatewayCommand::Resume {
            session_id: "session".into(),
            sequence: 9
        })
    );
    assert_eq!(
        lifecycle.on_frame(&GatewayFrame::InvalidSession { resumable: false }),
        Some(GatewayCommand::Identify {
            intents: 1 | 1 << 9 | 1 << 15
        })
    );
    assert_eq!(lifecycle.state, GatewayResumeState::default());
}

#[test]
fn readiness_distinguishes_intents_and_permissions_from_application_liveness() {
    assert_eq!(
        GatewayLifecycle::readiness(false, true, true, true, true),
        DiscordReadiness::MissingCredential
    );
    assert_eq!(
        GatewayLifecycle::readiness(true, false, true, true, true),
        DiscordReadiness::MissingMessageContentIntent
    );
    assert_eq!(
        GatewayLifecycle::readiness(true, true, true, true, false),
        DiscordReadiness::InvalidOrDisallowedIntents
    );
    assert_eq!(
        GatewayLifecycle::readiness(true, true, true, true, true),
        DiscordReadiness::Ready
    );
}

#[test]
fn rate_limited_partial_update_is_deferred_and_kill_switch_keeps_inbox_empty() {
    let mut inbox = MemoryInbox::default();
    let rest = FakeDiscordRest::default();
    rest.fail("guild-a", "thread-a", "m", RestError::RateLimited);
    let p = processor();
    p.process(
        Some(&binding()),
        event(
            "create",
            InboundEventKind::MessageCreate,
            Some(message("m", Some("persist"), 1)),
        ),
        &mut inbox,
        &rest,
    );
    assert_eq!(
        p.process(
            Some(&binding()),
            event(
                "update",
                InboundEventKind::MessageUpdate,
                Some(message("m", None, 3))
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::NeedsFetch
    );
    let disabled = InboundProcessor::new(InboundIntegrationState::default());
    assert_eq!(
        disabled.process(
            Some(&binding()),
            event(
                "disabled",
                InboundEventKind::MessageCreate,
                Some(message("new", Some("local survives"), 1))
            ),
            &mut inbox,
            &rest
        ),
        InboundResult::Ignored(IgnoreReason::Disabled)
    );
    assert_eq!(inbox.feedback.len(), 1);
}
