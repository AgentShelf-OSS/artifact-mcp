use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use sha2::Digest;

use sha2::Sha256;

use super::{backoff::backoff, *};
use crate::{
    integrations::discord_discussion::DiscussionReceipt,
    model::{ArtifactId, FeedbackId, NotificationPayload, OrgId, Timestamp, WebhookEvent},
    persistence::discussions::{DiscussionMode, DiscussionState},
};
use crate::{
    integrations::{
        delivery_envelope::{DeliveryEnvelopeV1, DeliveryPreviewReferenceV1},
        discord_delivery::{
            DeliveryClassification, DuplicateRisk, ProviderRequest, RateLimitMetadata,
            RateLimitScope, RetryReason, TerminalReason,
        },
        discord_discussion::{
            DiscussionOperation, DiscussionRequest, DiscussionResult, discussion_request,
        },
        discussion_envelope::{DiscordDiscussionEnvelopeV1, DiscordDiscussionOperationV1},
    },
    model::{WebhookDelivery, WebhookId},
    persistence::{
        discussions::{
            AcceptedDiscussionDelivery, AcceptedDiscussionMarker, ArtifactDiscussion,
            DiscussionConnectionDelivery, DiscussionConnectionStrategy, DiscussionMessageLink,
            TerminalDiscussionDelivery, discussion_ordering_key,
        },
        outbox::{DeadLetterTransition, MAX_ATTEMPTS, RetryTransition},
    },
    ports::{BoxFuture, discussions::OrganizationDiscordCredentialService},
};

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Accepted(String),
    Retry(RetryTransition),
    Dead(String),
    Rate {
        scope: String,
        bucket: String,
        until: i64,
    },
    /// A discussion-store transition, journaled only when a test opts into the shared ledger.
    Discussion(&'static str),
}
struct FakeOutbox {
    record: Mutex<Option<DeliveryRecord>>,
    actions: Mutex<Vec<Action>>,
    dead_risks: Mutex<Vec<bool>>,
    guarded: bool,
    on_claim: Mutex<Option<watch::Sender<bool>>>,
}
impl FakeOutbox {
    fn one(record: DeliveryRecord) -> Self {
        Self {
            record: Mutex::new(Some(record)),
            actions: Mutex::new(Vec::new()),
            dead_risks: Mutex::new(Vec::new()),
            guarded: true,
            on_claim: Mutex::new(None),
        }
    }
    fn actions(&self) -> Vec<Action> {
        std::mem::take(&mut *self.actions.lock().expect("lock"))
    }
    fn dead_risks(&self) -> Vec<bool> {
        std::mem::take(&mut *self.dead_risks.lock().expect("lock"))
    }
}
impl WorkerOutbox for FakeOutbox {
    fn claim<'a>(&'a self, _: String) -> BoxFuture<'a, Result<Option<DeliveryRecord>, AppError>> {
        Box::pin(async move {
            let row = self.record.lock().expect("lock").take();
            if let Some(sender) = self.on_claim.lock().expect("lock").take() {
                sender.send(true).expect("receiver live");
            }
            Ok(row)
        })
    }
    fn accepted<'a>(
        &'a self,
        _: &'a DeliveryRecord,
        _: String,
        message: String,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.actions
                .lock()
                .expect("lock")
                .push(Action::Accepted(message));
            Ok(self.guarded)
        })
    }
    fn retry<'a>(
        &'a self,
        _: &'a DeliveryRecord,
        _: String,
        update: RetryTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.actions
                .lock()
                .expect("lock")
                .push(Action::Retry(update));
            Ok(self.guarded)
        })
    }
    fn dead_letter<'a>(
        &'a self,
        _: &'a DeliveryRecord,
        _: String,
        update: DeadLetterTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.actions
                .lock()
                .expect("lock")
                .push(Action::Dead(update.classification));
            self.dead_risks
                .lock()
                .expect("lock")
                .push(update.duplicate_risk);
            Ok(self.guarded)
        })
    }
    fn persist_rate_limit<'a>(
        &'a self,
        scope: String,
        _: String,
        bucket: String,
        _: String,
        until: i64,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.actions.lock().expect("lock").push(Action::Rate {
                scope,
                bucket,
                until,
            });
            Ok(())
        })
    }
}
struct FakeWebhooks {
    response: Mutex<Result<WebhookDelivery, WebhookResolutionFailure>>,
    results: Mutex<Vec<Result<(), String>>>,
}
impl WorkerWebhooks for FakeWebhooks {
    fn delivery<'a>(
        &'a self,
        _: &'a WebhookId,
        _: &'a OrgId,
    ) -> BoxFuture<'a, Result<WebhookDelivery, WebhookResolutionFailure>> {
        Box::pin(async move { self.response.lock().expect("lock").clone() })
    }
    fn record_result<'a>(
        &'a self,
        _: &'a WebhookId,
        outcome: Result<(), &'static str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            self.results
                .lock()
                .expect("lock")
                .push(outcome.map_err(str::to_owned));
            Ok(())
        })
    }
}
struct FakeProvider {
    result: Mutex<DeliveryClassification>,
    calls: AtomicUsize,
}
struct FakePreview {
    outcome: Result<Option<Vec<u8>>, AppError>,
    calls: AtomicUsize,
}
impl WorkerPreviewResolver for FakePreview {
    fn preview<'a>(
        &'a self,
        _: &'a OrgId,
        _: &'a DeliveryPreviewReferenceV1,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        })
    }
}
impl WorkerProvider for FakeProvider {
    fn deliver<'a>(&'a self, _: ProviderRequest) -> BoxFuture<'a, DeliveryClassification> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.lock().expect("lock").clone()
        })
    }
}
struct FakeDiscussionProvider {
    result: Mutex<DiscussionResult>,
    calls: AtomicUsize,
    requests: Mutex<Vec<DiscussionRequest>>,
}
impl WorkerDiscussionProvider for FakeDiscussionProvider {
    fn deliver<'a>(
        &'a self,
        _: &'a OrgId,
        request: DiscussionRequest,
    ) -> BoxFuture<'a, DiscussionResult> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().expect("lock").push(request);
            self.result.lock().expect("lock").clone()
        })
    }
}
struct FakeDiscussions {
    connection: Mutex<Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>,
    discussion: Mutex<Option<ArtifactDiscussion>>,
    link: Mutex<Option<DiscussionMessageLink>>,
    posts: Mutex<Vec<AcceptedDiscussionDelivery>>,
    tombstones: Mutex<Vec<AcceptedDiscussionDelivery>>,
    markers: Mutex<Vec<AcceptedDiscussionMarker>>,
    terminals: Mutex<Vec<TerminalDiscussionDelivery>>,
    anchor: Mutex<Option<String>>,
    anchor_calls: Mutex<Vec<String>>,
    accept_guard: Mutex<bool>,
    ledger: Mutex<Option<Arc<FakeOutbox>>>,
}
impl FakeDiscussions {
    fn journal(&self, transition: &'static str) {
        if let Some(outbox) = self.ledger.lock().expect("lock").as_ref() {
            outbox
                .actions
                .lock()
                .expect("lock")
                .push(Action::Discussion(transition));
        }
    }
}
impl WorkerDiscussions for FakeDiscussions {
    fn connection<'a>(
        &'a self,
        _: &'a str,
        _: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<DiscussionConnectionDelivery>, DiscussionResolutionFailure>>
    {
        Box::pin(async move { self.connection.lock().expect("lock").clone() })
    }
    fn discussion<'a>(
        &'a self,
        _: &'a ArtifactId,
        _: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<ArtifactDiscussion>, AppError>> {
        Box::pin(async move { Ok(self.discussion.lock().expect("lock").clone()) })
    }
    fn feedback_link<'a>(
        &'a self,
        _: &'a ArtifactId,
        _: &'a OrgId,
        _: &'a FeedbackId,
    ) -> BoxFuture<'a, Result<Option<DiscussionMessageLink>, AppError>> {
        Box::pin(async move { Ok(self.link.lock().expect("lock").clone()) })
    }
    fn notification_anchor<'a>(
        &'a self,
        outbox_id: &'a str,
        _: &'a ArtifactId,
        _: &'a OrgId,
        _: &'a str,
        _: u64,
    ) -> BoxFuture<'a, Result<Option<String>, AppError>> {
        Box::pin(async move {
            self.anchor_calls
                .lock()
                .expect("lock")
                .push(outbox_id.to_owned());
            Ok(self.anchor.lock().expect("lock").clone())
        })
    }
    fn accept_post<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.journal("accept_post");
            self.posts.lock().expect("lock").push(input);
            Ok(*self.accept_guard.lock().expect("lock"))
        })
    }
    fn accept_tombstone<'a>(
        &'a self,
        input: AcceptedDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.journal("accept_tombstone");
            self.tombstones.lock().expect("lock").push(input);
            Ok(*self.accept_guard.lock().expect("lock"))
        })
    }
    fn accept_marker<'a>(
        &'a self,
        input: AcceptedDiscussionMarker,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.journal("accept_marker");
            self.markers.lock().expect("lock").push(input);
            Ok(*self.accept_guard.lock().expect("lock"))
        })
    }
    fn terminal<'a>(
        &'a self,
        input: TerminalDiscussionDelivery,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            self.journal("terminal");
            self.terminals.lock().expect("lock").push(input);
            Ok(true)
        })
    }
}
struct FixedClock(i64);
impl WorkerClock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

