//! Characterization of the event path, claim/lease handling, backoff and shutdown.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Claim(String),
    Accepted {
        row: String,
        worker: String,
        message: String,
    },
    Retry {
        row: String,
        worker: String,
        update: RetryTransition,
    },
    Dead {
        row: String,
        worker: String,
        update: DeadLetterTransition,
    },
    Rate {
        scope: String,
        target: String,
        bucket: String,
        secret: String,
        until: i64,
    },
    Lookup {
        id: String,
        org: String,
    },
    WebhookResult(Result<(), String>),
    Preview,
    Provider(String),
}

type Ledger = Arc<Mutex<Vec<Call>>>;

fn push(ledger: &Ledger, call: Call) {
    ledger.lock().expect("lock").push(call);
}

struct LedgerOutbox {
    ledger: Ledger,
    record: Mutex<Option<DeliveryRecord>>,
    guarded: bool,
    claim_error: bool,
    rate_error: bool,
    on_claim: Mutex<Option<watch::Sender<bool>>>,
}
impl WorkerOutbox for LedgerOutbox {
    fn claim<'a>(
        &'a self,
        worker: String,
    ) -> BoxFuture<'a, Result<Option<DeliveryRecord>, AppError>> {
        Box::pin(async move {
            push(&self.ledger, Call::Claim(worker));
            if self.claim_error {
                return Err(AppError::Unavailable("claim failed".into()));
            }
            let row = self.record.lock().expect("lock").take();
            if let Some(sender) = self.on_claim.lock().expect("lock").take() {
                sender.send(true).expect("receiver live");
            }
            Ok(row)
        })
    }
    fn accepted<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        message: String,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::Accepted {
                    row: record.id.clone(),
                    worker,
                    message,
                },
            );
            Ok(self.guarded)
        })
    }
    fn retry<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: RetryTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::Retry {
                    row: record.id.clone(),
                    worker,
                    update,
                },
            );
            Ok(self.guarded)
        })
    }
    fn dead_letter<'a>(
        &'a self,
        record: &'a DeliveryRecord,
        worker: String,
        update: DeadLetterTransition,
    ) -> BoxFuture<'a, Result<bool, AppError>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::Dead {
                    row: record.id.clone(),
                    worker,
                    update,
                },
            );
            Ok(self.guarded)
        })
    }
    fn persist_rate_limit<'a>(
        &'a self,
        scope: String,
        target: String,
        bucket: String,
        secret: String,
        blocked_until: i64,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::Rate {
                    scope,
                    target,
                    bucket,
                    secret,
                    until: blocked_until,
                },
            );
            if self.rate_error {
                return Err(AppError::Unavailable("rate store failed".into()));
            }
            Ok(())
        })
    }
}

struct LedgerWebhooks {
    ledger: Ledger,
    response: Result<WebhookDelivery, WebhookResolutionFailure>,
    result_error: bool,
}
impl WorkerWebhooks for LedgerWebhooks {
    fn delivery<'a>(
        &'a self,
        id: &'a WebhookId,
        org: &'a OrgId,
    ) -> BoxFuture<'a, Result<WebhookDelivery, WebhookResolutionFailure>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::Lookup {
                    id: id.0.clone(),
                    org: org.0.clone(),
                },
            );
            self.response.clone()
        })
    }
    fn record_result<'a>(
        &'a self,
        _: &'a WebhookId,
        outcome: Result<(), &'static str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            push(
                &self.ledger,
                Call::WebhookResult(outcome.map_err(str::to_owned)),
            );
            if self.result_error {
                return Err(AppError::Unavailable("status write failed".into()));
            }
            Ok(())
        })
    }
}

struct LedgerProvider {
    ledger: Ledger,
    result: DeliveryClassification,
}
impl WorkerProvider for LedgerProvider {
    fn deliver<'a>(&'a self, request: ProviderRequest) -> BoxFuture<'a, DeliveryClassification> {
        Box::pin(async move {
            // `ProviderRequest`'s Debug redacts the URL, reference and body but keeps the
            // content type, which is what distinguishes JSON from a preview attachment.
            push(&self.ledger, Call::Provider(format!("{request:?}")));
            self.result.clone()
        })
    }
}

