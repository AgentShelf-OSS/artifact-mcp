//! ADR-0012 reminder service, sweeper, and Web Push sender.
//!
//! [`PushService`] is the single boundary the viewer routes and the MCP tool use. It exists only
//! when the feature is enabled (see [`crate::config::AppConfig::web_push_enabled`]).
//! [`PushRuntime`] owns two tasks started next to the Discord delivery runtime: a sweeper (every
//! 5 seconds, plus a wake after a near-due write) and a sender (every 2 seconds, plus a wake after
//! the sweeper queues deliveries). Delivery is at least once.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt as _;
use tokio::{
    sync::{Notify, watch},
    task::JoinSet,
};

use crate::{
    config::{AppConfig, Clock, OsRandom, RandomSource, SystemClock, format_sqlite_datetime},
    error::AppError,
    integrations::web_push::{self, VapidSigner},
    mcp::protocol::OrderedJson,
    persistence::{
        db::{self, DbPool},
        push::{
            self, DueDelivery, NewSubscription, PushError, ReminderRecord, ReminderScope,
            ReminderWrite, RetryOutcome, SweepReport,
        },
    },
    ports::BoxFuture,
    security::crypto::WebhookCipher,
};

/// Sweeper polling interval.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Sender polling interval.
pub const SEND_INTERVAL: Duration = Duration::from_secs(2);
/// A reminder write due within this window wakes the sweeper immediately.
pub const NEAR_DUE_MS: i64 = 10_000;
/// Per-request push-service timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// `TTL` header and delivery lifetime, in seconds.
pub const PUSH_TTL_SECONDS: u32 = 1800;
/// Retry backoff after the 1st, 2nd, 3rd, and later failures.
pub const RETRY_BACKOFF_MS: [i64; 4] = [5_000, 30_000, 120_000, 600_000];
/// Upper bound for an honored `Retry-After`.
pub const MAX_RETRY_AFTER_MS: i64 = 600_000;
/// JWTs are reused for each audience for at most this long.
pub const JWT_CACHE_MS: i64 = 60 * 60 * 1000;
/// Deliveries claimed per sender turn.
pub const SEND_BATCH: i64 = 50;
/// Concurrent push-service requests per sender turn.
pub const SEND_CONCURRENCY: usize = 8;
/// Finished deliveries are pruned at most this often.
pub const PRUNE_INTERVAL_MS: i64 = 10 * 60 * 1000;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(11);

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    reminders_fired: AtomicU64,
    reminders_stale: AtomicU64,
    deliveries_accepted: AtomicU64,
    deliveries_retried: AtomicU64,
    deliveries_dead: AtomicU64,
    subscriptions_removed: AtomicU64,
    sweeper_errors: AtomicU64,
}

/// Aggregate, label-free Web Push counters. No endpoints, emails, or artifact ids.
#[derive(Clone, Default)]
pub struct PushTelemetry {
    counters: Arc<Counters>,
}

/// A point-in-time copy of [`PushTelemetry`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushCounts {
    pub reminders_fired: u64,
    pub reminders_stale: u64,
    pub deliveries_accepted: u64,
    pub deliveries_retried: u64,
    pub deliveries_dead: u64,
    pub subscriptions_removed: u64,
}