fn record() -> DeliveryRecord {
    let envelope = DeliveryEnvelopeV1::build(
        "event".into(),
        &OrgId("acme".into()),
        &WebhookEvent::Published,
        &NotificationPayload {
            artifact_id: ArtifactId("artifact".into()),
            title: "title".into(),
            url: "https://example.test/a".into(),
            description: "description".into(),
            uploader_label: "uploader".into(),
            category: "category".into(),
            revision: 1,
            bytes: 1,
            viewer_email: None,
            body: None,
            resolver: None,
        },
    )
    .expect("valid envelope");
    let payload = envelope.canonical_bytes().expect("canonical envelope");
    DeliveryRecord {
        id: "row".into(),
        event_id: "event".into(),
        tenant: "acme".into(),
        event_type: "published".into(),
        target_key: "hook".into(),
        bucket_id: "hook".into(),
        secret_ref: "webhook:hook".into(),
        payload: payload.clone(),
        payload_sha256: envelope.payload_sha256().expect("envelope hash"),
        durability_intent_id: None,
        delivery_kind: "event".into(),
        ordering_key: "hook".into(),
        depends_on_outbox_id: None,
        state: "leased".into(),
        attempts: 1,
        next_attempt_at: 0,
        lease_owner: Some("test".into()),
        lease_expires_at: Some(1),
        lease_token: Some("lease".into()),
        lease_version: 1,
        result_classification: String::new(),
        duplicate_risk: false,
        discord_message_id: None,
        terminal_error: String::new(),
        created_at: 0,
        updated_at: 0,
        completed_at: None,
    }
}

fn record_with_preview() -> DeliveryRecord {
    let envelope = DeliveryEnvelopeV1::build_with_preview(
        "event".into(),
        &OrgId("acme".into()),
        &WebhookEvent::Published,
        &NotificationPayload {
            artifact_id: ArtifactId("abc123".into()),
            title: "title".into(),
            url: "https://example.test/abc123".into(),
            description: "description".into(),
            uploader_label: "uploader".into(),
            category: "category".into(),
            revision: 1,
            bytes: 1,
            viewer_email: None,
            body: None,
            resolver: None,
        },
        Some(DeliveryPreviewReferenceV1::new("abc123", 1, &"a".repeat(64)).expect("reference")),
    )
    .expect("valid envelope");
    let payload = envelope.canonical_bytes().expect("canonical envelope");
    DeliveryRecord {
        payload,
        payload_sha256: envelope.payload_sha256().expect("envelope hash"),
        ..record()
    }
}
fn webhook() -> WebhookDelivery {
    WebhookDelivery {
        id: WebhookId("hook".into()),
        org: OrgId("acme".into()),
        url: "https://discord.com/api/webhooks/1/token".into(),
        label: String::new(),
        events: vec![WebhookEvent::Published],
    }
}
fn rate() -> RateLimitMetadata {
    RateLimitMetadata {
        webhook_ref: Some("webhook:hook".into()),
        bucket: Some("bucket".into()),
        remaining: None,
        reset_after_ms: None,
        retry_after_ms: None,
        scope: None,
    }
}
fn make_worker(
    outbox: Arc<FakeOutbox>,
    hooks: Result<Option<WebhookDelivery>, WebhookResolutionFailure>,
    result: DeliveryClassification,
) -> (DeliveryWorker, Arc<FakeProvider>) {
    let (worker, provider, _) = make_worker_with_hooks(outbox, hooks, result);
    (worker, provider)
}
fn make_worker_with_hooks(
    outbox: Arc<FakeOutbox>,
    hooks: Result<Option<WebhookDelivery>, WebhookResolutionFailure>,
    result: DeliveryClassification,
) -> (DeliveryWorker, Arc<FakeProvider>, Arc<FakeWebhooks>) {
    let provider = Arc::new(FakeProvider {
        result: Mutex::new(result),
        calls: AtomicUsize::new(0),
    });
    let hooks = Arc::new(FakeWebhooks {
        response: Mutex::new(
            hooks.and_then(|row| row.ok_or(WebhookResolutionFailure::InvalidReference)),
        ),
        results: Mutex::new(Vec::new()),
    });
    (
        DeliveryWorker::with_adapters(
            outbox,
            hooks.clone(),
            provider.clone(),
            Arc::new(FixedClock(10_000)),
            "test".into(),
        ),
        provider,
        hooks,
    )
}