struct LedgerPreview {
    ledger: Ledger,
    png: Option<Vec<u8>>,
}
impl WorkerPreviewResolver for LedgerPreview {
    fn preview<'a>(
        &'a self,
        _: &'a OrgId,
        _: &'a DeliveryPreviewReferenceV1,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, AppError>> {
        Box::pin(async move {
            push(&self.ledger, Call::Preview);
            Ok(self.png.clone())
        })
    }
}

struct Setup {
    record: Option<DeliveryRecord>,
    webhook: Result<WebhookDelivery, WebhookResolutionFailure>,
    result: DeliveryClassification,
    guarded: bool,
    claim_error: bool,
    rate_error: bool,
    result_error: bool,
    preview: Option<Option<Vec<u8>>>,
}
impl Setup {
    fn new(record: DeliveryRecord, result: DeliveryClassification) -> Self {
        Self {
            record: Some(record),
            webhook: Ok(webhook()),
            result,
            guarded: true,
            claim_error: false,
            rate_error: false,
            result_error: false,
            preview: None,
        }
    }
    fn build(self) -> (DeliveryWorker, Arc<LedgerOutbox>, Ledger) {
        let ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
        let outbox = Arc::new(LedgerOutbox {
            ledger: ledger.clone(),
            record: Mutex::new(self.record),
            guarded: self.guarded,
            claim_error: self.claim_error,
            rate_error: self.rate_error,
            on_claim: Mutex::new(None),
        });
        let mut worker = DeliveryWorker::with_adapters(
            outbox.clone(),
            Arc::new(LedgerWebhooks {
                ledger: ledger.clone(),
                response: self.webhook,
                result_error: self.result_error,
            }),
            Arc::new(LedgerProvider {
                ledger: ledger.clone(),
                result: self.result,
            }),
            Arc::new(FixedClock(10_000)),
            "w1".into(),
        );
        if let Some(png) = self.preview {
            worker = worker.with_preview_resolver(Arc::new(LedgerPreview {
                ledger: ledger.clone(),
                png,
            }));
        }
        (worker, outbox, ledger)
    }
}

fn calls(ledger: &Ledger) -> Vec<Call> {
    std::mem::take(&mut *ledger.lock().expect("lock"))
}
fn claim() -> Call {
    Call::Claim("w1".into())
}
fn lookup() -> Call {
    Call::Lookup {
        id: "hook".into(),
        org: "acme".into(),
    }
}
fn json_send() -> Call {
    Call::Provider(
        "ProviderRequest { url: \"<redacted>\", webhook_ref: \"<redacted>\", \
         content_type: \"application/json\", payload: \"<redacted>\" }"
            .into(),
    )
}
fn dead(class: &str, duplicate_risk: bool) -> Call {
    Call::Dead {
        row: "row".into(),
        worker: "w1".into(),
        update: DeadLetterTransition {
            classification: class.into(),
            error: class.into(),
            duplicate_risk,
        },
    }
}
fn retry(next_attempt_at: i64, class: &str, duplicate_risk: bool) -> Call {
    Call::Retry {
        row: "row".into(),
        worker: "w1".into(),
        update: RetryTransition {
            next_attempt_at,
            classification: class.into(),
            duplicate_risk,
        },
    }
}
fn accepted_call(message: &str) -> Call {
    Call::Accepted {
        row: "row".into(),
        worker: "w1".into(),
        message: message.into(),
    }
}
fn bucket_rate(until: i64) -> Call {
    Call::Rate {
        scope: "bucket".into(),
        target: "hook".into(),
        bucket: "bucket".into(),
        secret: "webhook:hook".into(),
        until,
    }
}
fn accepted(message: &str, rate_limit: RateLimitMetadata) -> DeliveryClassification {
    DeliveryClassification::Accepted {
        message_id: message.into(),
        rate_limit,
    }
}
fn retry_outcome(
    reason: RetryReason,
    duplicate_risk: DuplicateRisk,
    rate_limit: Option<RateLimitMetadata>,
) -> DeliveryClassification {
    DeliveryClassification::Retry {
        reason,
        duplicate_risk,
        rate_limit,
    }
}
fn with_attempts(attempts: i64) -> DeliveryRecord {
    DeliveryRecord {
        attempts,
        ..record()
    }
}
fn shutdown_on_claim(outbox: &LedgerOutbox) -> watch::Receiver<bool> {
    let (sender, receiver) = watch::channel(false);
    *outbox.on_claim.lock().expect("lock") = Some(sender);
    receiver
}
async fn turn_of(worker: &DeliveryWorker) -> Result<WorkerTurn, AppError> {
    let (_, mut shutdown) = watch::channel(false);
    worker.run_once(&mut shutdown).await
}