impl PushTelemetry {
    fn add(counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    /// Current values.
    #[must_use]
    pub fn snapshot(&self) -> PushCounts {
        let c = &self.counters;
        PushCounts {
            reminders_fired: c.reminders_fired.load(Ordering::Relaxed),
            reminders_stale: c.reminders_stale.load(Ordering::Relaxed),
            deliveries_accepted: c.deliveries_accepted.load(Ordering::Relaxed),
            deliveries_retried: c.deliveries_retried.load(Ordering::Relaxed),
            deliveries_dead: c.deliveries_dead.load(Ordering::Relaxed),
            subscriptions_removed: c.subscriptions_removed.load(Ordering::Relaxed),
        }
    }

    /// Prometheus exposition in the existing `/metrics` style.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        let s = self.snapshot();
        let errors = self.counters.sweeper_errors.load(Ordering::Relaxed);
        format!(
            "# HELP artifact_mcp_push_reminders_fired_total Reminders marked fired, by delivery decision.\n\
             # TYPE artifact_mcp_push_reminders_fired_total counter\n\
             artifact_mcp_push_reminders_fired_total{{result=\"queued\"}} {}\n\
             artifact_mcp_push_reminders_fired_total{{result=\"stale\"}} {}\n\
             # HELP artifact_mcp_push_deliveries_total Web Push delivery outcomes.\n\
             # TYPE artifact_mcp_push_deliveries_total counter\n\
             artifact_mcp_push_deliveries_total{{outcome=\"accepted\"}} {}\n\
             artifact_mcp_push_deliveries_total{{outcome=\"retried\"}} {}\n\
             artifact_mcp_push_deliveries_total{{outcome=\"dead\"}} {}\n\
             # HELP artifact_mcp_push_subscriptions_removed_total Subscriptions removed after a 404 or 410 from the push service.\n\
             # TYPE artifact_mcp_push_subscriptions_removed_total counter\n\
             artifact_mcp_push_subscriptions_removed_total {}\n\
             # HELP artifact_mcp_push_worker_errors_total Sweeper or sender turns that failed before persisting an outcome.\n\
             # TYPE artifact_mcp_push_worker_errors_total counter\n\
             artifact_mcp_push_worker_errors_total {}\n",
            s.reminders_fired,
            s.reminders_stale,
            s.deliveries_accepted,
            s.deliveries_retried,
            s.deliveries_dead,
            s.subscriptions_removed,
            errors,
        )
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// One push-service request. The URL is the decrypted subscription endpoint.
pub struct PushRequest {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for PushRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PushRequest")
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// A classified push-service response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushResponse {
    Status {
        status: u16,
        retry_after_ms: Option<i64>,
    },
    NetworkError,
}

/// The HTTP boundary, replaceable in tests.
pub trait PushTransport: Send + Sync {
    fn send(&self, request: PushRequest) -> BoxFuture<'_, PushResponse>;
}

/// `reqwest` + rustls, no redirects, 10-second timeout.
pub struct HttpPushTransport {
    client: reqwest::Client,
    origin_override: Option<url::Url>,
}

impl HttpPushTransport {
    /// Production transport.
    ///
    /// # Errors
    /// [`AppError::Unavailable`] when the HTTP client cannot be built.
    pub fn new() -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(SEND_TIMEOUT)
            .build()
            .map_err(|_| AppError::Unavailable("push transport unavailable".to_owned()))?;
        Ok(Self {
            client,
            origin_override: None,
        })
    }

    /// Test seam: send every request to `origin` (scheme, host, and port) while keeping the
    /// endpoint path and query. Allowlist checks still run against the real endpoint. Not
    /// reachable from configuration.
    ///
    /// # Errors
    /// As [`Self::new`], or [`AppError::Validation`] for an unparsable origin.
    #[doc(hidden)]
    pub fn with_origin_override(origin: &str) -> Result<Self, AppError> {
        let origin = url::Url::parse(origin)
            .map_err(|_| AppError::Validation("bad test origin".to_owned()))?;
        Ok(Self {
            origin_override: Some(origin),
            ..Self::new()?
        })
    }

    fn target(&self, endpoint: &str) -> Option<url::Url> {
        let mut url = url::Url::parse(endpoint).ok()?;
        if let Some(origin) = &self.origin_override {
            let mut rewritten = origin.clone();
            rewritten.set_path(url.path());
            rewritten.set_query(url.query());
            url = rewritten;
        }
        Some(url)
    }
}

impl PushTransport for HttpPushTransport {
    fn send(&self, request: PushRequest) -> BoxFuture<'_, PushResponse> {
        Box::pin(async move {
            let Some(target) = self.target(&request.url) else {
                return PushResponse::NetworkError;
            };
            let mut builder = self.client.post(target);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            match builder.body(request.body).send().await {
                Ok(response) => {
                    let retry_after_ms = response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| parse_retry_after_ms(value, now_unix_ms()));
                    PushResponse::Status {
                        status: response.status().as_u16(),
                        retry_after_ms,
                    }
                }
                Err(_) => PushResponse::NetworkError,
            }
        })
    }
}