fn make_worker_with_preview(
    outbox: Arc<FakeOutbox>,
    previews: Arc<FakePreview>,
    result: DeliveryClassification,
) -> (DeliveryWorker, Arc<FakeProvider>) {
    let provider = Arc::new(FakeProvider {
        result: Mutex::new(result),
        calls: AtomicUsize::new(0),
    });
    (
        DeliveryWorker::with_adapters(
            outbox,
            Arc::new(FakeWebhooks {
                response: Mutex::new(Ok(webhook())),
                results: Mutex::new(Vec::new()),
            }),
            provider.clone(),
            Arc::new(FixedClock(10_000)),
            "test".into(),
        )
        .with_preview_resolver(previews),
        provider,
    )
}
async fn turn(worker: &DeliveryWorker) -> Result<WorkerTurn, AppError> {
    let (_, mut shutdown) = watch::channel(false);
    worker.run_once(&mut shutdown).await
}

fn discussion_record(operation: DiscordDiscussionOperationV1, kind: &str) -> DeliveryRecord {
    let envelope = DiscordDiscussionEnvelopeV1::build(
        "discussion-event".into(),
        &OrgId("acme".into()),
        "artifact-a".into(),
        "connection-a".into(),
        1,
        operation,
    )
    .expect("valid discussion envelope");
    let payload = envelope
        .canonical_bytes()
        .expect("canonical discussion envelope");
    DeliveryRecord {
        id: "discussion-row".into(),
        event_id: "discussion-event".into(),
        tenant: "acme".into(),
        event_type: "discussion".into(),
        target_key: "connection-a".into(),
        bucket_id: "connection-a".into(),
        secret_ref: "discussion:connection-a".into(),
        payload,
        payload_sha256: envelope.payload_sha256().expect("discussion hash"),
        delivery_kind: kind.into(),
        ordering_key: discussion_ordering_key(&ArtifactId("artifact-a".into()), 1)
            .expect("ordering"),
        ..record()
    }
}

fn discussion_state(thread_id: Option<&str>) -> ArtifactDiscussion {
    ArtifactDiscussion {
        artifact_id: ArtifactId("artifact-a".into()),
        org: OrgId("acme".into()),
        mode: DiscussionMode::DiscordMirror,
        connection_org: Some(OrgId("acme".into())),
        connection_id: Some("connection-a".into()),
        thread_id: thread_id.map(str::to_owned),
        root_message_id: None,
        state: DiscussionState::Connected,
        generation: 1,
        enabled_by: None,
        enabled_at: None,
        disabled_at: None,
        last_synced_at: None,
        last_error: None,
        created_at: None,
        updated_at: None,
        anchor_outbox_id: None,
    }
}

fn discussion_link(record: &DeliveryRecord, tombstone: bool) -> DiscussionMessageLink {
    DiscussionMessageLink {
        artifact_id: ArtifactId("artifact-a".into()),
        org: OrgId("acme".into()),
        connection_id: "connection-a".into(),
        feedback_id: FeedbackId("feedback-a".into()),
        delivery_event_id: "discussion-event".into(),
        outbox_id: if tombstone {
            "prior-row".into()
        } else {
            record.id.clone()
        },
        tombstone_outbox_id: tombstone.then(|| record.id.clone()),
        external_thread_id: Some("123456789012345678".into()),
        external_message_id: Some("223456789012345678".into()),
        generation: 1,
        state: "posted".into(),
        last_error: None,
        local_deleted_at: None,
        created_at: Timestamp("1".into()),
        updated_at: Timestamp("1".into()),
        posted_at: Some(Timestamp("1".into())),
    }
}

fn fake_discussions(record: &DeliveryRecord, thread_id: Option<&str>) -> Arc<FakeDiscussions> {
    Arc::new(FakeDiscussions {
        connection: Mutex::new(Ok(Some(DiscussionConnectionDelivery {
            org: OrgId("acme".into()),
            label: "discussion".into(),
            url: "https://discord.com/api/webhooks/123/token".into(),
            strategy: DiscussionConnectionStrategy::ForumWebhook,
            notification_webhook_id: None,
            notification_provider_webhook_id: None,
            channel_id: None,
            guild_id: None,
        }))),
        discussion: Mutex::new(Some(discussion_state(thread_id))),
        link: Mutex::new(Some(discussion_link(record, false))),
        posts: Mutex::new(Vec::new()),
        tombstones: Mutex::new(Vec::new()),
        markers: Mutex::new(Vec::new()),
        terminals: Mutex::new(Vec::new()),
        anchor: Mutex::new(None),
        anchor_calls: Mutex::new(Vec::new()),
        accept_guard: Mutex::new(true),
        ledger: Mutex::new(None),
    })
}