#[test]
fn backoff_is_indexed_by_attempt_and_clamps_both_ends() {
    let secs = |attempt| backoff(attempt).as_secs();
    // Attempt 0 and negative attempts cannot index the table and clamp to the last step.
    assert_eq!(secs(i64::MIN), 3600);
    assert_eq!(secs(-1), 3600);
    assert_eq!(secs(0), 3600);
    let schedule: Vec<u64> = (1..=9).map(secs).collect();
    assert_eq!(schedule, vec![1, 5, 30, 120, 600, 1800, 3600, 3600, 3600]);
    assert_eq!(secs(i64::MAX), 3600);
}

#[tokio::test]
async fn shutdown_before_claim_returns_without_touching_the_outbox() {
    let (worker, _, ledger) = Setup::new(record(), accepted("1", rate())).build();
    let (_sender, mut shutdown) = watch::channel(true);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Ok(WorkerTurn::Shutdown)
    );
    assert_eq!(calls(&ledger), Vec::<Call>::new());
}

#[tokio::test]
async fn empty_queue_is_idle_after_one_claim_with_the_worker_id() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.record = None;
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Idle));
    assert_eq!(calls(&ledger), vec![claim()]);
}

#[tokio::test]
async fn claim_error_is_returned_without_a_transition() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.claim_error = true;
    let (worker, _, ledger) = setup.build();
    assert_eq!(
        turn_of(&worker).await,
        Err(AppError::Unavailable("claim failed".into()))
    );
    assert_eq!(calls(&ledger), vec![claim()]);
}

#[tokio::test]
async fn shutdown_after_claim_retries_by_fixed_delay_or_exhausts_without_risk() {
    // The release delay is a fixed second, not the attempt backoff.
    let (worker, outbox, ledger) = Setup::new(with_attempts(4), accepted("1", rate())).build();
    let mut shutdown = shutdown_on_claim(&outbox);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Ok(WorkerTurn::Shutdown)
    );
    assert_eq!(
        calls(&ledger),
        vec![claim(), retry(11_000, "network", false)]
    );

    let (worker, outbox, ledger) =
        Setup::new(with_attempts(MAX_ATTEMPTS), accepted("1", rate())).build();
    let mut shutdown = shutdown_on_claim(&outbox);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Ok(WorkerTurn::Shutdown)
    );
    assert_eq!(
        calls(&ledger),
        vec![claim(), dead("attempts_exhausted", false)]
    );
}

#[tokio::test]
async fn shutdown_release_lost_lease_is_surfaced() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.guarded = false;
    let (worker, outbox, _) = setup.build();
    let mut shutdown = shutdown_on_claim(&outbox);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Err(AppError::Conflict("outbox guarded retry lost lease".into()))
    );
}