fn now_unix_ms() -> i64 {
    SystemClock.now_unix_millis()
}

/// `Retry-After` as delta-seconds or an HTTP date, in milliseconds from `now_ms`.
#[must_use]
pub fn parse_retry_after_ms(value: &str, now_ms: i64) -> Option<i64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        return value
            .parse::<i64>()
            .ok()
            .map(|seconds| seconds.saturating_mul(1000));
    }
    let date =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822).ok()?;
    let at = i64::try_from(date.unix_timestamp_nanos() / 1_000_000).ok()?;
    Some(at.saturating_sub(now_ms).max(0))
}

/// Delay before the next attempt, given the attempt count after this failure.
#[must_use]
pub fn retry_delay_ms(attempts_after_failure: i64, retry_after_ms: Option<i64>) -> i64 {
    if let Some(delay) = retry_after_ms {
        return delay.clamp(1_000, MAX_RETRY_AFTER_MS);
    }
    let index = usize::try_from(attempts_after_failure.saturating_sub(1).max(0))
        .unwrap_or(usize::MAX)
        .min(RETRY_BACKOFF_MS.len() - 1);
    RETRY_BACKOFF_MS[index]
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Validated reminder fields as accepted from a route body or MCP arguments.
#[derive(Clone, Copy, Debug)]
pub struct ReminderInput<'a> {
    pub fire_at: Option<&'a OrderedJson>,
    pub delay_seconds: Option<&'a OrderedJson>,
    pub title: Option<&'a OrderedJson>,
    pub body: Option<&'a OrderedJson>,
}

/// A stored reminder after a successful write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReminderSaved {
    pub fire_at: i64,
    pub revision: i64,
}

/// Reminder, opt-in, subscription, sweeper, and sender operations over the shared pool.
pub struct PushService {
    pool: DbPool,
    cipher: WebhookCipher,
    signer: VapidSigner,
    endpoint_hosts: Vec<String>,
    public_base_url: String,
    clock: Arc<dyn Clock>,
    random: Arc<dyn RandomSource>,
    sweeper_wake: Arc<Notify>,
    sender_wake: Arc<Notify>,
    telemetry: PushTelemetry,
    jwt_cache: Mutex<HashMap<String, (String, i64)>>,
    last_prune_ms: Mutex<Option<i64>>,
}

impl std::fmt::Debug for PushService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PushService")
            .field("public_key", &self.signer.public_key())
            .finish_non_exhaustive()
    }
}

impl PushService {
    /// Build the service when the feature is enabled; `Ok(None)` when it is not.
    ///
    /// # Errors
    /// [`AppError::Validation`] when the configured keys cannot be used.
    pub fn from_config(config: &AppConfig, pool: DbPool) -> Result<Option<Arc<Self>>, AppError> {
        Self::with_clock(config, pool, Arc::new(SystemClock))
    }