fn make_discussion_worker(
    outbox: Arc<FakeOutbox>,
    discussions: Arc<FakeDiscussions>,
    result: DiscussionResult,
) -> (DeliveryWorker, Arc<FakeDiscussionProvider>) {
    let provider = Arc::new(FakeDiscussionProvider {
        result: Mutex::new(result),
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let worker = DeliveryWorker::with_adapters(
        outbox,
        Arc::new(FakeWebhooks {
            response: Mutex::new(Ok(webhook())),
            results: Mutex::new(Vec::new()),
        }),
        Arc::new(FakeProvider {
            result: Mutex::new(DeliveryClassification::Terminal {
                reason: TerminalReason::ClientError,
            }),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(FixedClock(10_000)),
        "test".into(),
    )
    .with_discussion_adapters(discussions, provider.clone());
    (worker, provider)
}

#[tokio::test]
async fn accepted_persists_message_id_and_bucket_discovery_without_blocking() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let metadata = RateLimitMetadata {
        remaining: Some(1),
        ..rate()
    };
    let (worker, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "123".into(),
            rate_limit: metadata,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 10_000
            },
            Action::Accepted("123".into())
        ]
    );
}
#[tokio::test]
async fn rate_limit_uses_exact_delay_and_persists_block() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let metadata = RateLimitMetadata {
        retry_after_ms: Some(250),
        ..rate()
    };
    let (worker, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Retry {
            reason: RetryReason::RateLimited,
            duplicate_risk: DuplicateRisk::None,
            rate_limit: Some(metadata),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 10_250
            },
            Action::Retry(RetryTransition {
                next_attempt_at: 10_250,
                classification: "rate_limited".into(),
                duplicate_risk: false
            })
        ]
    );
}
#[tokio::test]
async fn global_rate_metadata_is_normalized_and_does_not_strand_the_lease() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let metadata = RateLimitMetadata {
        scope: Some(RateLimitScope::Global),
        remaining: Some(0),
        reset_after_ms: Some(500),
        ..rate()
    };
    let (worker, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "123".into(),
            rate_limit: metadata,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "global".into(),
                bucket: String::new(),
                until: 10_500
            },
            Action::Accepted("123".into())
        ]
    );
}
#[tokio::test]
async fn global_429_uses_exact_retry_delay_with_normalized_state() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let metadata = RateLimitMetadata {
        scope: Some(RateLimitScope::Global),
        retry_after_ms: Some(250),
        ..rate()
    };
    let (worker, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Retry {
            reason: RetryReason::RateLimited,
            duplicate_risk: DuplicateRisk::None,
            rate_limit: Some(metadata),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "global".into(),
                bucket: String::new(),
                until: 10_250
            },
            Action::Retry(RetryTransition {
                next_attempt_at: 10_250,
                classification: "rate_limited".into(),
                duplicate_risk: false
            })
        ]
    );
}
#[tokio::test]
async fn accepted_and_terminal_outcomes_record_advisory_webhook_status_after_guarding() {
    let accepted_outbox = Arc::new(FakeOutbox::one(record()));
    let (accepted, _, accepted_hooks) = make_worker_with_hooks(
        accepted_outbox,
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "123".into(),
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&accepted).await, Ok(WorkerTurn::Processed));
    assert_eq!(*accepted_hooks.results.lock().expect("lock"), vec![Ok(())]);

    let terminal_outbox = Arc::new(FakeOutbox::one(record()));
    let (terminal, _, terminal_hooks) = make_worker_with_hooks(
        terminal_outbox,
        Ok(Some(webhook())),
        DeliveryClassification::Terminal {
            reason: TerminalReason::BadRequest,
        },
    );
    assert_eq!(turn(&terminal).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        *terminal_hooks.results.lock().expect("lock"),
        vec![Err("bad_request".into())]
    );
}
#[tokio::test]
async fn lookup_failure_retries_but_missing_or_cross_tenant_is_terminal_without_send() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Err(WebhookResolutionFailure::Retryable),
        DeliveryClassification::Terminal {
            reason: TerminalReason::BadRequest,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Retry(RetryTransition {
            next_attempt_at: 11_000,
            classification: "network".into(),
            duplicate_risk: true
        })]
    );
    let outbox = Arc::new(FakeOutbox::one(record()));
    let mut wrong = webhook();
    wrong.org = OrgId("other".into());
    let (worker, provider) = make_worker(
        outbox.clone(),
        Ok(Some(wrong)),
        DeliveryClassification::Terminal {
            reason: TerminalReason::BadRequest,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("invalid_webhook".into())]
    );
    let outbox = Arc::new(FakeOutbox::one(record()));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Err(WebhookResolutionFailure::DecryptFailed),
        DeliveryClassification::Terminal {
            reason: TerminalReason::BadRequest,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("decrypt_failed".into())]
    );
}
#[tokio::test]
async fn hash_mismatch_and_attempt_exhaustion_dead_letter_without_send() {
    let mut bad = record();
    bad.payload_sha256 = "bad".into();
    let outbox = Arc::new(FakeOutbox::one(bad));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "1".into(),
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("payload_hash_mismatch".into())]
    );
    let mut exhausted = record();
    exhausted.attempts = MAX_ATTEMPTS;
    let outbox = Arc::new(FakeOutbox::one(exhausted));
    let (worker, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Retry {
            reason: RetryReason::Network,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("attempts_exhausted".into())]
    );
    assert_eq!(outbox.dead_risks(), vec![true]);
}
#[tokio::test]
async fn envelope_tamper_or_binding_mismatch_never_reaches_provider() {
    let mut tampered = record();
    let mut value: serde_json::Value =
        serde_json::from_slice(&tampered.payload).expect("envelope JSON");
    value["tenant"] = serde_json::Value::String("other".into());
    tampered.payload = serde_json::to_vec(&value).expect("tampered JSON");
    tampered.payload_sha256 = hex::encode(sha2::Sha256::digest(&tampered.payload));
    let outbox = Arc::new(FakeOutbox::one(tampered));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "1".into(),
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("validation_failed".into())]
    );
}
#[tokio::test]
async fn stale_guard_is_surfaced() {
    let outbox = Arc::new(FakeOutbox {
        record: Mutex::new(Some(record())),
        actions: Mutex::new(Vec::new()),
        dead_risks: Mutex::new(Vec::new()),
        guarded: false,
        on_claim: Mutex::new(None),
    });
    let (worker, _) = make_worker(
        outbox,
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "1".into(),
            rate_limit: rate(),
        },
    );
    assert!(matches!(turn(&worker).await, Err(AppError::Conflict(_))));
}

#[tokio::test]
async fn shutdown_before_send_releases_lease_as_a_safe_network_retry() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "1".into(),
            rate_limit: rate(),
        },
    );
    let (shutdown_tx, mut shutdown) = watch::channel(false);
    *outbox.on_claim.lock().expect("lock") = Some(shutdown_tx);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Ok(WorkerTurn::Shutdown)
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Retry(RetryTransition {
            next_attempt_at: 11_000,
            classification: "network".into(),
            duplicate_risk: false,
        })]
    );
}