#[tokio::test]
async fn shutdown_after_claiming_a_discussion_row_uses_the_plain_outbox_retry() {
    let row = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let discussions = fake_discussions(&row, Some("123456789012345678"));
    let (worker, outbox, ledger) = Setup::new(
        DeliveryRecord {
            attempts: MAX_ATTEMPTS,
            ..row
        },
        accepted("1", rate()),
    )
    .build();
    let provider = Arc::new(FakeDiscussionProvider {
        result: Mutex::new(DiscussionResult::Terminal {
            reason: TerminalReason::ClientError,
        }),
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let worker = worker.with_discussion_adapters(discussions.clone(), provider.clone());
    let mut shutdown = shutdown_on_claim(&outbox);
    assert_eq!(
        worker.run_once(&mut shutdown).await,
        Ok(WorkerTurn::Shutdown)
    );
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            Call::Dead {
                row: "discussion-row".into(),
                worker: "w1".into(),
                update: DeadLetterTransition {
                    classification: "attempts_exhausted".into(),
                    error: "attempts_exhausted".into(),
                    duplicate_risk: false,
                },
            }
        ]
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(discussions.terminals.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn unknown_delivery_kind_dead_letters_before_any_lookup() {
    let row = DeliveryRecord {
        delivery_kind: "carrier_pigeon".into(),
        ..record()
    };
    let (worker, _, ledger) = Setup::new(row, accepted("1", rate())).build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![claim(), dead("validation_failed", false)]
    );
}

#[tokio::test]
async fn event_validation_answers_in_order_before_any_lookup() {
    let tampered = {
        let mut row = record();
        let mut value: serde_json::Value =
            serde_json::from_slice(&row.payload).expect("envelope JSON");
        value["tenant"] = serde_json::Value::String("other".into());
        row.payload = serde_json::to_vec(&value).expect("tampered JSON");
        row.payload_sha256 = hex::encode(sha2::Sha256::digest(&row.payload));
        row
    };
    let cases = [
        // Hash mismatch answers before the event name, decode and secret checks.
        (
            DeliveryRecord {
                payload_sha256: "bad".into(),
                event_type: "no_such_event".into(),
                secret_ref: "bogus".into(),
                ..record()
            },
            "payload_hash_mismatch",
        ),
        // Unknown event name answers before the secret reference.
        (
            DeliveryRecord {
                event_type: "no_such_event".into(),
                secret_ref: "bogus".into(),
                ..record()
            },
            "validation_failed",
        ),
        // Canonical decode answers before the secret reference.
        (
            DeliveryRecord {
                secret_ref: "bogus".into(),
                ..tampered
            },
            "validation_failed",
        ),
        (
            DeliveryRecord {
                secret_ref: "hook".into(),
                ..record()
            },
            "invalid_secret",
        ),
        (
            DeliveryRecord {
                secret_ref: "webhook:".into(),
                ..record()
            },
            "invalid_secret",
        ),
    ];
    for (row, class) in cases {
        let (worker, _, ledger) = Setup::new(row, accepted("1", rate())).build();
        assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed), "{class}");
        assert_eq!(calls(&ledger), vec![claim(), dead(class, false)], "{class}");
    }
}

#[tokio::test]
async fn webhook_binding_failures_dead_letter_after_lookup_without_status() {
    let mut other_id = webhook();
    other_id.id = WebhookId("other".into());
    let cases = [
        (Err(WebhookResolutionFailure::InvalidReference), record()),
        (Ok(other_id), record()),
        (
            Ok(webhook()),
            DeliveryRecord {
                target_key: "other".into(),
                ..record()
            },
        ),
    ];
    for (response, row) in cases {
        let mut setup = Setup::new(row, accepted("1", rate()));
        setup.webhook = response;
        let (worker, _, ledger) = setup.build();
        assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
        assert_eq!(
            calls(&ledger),
            vec![claim(), lookup(), dead("invalid_webhook", false)]
        );
    }
}

#[tokio::test]
async fn webhook_lookup_retry_backs_off_by_attempt_with_duplicate_risk() {
    let mut setup = Setup::new(with_attempts(3), accepted("1", rate()));
    setup.webhook = Err(WebhookResolutionFailure::Retryable);
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![claim(), lookup(), retry(40_000, "network", true)]
    );

    let mut setup = Setup::new(with_attempts(MAX_ATTEMPTS), accepted("1", rate()));
    setup.webhook = Err(WebhookResolutionFailure::Retryable);
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![claim(), lookup(), dead("attempts_exhausted", true)]
    );
}