    /// [`Self::from_config`] with an injected clock (tests).
    ///
    /// # Errors
    /// As [`Self::from_config`].
    pub fn with_clock(
        config: &AppConfig,
        pool: DbPool,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Arc<Self>>, AppError> {
        if !config.web_push_enabled() {
            return Ok(None);
        }
        let (Some(vapid), Some(key)) = (&config.web_push.vapid, &config.webhook_enc_key) else {
            return Ok(None);
        };
        Ok(Some(Arc::new(Self {
            pool,
            cipher: WebhookCipher::new(key)?,
            signer: VapidSigner::new(vapid.private_key.expose(), &vapid.subject)?,
            endpoint_hosts: config.web_push.endpoint_hosts.clone(),
            public_base_url: config.public_base_url.clone(),
            clock,
            random: Arc::new(OsRandom),
            sweeper_wake: Arc::new(Notify::new()),
            sender_wake: Arc::new(Notify::new()),
            telemetry: PushTelemetry::default(),
            jwt_cache: Mutex::new(HashMap::new()),
            last_prune_ms: Mutex::new(None),
        })))
    }

    /// The VAPID application-server key the browser subscribes with.
    #[must_use]
    pub fn public_key(&self) -> &str {
        self.signer.public_key()
    }

    /// Aggregate counters for `/metrics`.
    #[must_use]
    pub fn telemetry(&self) -> &PushTelemetry {
        &self.telemetry
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_unix_millis()
    }

    fn now_text(&self) -> String {
        format_sqlite_datetime(self.now_ms().div_euclid(1000))
    }

    /// Wake the sweeper (lossless: a permit is stored when it is busy).
    pub fn wake_sweeper(&self) {
        self.sweeper_wake.notify_one();
    }

    fn uuid_v4(&self) -> Result<String, AppError> {
        let mut bytes = [0_u8; 16];
        self.random.fill_bytes(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = hex::encode(bytes);
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..32]
        ))
    }

    /// `PUT /push/subscriptions`.
    ///
    /// # Errors
    /// [`PushError::BadEndpoint`], [`PushError::BadKeys`], or [`PushError::Unavailable`].
    pub async fn save_subscription(
        &self,
        org: String,
        viewer_email: String,
        endpoint: String,
        p256dh: String,
        auth: String,
        label: String,
    ) -> Result<String, PushError> {
        if !push::valid_endpoint(&endpoint, &self.endpoint_hosts) {
            return Err(PushError::BadEndpoint);
        }
        if !push::valid_keys(&p256dh, &auth) {
            return Err(PushError::BadKeys);
        }
        let id = self.uuid_v4()?;
        let now_text = self.now_text();
        let ciphertext = push::pack_endpoint_ciphertext(&self.cipher.encrypt(&endpoint)?);
        Ok(db::interact(&self.pool, move |conn| {
            push::save_subscription(
                conn,
                &ciphertext,
                &NewSubscription {
                    org: &org,
                    viewer_email: &viewer_email,
                    endpoint: &endpoint,
                    p256dh: &p256dh,
                    auth: &auth,
                    label: &label,
                },
                id,
                &now_text,
            )
        })
        .await?)
    }

    /// Move this viewer's subscriptions to their current organization (best effort), so an org
    /// reassignment takes effect without re-registering a device.
    pub async fn refresh_viewer_org(&self, viewer_email: String, org: String) {
        let result = db::interact(&self.pool, move |conn| {
            push::refresh_subscription_org(conn, &viewer_email, &org)
        })
        .await;
        if result.is_err() {
            tracing::warn!("push subscription org refresh failed");
        }
    }

    /// `DELETE /push/subscriptions`.
    ///
    /// # Errors
    /// [`PushError::Unavailable`].
    pub async fn remove_subscription(
        &self,
        viewer_email: String,
        endpoint: String,
    ) -> Result<(), PushError> {
        db::interact(&self.pool, move |conn| {
            push::remove_subscription(conn, &viewer_email, &endpoint)
        })
        .await?;
        Ok(())
    }

    /// `GET /{id}/push`: `(opted_in, devices)`.
    ///
    /// # Errors
    /// [`PushError::Unavailable`].
    pub async fn status(
        &self,
        artifact_id: String,
        org: String,
        viewer_email: String,
    ) -> Result<(bool, i64), PushError> {
        Ok(db::interact(&self.pool, move |conn| {
            Ok((
                push::opted_in(conn, &artifact_id, &viewer_email)?,
                push::device_count(conn, &org, &viewer_email)?,
            ))
        })
        .await?)
    }

    /// `PUT /{id}/push/optin` (`opt_in = true`) and `DELETE` (`false`).
    ///
    /// # Errors
    /// [`PushError::Unavailable`].
    pub async fn set_opt_in(
        &self,
        artifact_id: String,
        org: String,
        viewer_email: String,
        opt_in: bool,
    ) -> Result<(), PushError> {
        let now_text = self.now_text();
        Ok(db::interact(&self.pool, move |conn| {
            if opt_in {
                push::opt_in(conn, &artifact_id, &org, &viewer_email, &now_text)
            } else {
                push::opt_out(conn, &artifact_id, &viewer_email)
            }
        })
        .await?)
    }

    /// `GET /{id}/reminders`.
    ///
    /// # Errors
    /// [`PushError::Unavailable`].
    pub async fn list_reminders(
        &self,
        artifact_id: String,
        scope: ReminderScope,
        owner: String,
    ) -> Result<Vec<ReminderRecord>, PushError> {
        Ok(db::interact(&self.pool, move |conn| {
            push::list_reminders(conn, &artifact_id, scope, &owner)
        })
        .await?)
    }

    /// Shared route/MCP write: validates key, time, and text, then upserts.
    ///
    /// # Errors
    /// The shared [`PushError`] codes.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_reminder(
        &self,
        artifact_id: String,
        org: String,
        scope: ReminderScope,
        owner: String,
        key: String,
        input: ReminderInput<'_>,
        created_by: String,
    ) -> Result<ReminderSaved, PushError> {
        if !push::valid_key(&key) {
            return Err(PushError::BadKey);
        }
        if scope == ReminderScope::Viewer && owner.is_empty() {
            return Err(PushError::BadScope);
        }
        let now = self.now_ms();
        let fire_at = push::resolve_fire_at(input.fire_at, input.delay_seconds, now)?;
        let text = push::validate_text(input.title, input.body)?;
        let now_text = self.now_text();
        let pool = self.pool.clone();
        let revision = tokio::task::spawn_blocking(move || {
            let mut conn = db::checkout(&pool)?;
            push::set_reminder(
                &mut conn,
                &ReminderWrite {
                    artifact_id: &artifact_id,
                    org: &org,
                    scope,
                    owner: &owner,
                    key: &key,
                    fire_at,
                    text: &text,
                    created_by: &created_by,
                },
                &now_text,
            )
        })
        .await
        .map_err(|_| PushError::Unavailable)??;
        if fire_at.saturating_sub(self.now_ms()) <= NEAR_DUE_MS {
            self.wake_sweeper();
        }
        Ok(ReminderSaved { fire_at, revision })
    }

    /// Delete one reminder key (absent keys succeed).
    ///
    /// # Errors
    /// [`PushError::Unavailable`].
    pub async fn clear_reminder(
        &self,
        artifact_id: String,
        scope: ReminderScope,
        owner: String,
        key: String,
    ) -> Result<(), PushError> {
        db::interact(&self.pool, move |conn| {
            push::clear_reminder(conn, &artifact_id, scope, &owner, &key)
        })
        .await?;
        Ok(())
    }

    /// One sweeper transaction. Wakes the sender when deliveries were queued.
    ///
    /// # Errors
    /// [`AppError::Internal`] when the transaction fails; nothing is committed.
    pub async fn sweep_once(&self) -> Result<SweepReport, AppError> {
        let now = self.now_ms();
        let base = self.public_base_url.clone();
        let report =
            db::interact(&self.pool, move |conn| push::sweep_due(conn, now, &base)).await?;
        PushTelemetry::add(&self.telemetry.counters.reminders_fired, report.fired);
        PushTelemetry::add(&self.telemetry.counters.reminders_stale, report.stale);
        if report.deliveries > 0 {
            self.sender_wake.notify_one();
        }
        if report.fired + report.stale > 0 {
            tracing::info!(
                fired = report.fired,
                stale = report.stale,
                deliveries = report.deliveries,
                "push reminders fired"
            );
        }
        Ok(report)
    }

    fn jwt_for(&self, audience: &str, now_ms: i64) -> String {
        let mut cache = self
            .jwt_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((jwt, created)) = cache.get(audience)
            && now_ms.saturating_sub(*created) < JWT_CACHE_MS
        {
            return jwt.clone();
        }
        let jwt = self.signer.jwt(audience, now_ms.div_euclid(1000));
        cache.insert(audience.to_owned(), (jwt.clone(), now_ms));
        jwt
    }

    fn build_request(&self, delivery: &DueDelivery, now_ms: i64) -> Option<PushRequest> {
        let endpoint = push::unpack_endpoint_ciphertext(&delivery.endpoint_ciphertext)
            .and_then(|record| self.cipher.decrypt(&record).ok())?;
        // Re-checked before every send: an operator can narrow the allowlist after a save.
        if !push::valid_endpoint(&endpoint, &self.endpoint_hosts) {
            return None;
        }
        let url = url::Url::parse(&endpoint).ok()?;
        let body = web_push::encrypt_payload(
            delivery.payload.as_bytes(),
            &delivery.p256dh,
            &delivery.auth,
            self.random.as_ref(),
        )
        .ok()?;
        let jwt = self.jwt_for(&web_push::audience(&url), now_ms);
        let tag = format!("{}:{}", delivery.artifact_id, delivery.reminder_key);
        Some(PushRequest {
            url: endpoint,
            headers: vec![
                ("ttl", PUSH_TTL_SECONDS.to_string()),
                ("urgency", "high".to_owned()),
                ("topic", web_push::topic(&tag)),
                ("content-encoding", "aes128gcm".to_owned()),
                ("content-type", "application/octet-stream".to_owned()),
                ("authorization", self.signer.authorization(&jwt)),
            ],
            body,
        })
    }

    /// One sender turn: expire overdue rows, prune old finished rows (every 10 minutes), lease
    /// every due delivery, and attempt each once with bounded concurrency. Returns the number of
    /// attempted deliveries.
    ///
    /// # Errors
    /// [`AppError::Internal`] when the queue cannot be read or an outcome cannot be stored.
    pub async fn send_due(&self, transport: &dyn PushTransport) -> Result<usize, AppError> {
        let now = self.now_ms();
        let prune = {
            let mut last = self
                .last_prune_ms
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let due = last.is_none_or(|last| now.saturating_sub(last) >= PRUNE_INTERVAL_MS);
            if due {
                *last = Some(now);
            }
            due
        };
        let (expired, due) = db::interact(&self.pool, move |conn| {
            if prune {
                push::prune_finished(conn, now)?;
            }
            Ok((
                push::expire_overdue(conn, now)?,
                push::claim_due(conn, now, SEND_BATCH, push::DELIVERY_LEASE_MS)?,
            ))
        })
        .await?;
        PushTelemetry::add(&self.telemetry.counters.deliveries_dead, expired);
        let attempted = due.len();
        let outcomes: Vec<Result<(), AppError>> = futures_util::stream::iter(due)
            .map(|delivery| self.attempt(transport, delivery))
            .buffer_unordered(SEND_CONCURRENCY)
            .collect()
            .await;
        outcomes.into_iter().collect::<Result<Vec<()>, _>>()?;
        Ok(attempted)
    }

    async fn attempt(
        &self,
        transport: &dyn PushTransport,
        delivery: DueDelivery,
    ) -> Result<(), AppError> {
        let counters = &self.telemetry.counters;
        let now = self.now_ms();
        let Some(request) = self.build_request(&delivery, now) else {
            db::interact(&self.pool, move |conn| {
                push::mark_dead(conn, &delivery, None, now)
            })
            .await?;
            PushTelemetry::add(&counters.deliveries_dead, 1);
            return Ok(());
        };
        let response = transport.send(request).await;
        let now = self.now_ms();
        let (status, retry_after) = match response {
            PushResponse::Status {
                status,
                retry_after_ms,
            } => (Some(status), retry_after_ms),
            PushResponse::NetworkError => (None, None),
        };
        match status {
            Some(status @ 200..=202) => {
                db::interact(&self.pool, move |conn| {
                    push::mark_accepted(conn, &delivery, status, now)
                })
                .await?;
                PushTelemetry::add(&counters.deliveries_accepted, 1);
            }
            Some(404 | 410) => {
                let subscription = delivery.subscription_id.clone();
                db::interact(&self.pool, move |conn| {
                    push::remove_subscription_by_id(conn, &subscription)
                })
                .await?;
                PushTelemetry::add(&counters.subscriptions_removed, 1);
            }
            None | Some(429 | 500..=599) => {
                let next = now + retry_delay_ms(delivery.attempts + 1, retry_after);
                let outcome = db::interact(&self.pool, move |conn| {
                    push::mark_retry(conn, &delivery, status, next, now)
                })
                .await?;
                match outcome {
                    RetryOutcome::Retrying => PushTelemetry::add(&counters.deliveries_retried, 1),
                    RetryOutcome::Dead => PushTelemetry::add(&counters.deliveries_dead, 1),
                }
            }
            Some(other) => {
                db::interact(&self.pool, move |conn| {
                    push::mark_dead(conn, &delivery, Some(other), now)
                })
                .await?;
                PushTelemetry::add(&counters.deliveries_dead, 1);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Owns the sweeper and sender tasks; consumed during graceful shutdown.
pub struct PushRuntime {
    shutdown: watch::Sender<bool>,
    service: Arc<PushService>,
    tasks: JoinSet<()>,
}

impl PushRuntime {
    /// Start both tasks. Call only when the feature is enabled.
    #[must_use]
    pub fn start(service: Arc<PushService>, transport: Arc<dyn PushTransport>) -> Self {
        Self::start_with_intervals(service, transport, SWEEP_INTERVAL, SEND_INTERVAL)
    }

    /// [`Self::start`] with explicit polling intervals (tests).
    #[must_use]
    pub fn start_with_intervals(
        service: Arc<PushService>,
        transport: Arc<dyn PushTransport>,
        sweep_interval: Duration,
        send_interval: Duration,
    ) -> Self {
        let (shutdown, receive) = watch::channel(false);
        let mut tasks = JoinSet::new();
        {
            let service = Arc::clone(&service);
            let mut shutdown = receive.clone();
            tasks.spawn(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if service.sweep_once().await.is_err() {
                        service
                            .telemetry
                            .counters
                            .sweeper_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("push sweeper turn failed");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        () = tokio::time::sleep(sweep_interval) => {}
                        () = service.sweeper_wake.notified() => {}
                    }
                }
            });
        }
        {
            let service = Arc::clone(&service);
            let mut shutdown = receive;
            tasks.spawn(async move {
                loop {
                    if *shutdown.borrow() {
                        break;
                    }
                    if service.send_due(transport.as_ref()).await.is_err() {
                        service
                            .telemetry
                            .counters
                            .sweeper_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("push sender turn failed");
                    }
                    tokio::select! {
                        _ = shutdown.changed() => {}
                        () = tokio::time::sleep(send_interval) => {}
                        () = service.sender_wake.notified() => {}
                    }
                }
            });
        }
        Self {
            shutdown,
            service,
            tasks,
        }
    }

    /// Stop both loops; an in-flight send may finish within its 10-second timeout.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        self.service.sweeper_wake.notify_one();
        self.service.sender_wake.notify_one();
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while let Some(joined) = self.tasks.join_next().await {
                if let Err(error) = joined {
                    tracing::warn!(error = %error, "push task ended unexpectedly");
                }
            }
        })
        .await;
        if drained.is_err() {
            self.tasks.abort_all();
        }
    }
}