#[tokio::test]
async fn two_workers_cannot_claim_the_same_ready_row() {
    let outbox = Arc::new(FakeOutbox::one(record()));
    let (first, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "1".into(),
            rate_limit: rate(),
        },
    );
    let (second, _) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Accepted {
            message_id: "2".into(),
            rate_limit: rate(),
        },
    );
    let (_, mut first_shutdown) = watch::channel(false);
    let (_, mut second_shutdown) = watch::channel(false);
    let (first_turn, second_turn) = tokio::join!(
        first.run_once(&mut first_shutdown),
        second.run_once(&mut second_shutdown)
    );
    assert!(matches!(first_turn, Ok(WorkerTurn::Processed)));
    assert!(matches!(second_turn, Ok(WorkerTurn::Idle)));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 10_000,
            },
            Action::Accepted("1".into())
        ]
    );
}

#[tokio::test]
async fn missing_or_failed_preview_falls_back_to_json_delivery() {
    for outcome in [
        Ok(None),
        Err(AppError::Unavailable("preview renderer unavailable".into())),
    ] {
        let outbox = Arc::new(FakeOutbox::one(record_with_preview()));
        let previews = Arc::new(FakePreview {
            outcome,
            calls: AtomicUsize::new(0),
        });
        let (worker, provider) = make_worker_with_preview(
            outbox.clone(),
            previews.clone(),
            DeliveryClassification::Accepted {
                message_id: "1".into(),
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(previews.calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(
            outbox
                .actions()
                .iter()
                .any(|action| matches!(action, Action::Accepted(message) if message == "1"))
        );
    }
}

#[tokio::test]
async fn discussion_thread_accepts_provider_thread_and_message_receipt() {
    let record = discussion_record(
        DiscordDiscussionOperationV1::thread("feedback-a".into(), "Thread".into(), "Body".into())
            .expect("thread"),
        DELIVERY_KIND_DISCUSSION_THREAD,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, None);
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        DiscussionResult::Accepted {
            receipt: DiscussionReceipt {
                message_id: "223456789012345678".into(),
                thread_id: Some("123456789012345678".into()),
            },
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        discussions.posts.lock().expect("lock")[0].external_thread_id,
        "123456789012345678"
    );
    assert_eq!(
        discussions.posts.lock().expect("lock")[0].external_message_id,
        "223456789012345678"
    );
}

#[tokio::test]
async fn discussion_reply_uses_persisted_thread_and_accepts_post() {
    let record = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some("123456789012345678"));
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        DiscussionResult::Accepted {
            receipt: DiscussionReceipt {
                message_id: "323456789012345678".into(),
                thread_id: None,
            },
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        discussions.posts.lock().expect("lock")[0].external_thread_id,
        "123456789012345678"
    );
}

#[tokio::test]
async fn discussion_markers_use_persisted_thread_and_marker_acceptor() {
    for operation in [
        DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
        DiscordDiscussionOperationV1::reopened("feedback-a".into()).expect("reopened"),
    ] {
        let record = discussion_record(operation, DELIVERY_KIND_DISCUSSION_MESSAGE);
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Accepted {
                receipt: DiscussionReceipt {
                    message_id: "423456789012345678".into(),
                    thread_id: None,
                },
                rate_limit: rate(),
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert_eq!(discussions.markers.lock().expect("lock")[0].generation, 1);
        assert!(discussions.posts.lock().expect("lock").is_empty());
    }
}

#[tokio::test]
async fn discussion_tombstone_uses_retained_link_and_tombstone_acceptor() {
    let record = discussion_record(
        DiscordDiscussionOperationV1::tombstone("feedback-a".into()).expect("tombstone"),
        DELIVERY_KIND_DISCUSSION_TOMBSTONE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some("123456789012345678"));
    *discussions.link.lock().expect("lock") = Some(discussion_link(&record, true));
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        DiscussionResult::Accepted {
            receipt: DiscussionReceipt {
                message_id: "223456789012345678".into(),
                thread_id: None,
            },
            rate_limit: rate(),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        discussions.tombstones.lock().expect("lock")[0].external_thread_id,
        "123456789012345678"
    );
    assert_eq!(
        discussions.tombstones.lock().expect("lock")[0].external_message_id,
        "223456789012345678"
    );
}

#[tokio::test]
async fn malformed_discussion_payload_dead_letters_without_provider_call() {
    let mut record = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    record.payload = b"{".to_vec();
    record.payload_sha256 = format!("{:x}", sha2::Sha256::digest(&record.payload));
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some("123456789012345678"));
    let (worker, provider) = make_discussion_worker(
        outbox.clone(),
        discussions,
        DiscussionResult::Terminal {
            reason: TerminalReason::ClientError,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("validation_failed".into())]
    );
}

#[tokio::test]
async fn discussion_claim_binding_mismatches_never_reach_provider() {
    let base = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let mut records = Vec::new();
    let mut wrong_tenant = base.clone();
    wrong_tenant.tenant = "other".into();
    records.push(wrong_tenant);
    let mut wrong_target = base.clone();
    wrong_target.target_key = "other-connection".into();
    records.push(wrong_target);
    let mut wrong_secret = base.clone();
    wrong_secret.secret_ref = "discussion:other-connection".into();
    records.push(wrong_secret);
    let mut wrong_ordering = base.clone();
    wrong_ordering.ordering_key = "discussion:artifact-b:1".into();
    records.push(wrong_ordering);
    let mut wrong_kind = base.clone();
    wrong_kind.delivery_kind = DELIVERY_KIND_DISCUSSION_THREAD.into();
    records.push(wrong_kind);

    for record in records {
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        let (worker, provider) = make_discussion_worker(
            outbox.clone(),
            discussions.clone(),
            DiscussionResult::Terminal {
                reason: TerminalReason::ClientError,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert!(
            !outbox.actions().is_empty() || !discussions.terminals.lock().expect("lock").is_empty()
        );
    }

    for mismatch in ["generation", "link"] {
        let record = base.clone();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some("123456789012345678"));
        if mismatch == "generation" {
            discussions
                .discussion
                .lock()
                .expect("lock")
                .as_mut()
                .expect("discussion")
                .generation = 2;
        } else {
            discussions
                .link
                .lock()
                .expect("lock")
                .as_mut()
                .expect("link")
                .outbox_id = "other-row".into();
        }
        let (worker, provider) = make_discussion_worker(
            outbox,
            discussions.clone(),
            DiscussionResult::Terminal {
                reason: TerminalReason::ClientError,
            },
        );
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(discussions.terminals.lock().expect("lock").len(), 1);
    }
}

#[tokio::test]
async fn discussion_credential_retry_is_retried_without_provider_call() {
    let record = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some("123456789012345678"));
    *discussions.connection.lock().expect("lock") = Err(DiscussionResolutionFailure::Retryable);
    let (worker, provider) = make_discussion_worker(
        outbox.clone(),
        discussions,
        DiscussionResult::Terminal {
            reason: TerminalReason::ClientError,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(outbox.actions().as_slice(), [Action::Retry(update)] if update.classification == "network")
    );
}

// --- Characterization of `process_discussion` branch outcomes -----------------------------
//
// These lock the persistence call, classification, feedback correlation, and provider-call
// count for every early exit and outcome so the pipeline can be restructured safely.

const THREAD: &str = "123456789012345678";
const ANCHOR: &str = "523456789012345678";
const CHANNEL: &str = "623456789012345678";

fn reply_record() -> DeliveryRecord {
    discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    )
}

fn thread_record() -> DeliveryRecord {
    discussion_record(
        DiscordDiscussionOperationV1::thread("feedback-a".into(), "Thread".into(), "Body".into())
            .expect("thread"),
        DELIVERY_KIND_DISCUSSION_THREAD,
    )
}

fn accepted(message: &str, thread: Option<&str>) -> DiscussionResult {
    DiscussionResult::Accepted {
        receipt: DiscussionReceipt {
            message_id: message.into(),
            thread_id: thread.map(str::to_owned),
        },
        rate_limit: rate(),
    }
}

fn client_error() -> DiscussionResult {
    DiscussionResult::Terminal {
        reason: TerminalReason::ClientError,
    }
}

/// `(classification, feedback_id, duplicate_risk)` for each guarded terminal update.
fn terminal_summary(discussions: &FakeDiscussions) -> Vec<(String, Option<String>, bool)> {
    discussions
        .terminals
        .lock()
        .expect("lock")
        .iter()
        .map(|row| {
            (
                row.classification.clone(),
                row.feedback_id.as_ref().map(|id| id.0.clone()),
                row.duplicate_risk,
            )
        })
        .collect()
}

fn terminal(class: &str, feedback: Option<&str>, risk: bool) -> (String, Option<String>, bool) {
    (class.to_owned(), feedback.map(str::to_owned), risk)
}

fn notification_thread(discussions: &FakeDiscussions) {
    if let Ok(Some(connection)) = discussions.connection.lock().expect("lock").as_mut() {
        connection.strategy = DiscussionConnectionStrategy::NotificationThread;
        connection.channel_id = Some(CHANNEL.into());
    }
    *discussions.anchor.lock().expect("lock") = Some(ANCHOR.into());
}

#[tokio::test]
async fn discussion_rows_without_discussion_adapters_dead_letter_as_validation_failed() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record));
    let (worker, provider) = make_worker(
        outbox.clone(),
        Ok(Some(webhook())),
        DeliveryClassification::Terminal {
            reason: TerminalReason::ClientError,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("validation_failed".into())]
    );
}