#[tokio::test]
async fn provider_request_rejection_dead_letters_without_send_or_status() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.webhook = Ok(WebhookDelivery {
        url: "https://evil.test/api/webhooks/1/token".into(),
        ..webhook()
    });
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![claim(), lookup(), dead("allowlist_rejected", false)]
    );

    // A reference that passes the worker's prefix check but not the transport's
    // character rules is rejected by `provider_request`, after the lookup.
    let row = DeliveryRecord {
        secret_ref: "webhook:bad ref".into(),
        target_key: "bad ref".into(),
        ..record()
    };
    let mut setup = Setup::new(row, accepted("1", rate()));
    setup.webhook = Ok(WebhookDelivery {
        id: WebhookId("bad ref".into()),
        ..webhook()
    });
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            Call::Lookup {
                id: "bad ref".into(),
                org: "acme".into(),
            },
            dead("invalid_secret", false)
        ]
    );
}

#[tokio::test]
async fn accepted_send_records_rate_then_acceptance_then_advisory_status() {
    let (worker, _, ledger) = Setup::new(record(), accepted("123", rate())).build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            lookup(),
            json_send(),
            bucket_rate(10_000),
            accepted_call("123"),
            Call::WebhookResult(Ok(())),
        ]
    );
}

#[tokio::test]
async fn terminal_send_dead_letters_then_records_advisory_status_for_every_reason() {
    let reasons = [
        (TerminalReason::InvalidSecret, "invalid_secret"),
        (TerminalReason::DecryptFailed, "decrypt_failed"),
        (TerminalReason::AllowlistRejected, "allowlist_rejected"),
        (TerminalReason::Redirect, "redirect"),
        (TerminalReason::BadRequest, "bad_request"),
        (TerminalReason::Unauthorized, "unauthorized"),
        (TerminalReason::Forbidden, "forbidden"),
        (TerminalReason::NotFound, "not_found"),
        (
            TerminalReason::InvalidRateLimitDelay,
            "invalid_rate_limit_delay",
        ),
        (TerminalReason::ClientError, "client_error"),
        (TerminalReason::ServerError, "server_error"),
    ];
    for (reason, class) in reasons {
        let (worker, _, ledger) =
            Setup::new(record(), DeliveryClassification::Terminal { reason }).build();
        assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed), "{class}");
        assert_eq!(
            calls(&ledger),
            vec![
                claim(),
                lookup(),
                json_send(),
                dead(class, false),
                Call::WebhookResult(Err(class.into())),
            ],
            "{class}"
        );
    }
}

#[tokio::test]
async fn non_rate_retry_uses_attempt_backoff_reason_code_and_risk_without_rate_state() {
    // Rate metadata on a non-429 retry is not persisted, even with a retry-after.
    let metadata = RateLimitMetadata {
        retry_after_ms: Some(250),
        ..rate()
    };
    let cases = [
        (
            RetryReason::Network,
            DuplicateRisk::None,
            1,
            11_000,
            "network",
            false,
        ),
        (
            RetryReason::Timeout,
            DuplicateRisk::Possible,
            2,
            15_000,
            "timeout",
            true,
        ),
        (
            RetryReason::Ambiguous,
            DuplicateRisk::Possible,
            3,
            40_000,
            "ambiguous",
            true,
        ),
        (
            RetryReason::ServerError,
            DuplicateRisk::None,
            7,
            3_610_000,
            "server_error",
            false,
        ),
    ];
    for (reason, risk, attempts, next, class, flagged) in cases {
        let (worker, _, ledger) = Setup::new(
            with_attempts(attempts),
            retry_outcome(reason, risk, Some(metadata.clone())),
        )
        .build();
        assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed), "{class}");
        assert_eq!(
            calls(&ledger),
            vec![claim(), lookup(), json_send(), retry(next, class, flagged)],
            "{class}"
        );
    }
}