#[tokio::test]
async fn discussion_hash_mismatch_dead_letters_before_discussion_state_is_read() {
    let mut record = reply_record();
    record.payload_sha256 = "0".repeat(64);
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, provider) =
        make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        outbox.actions(),
        vec![Action::Dead("payload_hash_mismatch".into())]
    );
    assert!(terminal_summary(&discussions).is_empty());
}

#[tokio::test]
async fn non_canonical_or_cross_tenant_envelopes_dead_letter_without_discussion_state() {
    let mut non_canonical = reply_record();
    let value: serde_json::Value =
        serde_json::from_slice(&non_canonical.payload).expect("canonical JSON");
    non_canonical.payload = serde_json::to_vec_pretty(&value).expect("pretty JSON");
    non_canonical.payload_sha256 = hex::encode(Sha256::digest(&non_canonical.payload));
    let mut cross_tenant = reply_record();
    cross_tenant.tenant = "other".into();

    for record in [non_canonical, cross_tenant] {
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let (worker, provider) =
            make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            outbox.actions(),
            vec![Action::Dead("validation_failed".into())]
        );
        assert!(terminal_summary(&discussions).is_empty());
    }
}

#[tokio::test]
async fn discussion_binding_and_authority_failures_are_terminal_before_provider_io() {
    type RecordEdit = fn(&mut DeliveryRecord);
    type StateEdit = fn(&FakeDiscussions);
    let keep_record: RecordEdit = |_| {};
    let keep_state: StateEdit = |_| {};
    let cases: &[(&str, RecordEdit, StateEdit, &str)] = &[
        (
            "target",
            |record| record.target_key = "connection-b".into(),
            keep_state,
            "validation_failed",
        ),
        (
            "secret",
            |record| record.secret_ref = "discussion:connection-b".into(),
            keep_state,
            "validation_failed",
        ),
        (
            "ordering",
            |record| record.ordering_key = "discussion:artifact-b:1".into(),
            keep_state,
            "validation_failed",
        ),
        (
            "kind",
            |record| record.delivery_kind = DELIVERY_KIND_DISCUSSION_TOMBSTONE.into(),
            keep_state,
            "validation_failed",
        ),
        (
            "discussion missing",
            keep_record,
            |state| *state.discussion.lock().expect("lock") = None,
            "validation_failed",
        ),
        (
            "foreign connection",
            keep_record,
            |state| {
                state
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .connection_id = Some("connection-b".into());
            },
            "validation_failed",
        ),
        (
            "disconnected",
            keep_record,
            |state| {
                state
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .connection_id = None;
            },
            "validation_failed",
        ),
        (
            "stale generation",
            keep_record,
            |state| {
                state
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .generation = 2;
            },
            "validation_failed",
        ),
        (
            "link missing",
            keep_record,
            |state| *state.link.lock().expect("lock") = None,
            "validation_failed",
        ),
        (
            "link generation",
            keep_record,
            |state| {
                state
                    .link
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("link")
                    .generation = 2;
            },
            "validation_failed",
        ),
        (
            "reply without thread",
            keep_record,
            |state| {
                state
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .thread_id = None;
            },
            "validation_failed",
        ),
        (
            "connection missing",
            keep_record,
            |state| *state.connection.lock().expect("lock") = Ok(None),
            "validation_failed",
        ),
        (
            "connection cross-tenant",
            keep_record,
            |state| {
                if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                    connection.org = OrgId("other".into());
                }
            },
            "validation_failed",
        ),
        (
            "connection invalid reference",
            keep_record,
            |state| {
                *state.connection.lock().expect("lock") =
                    Err(DiscussionResolutionFailure::InvalidReference);
            },
            "validation_failed",
        ),
        (
            "connection decrypt",
            keep_record,
            |state| {
                *state.connection.lock().expect("lock") =
                    Err(DiscussionResolutionFailure::DecryptFailed);
            },
            "decrypt_failed",
        ),
        (
            "persisted thread is not a snowflake",
            keep_record,
            |state| {
                state
                    .discussion
                    .lock()
                    .expect("lock")
                    .as_mut()
                    .expect("discussion")
                    .thread_id = Some("not-a-snowflake".into());
            },
            "validation_failed",
        ),
        (
            "connection url outside allowlist",
            keep_record,
            |state| {
                if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                    connection.url = "https://example.test/api/webhooks/123/token".into();
                }
            },
            "allowlist_rejected",
        ),
    ];

    for &(name, edit_record, edit_state, class) in cases {
        let mut record = reply_record();
        edit_record(&mut record);
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        edit_state(&discussions);
        let (worker, provider) =
            make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{name}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{name}");
        assert!(outbox.actions().is_empty(), "{name}");
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal(class, Some("feedback-a"), false)],
            "{name}"
        );
        let terminals = discussions.terminals.lock().expect("lock");
        let row = &terminals[0];
        assert_eq!(
            (
                row.outbox_id.as_str(),
                row.worker.as_str(),
                row.lease_token.as_str(),
                row.lease_version,
                row.artifact_id.0.as_str(),
                row.org.0.as_str(),
                row.connection_id.as_str(),
                row.generation,
                row.delivery_kind.as_str(),
                row.now_millis,
            ),
            (
                "discussion-row",
                "test",
                "lease",
                1,
                "artifact-a",
                "acme",
                "connection-a",
                1,
                record.delivery_kind.as_str(),
                10_000,
            ),
            "{name}"
        );
    }
}