#[tokio::test]
async fn exhausted_retry_dead_letters_with_the_send_duplicate_risk_and_no_status() {
    let (worker, _, ledger) = Setup::new(
        with_attempts(MAX_ATTEMPTS),
        retry_outcome(RetryReason::Timeout, DuplicateRisk::Possible, None),
    )
    .build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            lookup(),
            json_send(),
            dead("attempts_exhausted", true)
        ]
    );

    // A 429 at the attempt budget still persists the block before exhausting.
    let metadata = RateLimitMetadata {
        retry_after_ms: Some(250),
        ..rate()
    };
    let (worker, _, ledger) = Setup::new(
        with_attempts(MAX_ATTEMPTS),
        retry_outcome(
            RetryReason::RateLimited,
            DuplicateRisk::None,
            Some(metadata),
        ),
    )
    .build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            lookup(),
            json_send(),
            bucket_rate(10_250),
            dead("attempts_exhausted", false)
        ]
    );
}

#[tokio::test]
async fn rate_limited_retry_without_retry_after_is_an_error_without_transition() {
    for rate_limit in [None, Some(rate())] {
        let (worker, _, ledger) = Setup::new(
            record(),
            retry_outcome(RetryReason::RateLimited, DuplicateRisk::None, rate_limit),
        )
        .build();
        assert_eq!(
            turn_of(&worker).await,
            Err(AppError::Validation(
                "rate_limited outcome missing retry_after".into()
            ))
        );
        assert_eq!(calls(&ledger), vec![claim(), lookup(), json_send()]);
    }
}

#[tokio::test]
async fn rate_state_scope_target_bucket_and_delay_follow_the_outcome() {
    let row = DeliveryRecord {
        bucket_id: "row-bucket".into(),
        ..record()
    };
    let cases = [
        // User scope blocks only this target and has no shareable bucket.
        (
            accepted(
                "1",
                RateLimitMetadata {
                    scope: Some(RateLimitScope::User),
                    remaining: Some(3),
                    ..rate()
                },
            ),
            Some(("target", "hook", "", "", 10_000)),
        ),
        // A shared bucket without a provider bucket falls back to the row's bucket.
        (
            accepted(
                "1",
                RateLimitMetadata {
                    scope: Some(RateLimitScope::Shared),
                    bucket: None,
                    ..rate()
                },
            ),
            Some(("bucket", "hook", "row-bucket", "webhook:hook", 10_000)),
        ),
        // An exhausted bucket blocks until its reset.
        (
            accepted(
                "1",
                RateLimitMetadata {
                    remaining: Some(0),
                    reset_after_ms: Some(700),
                    ..rate()
                },
            ),
            Some(("bucket", "hook", "bucket", "webhook:hook", 10_700)),
        ),
        // An exhausted bucket without a reset persists nothing.
        (
            accepted(
                "1",
                RateLimitMetadata {
                    remaining: Some(0),
                    reset_after_ms: None,
                    ..rate()
                },
            ),
            None,
        ),
        // A terminal outcome carries no rate state.
        (
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
            None,
        ),
    ];
    for (outcome, expected) in cases {
        let (worker, _, ledger) = Setup::new(row.clone(), outcome.clone()).build();
        assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
        let rates: Vec<Call> = calls(&ledger)
            .into_iter()
            .filter(|call| matches!(call, Call::Rate { .. }))
            .collect();
        let expected: Vec<Call> = expected
            .into_iter()
            .map(|(scope, target, bucket, secret, until)| Call::Rate {
                scope: scope.into(),
                target: target.into(),
                bucket: bucket.into(),
                secret: secret.into(),
                until,
            })
            .collect();
        assert_eq!(rates, expected, "{outcome:?}");
    }
}

#[tokio::test]
async fn huge_retry_after_saturates_the_block_and_the_next_attempt() {
    let metadata = RateLimitMetadata {
        retry_after_ms: Some(u64::MAX),
        ..rate()
    };
    let (worker, _, ledger) = Setup::new(
        record(),
        retry_outcome(
            RetryReason::RateLimited,
            DuplicateRisk::None,
            Some(metadata),
        ),
    )
    .build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            lookup(),
            json_send(),
            bucket_rate(i64::MAX),
            retry(i64::MAX, "rate_limited", false)
        ]
    );
}