#[tokio::test]
async fn paused_discussion_in_the_current_generation_still_delivers() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    discussions
        .discussion
        .lock()
        .expect("lock")
        .as_mut()
        .expect("discussion")
        .state = DiscussionState::Paused;
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        accepted("323456789012345678", None),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(discussions.posts.lock().expect("lock").len(), 1);
    assert!(terminal_summary(&discussions).is_empty());
}

#[tokio::test]
async fn tombstone_without_retained_external_ids_is_terminal_before_provider_io() {
    for missing in ["link", "thread", "message"] {
        let record = discussion_record(
            DiscordDiscussionOperationV1::tombstone("feedback-a".into()).expect("tombstone"),
            DELIVERY_KIND_DISCUSSION_TOMBSTONE,
        );
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, Some(THREAD));
        let mut link = discussion_link(&record, true);
        match missing {
            "thread" => link.external_thread_id = None,
            "message" => link.external_message_id = None,
            _ => {}
        }
        *discussions.link.lock().expect("lock") = (missing != "link").then_some(link);
        let (worker, provider) =
            make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{missing}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{missing}");
        assert!(outbox.actions().is_empty(), "{missing}");
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal("validation_failed", Some("feedback-a"), false)],
            "{missing}"
        );
    }
}

#[tokio::test]
async fn marker_failures_clear_the_feedback_correlation() {
    // Early validation exit: a marker needs a persisted thread.
    let record = discussion_record(
        DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, None);
    let (worker, provider) = make_discussion_worker(outbox, discussions.clone(), client_error());
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        terminal_summary(&discussions),
        vec![terminal("validation_failed", None, false)]
    );

    // Provider terminal outcome for a reopened marker.
    let record = discussion_record(
        DiscordDiscussionOperationV1::reopened("feedback-a".into()).expect("reopened"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        DiscussionResult::Terminal {
            reason: TerminalReason::NotFound,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        terminal_summary(&discussions),
        vec![terminal("not_found", None, false)]
    );
    assert!(discussions.markers.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn notification_thread_creates_the_thread_from_the_anchor_message() {
    let mut record = thread_record();
    record.depends_on_outbox_id = Some("anchor-row".into());
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, None);
    notification_thread(&discussions);
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        accepted("223456789012345678", Some(THREAD)),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        *discussions.anchor_calls.lock().expect("lock"),
        vec!["anchor-row".to_owned()]
    );
    let expected = discussion_request(
        "https://discord.com/api/webhooks/123/token",
        "discussion:connection-a".into(),
        DiscussionOperation::create_thread_from_message(
            CHANNEL.into(),
            ANCHOR.into(),
            "Thread".into(),
            "Body".into(),
        )
        .expect("anchored thread"),
    )
    .expect("anchored request");
    assert_eq!(*provider.requests.lock().expect("lock"), vec![expected]);
    assert_eq!(
        discussions.posts.lock().expect("lock")[0].external_thread_id,
        THREAD
    );
}

#[tokio::test]
async fn notification_thread_without_anchor_or_channel_is_terminal_before_provider_io() {
    type StateEdit = fn(&FakeDiscussions);
    let cases: &[(&str, StateEdit, &str)] = &[
        (
            "anchor missing",
            |state| *state.anchor.lock().expect("lock") = None,
            "validation_failed",
        ),
        (
            "channel missing",
            |state| {
                if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                    connection.channel_id = None;
                }
            },
            "validation_failed",
        ),
        (
            "channel invalid",
            |state| {
                if let Ok(Some(connection)) = state.connection.lock().expect("lock").as_mut() {
                    connection.channel_id = Some("channel".into());
                }
            },
            "bad_request",
        ),
    ];
    for &(name, edit, class) in cases {
        let record = thread_record();
        let outbox = Arc::new(FakeOutbox::one(record.clone()));
        let discussions = fake_discussions(&record, None);
        notification_thread(&discussions);
        edit(&discussions);
        let (worker, provider) =
            make_discussion_worker(outbox.clone(), discussions.clone(), client_error());
        assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed), "{name}");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0, "{name}");
        assert!(outbox.actions().is_empty(), "{name}");
        assert_eq!(
            terminal_summary(&discussions),
            vec![terminal(class, Some("feedback-a"), false)],
            "{name}"
        );
    }
}

#[tokio::test]
async fn notification_thread_replies_use_the_persisted_thread_without_an_anchor() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    notification_thread(&discussions);
    let (worker, provider) = make_discussion_worker(
        outbox,
        discussions.clone(),
        accepted("323456789012345678", None),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert!(discussions.anchor_calls.lock().expect("lock").is_empty());
    let expected = discussion_request(
        "https://discord.com/api/webhooks/123/token",
        "discussion:connection-a".into(),
        DiscussionOperation::reply(THREAD.into(), "Body".into()).expect("reply"),
    )
    .expect("reply request");
    assert_eq!(*provider.requests.lock().expect("lock"), vec![expected]);
}

#[tokio::test]
async fn thread_acceptance_without_a_thread_receipt_retries_as_ambiguous() {
    let record = thread_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, None);
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions.clone(),
        accepted("223456789012345678", None),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 10_000,
            },
            Action::Retry(RetryTransition {
                next_attempt_at: 11_000,
                classification: "ambiguous".into(),
                duplicate_risk: true,
            }),
        ]
    );
    assert!(discussions.posts.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn discussion_rate_limit_is_persisted_before_the_exact_retry() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions,
        DiscussionResult::Retry {
            reason: RetryReason::RateLimited,
            duplicate_risk: DuplicateRisk::None,
            rate_limit: Some(RateLimitMetadata {
                retry_after_ms: Some(2_500),
                ..rate()
            }),
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 12_500,
            },
            Action::Retry(RetryTransition {
                next_attempt_at: 12_500,
                classification: "rate_limited".into(),
                duplicate_risk: false,
            }),
        ]
    );
}

#[tokio::test]
async fn discussion_rate_limit_without_retry_after_is_an_error_without_transition() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions.clone(),
        DiscussionResult::Retry {
            reason: RetryReason::RateLimited,
            duplicate_risk: DuplicateRisk::None,
            rate_limit: None,
        },
    );
    assert_eq!(
        turn(&worker).await,
        Err(AppError::Validation(
            "rate_limited outcome missing retry_after".into()
        ))
    );
    assert!(outbox.actions().is_empty());
    assert!(terminal_summary(&discussions).is_empty());
}

#[tokio::test]
async fn discussion_retry_backs_off_by_attempt_and_carries_duplicate_risk() {
    let mut record = reply_record();
    record.attempts = 3;
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions,
        DiscussionResult::Retry {
            reason: RetryReason::Timeout,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![Action::Retry(RetryTransition {
            next_attempt_at: 40_000,
            classification: "timeout".into(),
            duplicate_risk: true,
        })]
    );
}

#[tokio::test]
async fn exhausted_discussion_retry_is_terminal_with_its_duplicate_risk() {
    let mut record = reply_record();
    record.attempts = MAX_ATTEMPTS;
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions.clone(),
        DiscussionResult::Retry {
            reason: RetryReason::ServerError,
            duplicate_risk: DuplicateRisk::Possible,
            rate_limit: None,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert!(outbox.actions().is_empty());
    assert_eq!(
        terminal_summary(&discussions),
        vec![terminal("attempts_exhausted", Some("feedback-a"), true)]
    );
}

#[tokio::test]
async fn discussion_provider_terminal_maps_its_reason_code() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, provider) = make_discussion_worker(
        outbox.clone(),
        discussions.clone(),
        DiscussionResult::Terminal {
            reason: TerminalReason::Forbidden,
        },
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(outbox.actions().is_empty());
    assert_eq!(
        terminal_summary(&discussions),
        vec![terminal("forbidden", Some("feedback-a"), false)]
    );
}

#[tokio::test]
async fn discussion_acceptance_follows_rate_persistence_and_binds_the_lease() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    *discussions.ledger.lock().expect("lock") = Some(outbox.clone());
    let (worker, _) = make_discussion_worker(
        outbox.clone(),
        discussions.clone(),
        accepted("323456789012345678", None),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        outbox.actions(),
        vec![
            Action::Rate {
                scope: "bucket".into(),
                bucket: "bucket".into(),
                until: 10_000,
            },
            Action::Discussion("accept_post"),
        ]
    );
    assert_eq!(
        *discussions.posts.lock().expect("lock"),
        vec![AcceptedDiscussionDelivery {
            outbox_id: "discussion-row".into(),
            worker: "test".into(),
            lease_token: "lease".into(),
            lease_version: 1,
            external_thread_id: THREAD.into(),
            external_message_id: "323456789012345678".into(),
            now_millis: 10_000,
        }]
    );
    *discussions.ledger.lock().expect("lock") = None;

    // Marker acceptance carries the full generation-scoped identity instead of a link.
    let record = discussion_record(
        DiscordDiscussionOperationV1::resolved("feedback-a".into()).expect("resolved"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    let (worker, _) = make_discussion_worker(
        outbox,
        discussions.clone(),
        accepted("423456789012345678", None),
    );
    assert_eq!(turn(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        *discussions.markers.lock().expect("lock"),
        vec![AcceptedDiscussionMarker {
            outbox_id: "discussion-row".into(),
            worker: "test".into(),
            lease_token: "lease".into(),
            lease_version: 1,
            artifact_id: ArtifactId("artifact-a".into()),
            org: OrgId("acme".into()),
            connection_id: "connection-a".into(),
            generation: 1,
            message_id: "423456789012345678".into(),
            now_millis: 10_000,
        }]
    );
}

#[tokio::test]
async fn lost_discussion_acceptance_guard_is_surfaced() {
    let record = reply_record();
    let outbox = Arc::new(FakeOutbox::one(record.clone()));
    let discussions = fake_discussions(&record, Some(THREAD));
    *discussions.accept_guard.lock().expect("lock") = false;
    let (worker, _) =
        make_discussion_worker(outbox, discussions, accepted("323456789012345678", None));
    assert_eq!(
        turn(&worker).await,
        Err(AppError::Conflict(
            "outbox guarded discussion accepted lost lease".into()
        ))
    );
}

// --- Characterization of the event path, claim/lease handling, backoff and shutdown -------
//
// One shared ledger records every port call with its arguments, so these tests pin the order
// of awaits across the outbox, webhook store, preview resolver and provider, not only the
// final transition.
mod characterization;