#[tokio::test]
async fn rate_persist_failure_stops_before_any_transition_or_status() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.rate_error = true;
    let (worker, _, ledger) = setup.build();
    assert_eq!(
        turn_of(&worker).await,
        Err(AppError::Unavailable("rate store failed".into()))
    );
    assert_eq!(
        calls(&ledger),
        vec![claim(), lookup(), json_send(), bucket_rate(10_000)]
    );
}

#[tokio::test]
async fn lost_lease_names_the_transition_and_skips_the_advisory_status() {
    let cases = [
        (accepted("1", rate()), "outbox guarded accepted lost lease"),
        (
            DeliveryClassification::Terminal {
                reason: TerminalReason::BadRequest,
            },
            "outbox guarded dead-letter lost lease",
        ),
        (
            retry_outcome(RetryReason::Network, DuplicateRisk::None, None),
            "outbox guarded retry lost lease",
        ),
    ];
    for (outcome, message) in cases {
        let mut setup = Setup::new(record(), outcome);
        setup.guarded = false;
        let (worker, _, ledger) = setup.build();
        assert_eq!(
            turn_of(&worker).await,
            Err(AppError::Conflict(message.into()))
        );
        assert!(
            !calls(&ledger)
                .iter()
                .any(|call| matches!(call, Call::WebhookResult(_))),
            "{message}"
        );
    }
}

#[tokio::test]
async fn advisory_status_failure_does_not_fail_the_turn() {
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.result_error = true;
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(calls(&ledger).last(), Some(&Call::WebhookResult(Ok(()))));
}

#[tokio::test]
async fn preview_is_resolved_after_lookup_and_attached_as_multipart() {
    let mut setup = Setup::new(record_with_preview(), accepted("1", rate()));
    setup.preview = Some(Some(vec![0x89, b'P', b'N', b'G']));
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    let seen = calls(&ledger);
    assert_eq!(&seen[..3], &[claim(), lookup(), Call::Preview]);
    let Call::Provider(request) = &seen[3] else {
        panic!("expected a provider send, got {seen:?}");
    };
    assert!(
        request.contains("content_type: \"multipart/form-data"),
        "{request}"
    );
    assert_eq!(
        &seen[4..],
        &[
            bucket_rate(10_000),
            accepted_call("1"),
            Call::WebhookResult(Ok(()))
        ]
    );

    // An envelope without a preview reference never consults the resolver.
    let mut setup = Setup::new(record(), accepted("1", rate()));
    setup.preview = Some(Some(vec![0x89, b'P', b'N', b'G']));
    let (worker, _, ledger) = setup.build();
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(calls(&ledger)[..3], [claim(), lookup(), json_send()]);
}

struct FailingCredentials;
impl OrganizationDiscordCredentialService for FailingCredentials {
    fn credential_for_provider<'a>(
        &'a self,
        _: &'a OrgId,
    ) -> BoxFuture<'a, Result<Option<crate::config::Secret>, AppError>> {
        Box::pin(async { Err(AppError::Unavailable("credential store down".into())) })
    }
}

#[tokio::test]
async fn organization_credential_failure_is_a_safe_network_retry() {
    let row = discussion_record(
        DiscordDiscussionOperationV1::reply("feedback-a".into(), "Body".into()).expect("reply"),
        DELIVERY_KIND_DISCUSSION_MESSAGE,
    );
    let discussions = fake_discussions(&row, Some("123456789012345678"));
    let (worker, _, ledger) =
        Setup::new(DeliveryRecord { attempts: 2, ..row }, accepted("1", rate())).build();
    let worker = worker.with_discussion_adapters(
        discussions.clone(),
        Arc::new(OrganizationDiscordDiscussionProvider::new(Arc::new(
            FailingCredentials,
        ))),
    );
    assert_eq!(turn_of(&worker).await, Ok(WorkerTurn::Processed));
    assert_eq!(
        calls(&ledger),
        vec![
            claim(),
            Call::Retry {
                row: "discussion-row".into(),
                worker: "w1".into(),
                update: RetryTransition {
                    next_attempt_at: 15_000,
                    classification: "network".into(),
                    duplicate_risk: false,
                },
            }
        ]
    );
    assert!(discussions.posts.lock().expect("lock").is_empty());
}
