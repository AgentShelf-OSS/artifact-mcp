//! Guarded production Discord Gateway runtime for PBI-080.
//!
//! Twilight owns heartbeat, reconnect, resume, compression, and intent protocol behavior. This
//! adapter owns only organization-scoped credential resolution, exact server-side routing, safe
//! normalization, bounded REST hydration, and optional-integration health. The runtime is off by
//! default and never affects HTTP/application liveness or the outbound durable outbox.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use reqwest::{StatusCode, header::AUTHORIZATION};
use sha2::{Digest as _, Sha256};
use tokio::{sync::watch, task::JoinHandle};
use twilight_gateway::{
    ConfigBuilder, Event, EventTypeFlags, Intents, Session, Shard, ShardId, StreamExt as _,
};
use twilight_model::{
    channel::{Message, message::MessageType},
    gateway::payload::incoming::Ready,
};

use crate::{
    config::Secret,
    error::AppError,
    integrations::discord_inbound::{
        DiscordAuthor, DiscordMessage, InboundEvent, InboundEventKind, InboundResult,
    },
    model::OrgId,
    persistence::discord_inbound::{
        DiscordInboundMetricsSnapshot, DiscordInboundStore, GatewayOrganizationTarget,
        GatewayResume,
    },
    ports::discussions::OrganizationDiscordCredentialService,
};

const DISCORD_API: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "artifact-mcp/1.6 (+https://github.com/nb-artifact/artifact-mcp)";
const SUPERVISOR_POLL: Duration = Duration::from_secs(2);
const REST_TIMEOUT: Duration = Duration::from_secs(4);
const INBOX_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const INBOX_RETENTION_DAYS: u16 = 30;
const INBOX_CLEANUP_BATCH: u16 = 500;
const PENDING_FETCH_BATCH: u16 = 25;
const PENDING_FETCH_POLL: Duration = Duration::from_secs(5);
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct DiscordInboundProcessMetrics {
    snapshot: DiscordInboundMetricsSnapshot,
    reconnects: u64,
    duplicates: u64,
    application_errors: u64,
}

static INBOUND_METRICS: LazyLock<Mutex<DiscordInboundProcessMetrics>> =
    LazyLock::new(|| Mutex::new(DiscordInboundProcessMetrics::default()));

pub fn render_prometheus() -> String {
    let metrics = INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let snapshot = &metrics.snapshot;
    format!(
        "# HELP artifact_mcp_discord_gateway_organizations Organization Gateway sessions by fixed health state.\n\
         # TYPE artifact_mcp_discord_gateway_organizations gauge\n\
         artifact_mcp_discord_gateway_organizations{{state=\"disabled\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"connecting\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"ready\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"reconnecting\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"degraded\"}} {}\n\
         artifact_mcp_discord_gateway_organizations{{state=\"failed\"}} {}\n\
         # HELP artifact_mcp_discord_gateway_reconnects_total Gateway reconnect signals observed by this process.\n\
         # TYPE artifact_mcp_discord_gateway_reconnects_total counter\n\
         artifact_mcp_discord_gateway_reconnects_total {}\n\
         # HELP artifact_mcp_discord_inbound_inbox_depth Retained body-free inbound event receipts.\n\
         # TYPE artifact_mcp_discord_inbound_inbox_depth gauge\n\
         artifact_mcp_discord_inbound_inbox_depth {}\n\
         # HELP artifact_mcp_discord_inbound_pending_fetches Partial updates awaiting REST hydration.\n\
         # TYPE artifact_mcp_discord_inbound_pending_fetches gauge\n\
         artifact_mcp_discord_inbound_pending_fetches {}\n\
         # HELP artifact_mcp_discord_inbound_last_event_age_seconds Age of the newest retained inbound receipt.\n\
         # TYPE artifact_mcp_discord_inbound_last_event_age_seconds gauge\n\
         artifact_mcp_discord_inbound_last_event_age_seconds {}\n\
         # HELP artifact_mcp_discord_inbound_oldest_pending_age_seconds Age of the oldest deferred update.\n\
         # TYPE artifact_mcp_discord_inbound_oldest_pending_age_seconds gauge\n\
         artifact_mcp_discord_inbound_oldest_pending_age_seconds {}\n\
         # HELP artifact_mcp_discord_inbound_ignored Retained ignored inbound events.\n\
         # TYPE artifact_mcp_discord_inbound_ignored gauge\n\
         artifact_mcp_discord_inbound_ignored {}\n\
         # HELP artifact_mcp_discord_inbound_rejected_or_degraded Retained rejected, degraded, or failed inbound events.\n\
         # TYPE artifact_mcp_discord_inbound_rejected_or_degraded gauge\n\
         artifact_mcp_discord_inbound_rejected_or_degraded {}\n\
         # HELP artifact_mcp_discord_inbound_tombstones Discord-origin feedback tombstones.\n\
         # TYPE artifact_mcp_discord_inbound_tombstones gauge\n\
         artifact_mcp_discord_inbound_tombstones {}\n\
         # HELP artifact_mcp_discord_inbound_duplicates_total Replay duplicates observed by this process.\n\
         # TYPE artifact_mcp_discord_inbound_duplicates_total counter\n\
         artifact_mcp_discord_inbound_duplicates_total {}\n\
         # HELP artifact_mcp_discord_inbound_application_errors_total Inbound events that failed before a durable result.\n\
         # TYPE artifact_mcp_discord_inbound_application_errors_total counter\n\
         artifact_mcp_discord_inbound_application_errors_total {}\n",
        snapshot.gateway_disabled,
        snapshot.gateway_connecting,
        snapshot.gateway_ready,
        snapshot.gateway_reconnecting,
        snapshot.gateway_degraded,
        snapshot.gateway_failed,
        metrics.reconnects,
        snapshot.inbox_depth,
        snapshot.pending_fetches,
        snapshot.last_event_age_seconds,
        snapshot.oldest_pending_age_seconds,
        snapshot.ignored_events,
        snapshot.rejected_or_degraded_events,
        snapshot.tombstones,
        metrics.duplicates,
        metrics.application_errors,
    )
}

fn refresh_metrics(snapshot: DiscordInboundMetricsSnapshot) {
    INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .snapshot = snapshot;
}

fn increment_metric(select: fn(&mut DiscordInboundProcessMetrics) -> &mut u64) {
    let mut metrics = INBOUND_METRICS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let value = select(&mut metrics);
    *value = value.saturating_add(1);
}

pub struct DiscordInboundRuntime {
    stop: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl DiscordInboundRuntime {
    #[must_use]
    pub fn start(
        enabled: bool,
        store: DiscordInboundStore,
        credentials: Arc<dyn OrganizationDiscordCredentialService>,
    ) -> Self {
        let (stop, receive) = watch::channel(false);
        let join = tokio::spawn(async move {
            if !enabled {
                if store.disable_gateway_integration().await.is_err() {
                    tracing::warn!("could not persist disabled Discord inbound state");
                }
                return;
            }
            supervise(store, credentials, receive).await;
        });
        Self { stop, join }
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.join.await;
    }
}

struct ActiveWorker {
    fingerprint: (i64, String, String),
    join: JoinHandle<()>,
}

async fn supervise(
    store: DiscordInboundStore,
    credentials: Arc<dyn OrganizationDiscordCredentialService>,
    mut stop: watch::Receiver<bool>,
) {
    let mut workers: BTreeMap<String, ActiveWorker> = BTreeMap::new();
    let mut ticker = tokio::time::interval(SUPERVISOR_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_cleanup = Instant::now()
        .checked_sub(INBOX_CLEANUP_INTERVAL)
        .unwrap_or_else(Instant::now);

    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                if let Ok(snapshot) = store.operational_metrics().await {
                    refresh_metrics(snapshot);
                }
                if last_cleanup.elapsed() >= INBOX_CLEANUP_INTERVAL {
                    if store
                        .cleanup_processed_events(INBOX_RETENTION_DAYS, INBOX_CLEANUP_BATCH)
                        .await
                        .is_err()
                    {
                        tracing::warn!("Discord inbound inbox cleanup failed");
                    }
                    last_cleanup = Instant::now();
                }
                let targets = match store.gateway_targets().await {
                    Ok(targets) => targets,
                    Err(_) => {
                        tracing::warn!("Discord inbound supervisor could not load targets");
                        continue;
                    }
                };
                let wanted: BTreeMap<String, GatewayOrganizationTarget> = targets
                    .into_iter()
                    .map(|target| (target.org.0.clone(), target))
                    .collect();

                let stale: Vec<String> = workers
                    .iter()
                    .filter(|(org, worker)| {
                        worker.join.is_finished() || wanted.get(*org).is_none_or(|target| {
                            worker.fingerprint
                                != (
                                    target.credential_version,
                                    target.guild_id.clone(),
                                    target.channel_id.clone(),
                                )
                        })
                    })
                    .map(|(org, _)| org.clone())
                    .collect();
                for org in stale {
                    if let Some(worker) = workers.remove(&org) {
                        let credential_version = worker.fingerprint.0;
                        worker.join.abort();
                        let _ = worker.join.await;
                        if !wanted.contains_key(&org) {
                            let _ = store
                                .set_gateway_health(
                                    OrgId::from(org),
                                    credential_version,
                                    "disabled",
                                    "",
                                    None,
                                )
                                .await;
                        }
                    }
                }

                for (org_name, target) in wanted {
                    if workers.contains_key(&org_name) {
                        continue;
                    }
                    let credential = match credentials.credential_for_provider(&target.org).await {
                        Ok(Some(credential)) => credential,
                        Ok(None) | Err(_) => {
                            let _ = store
                                .set_gateway_health(
                                    target.org,
                                    target.credential_version,
                                    "failed",
                                    "missing_credential",
                                    None,
                                )
                                .await;
                            continue;
                        }
                    };
                    let fingerprint = (
                        target.credential_version,
                        target.guild_id.clone(),
                        target.channel_id.clone(),
                    );
                    let worker_store = store.clone();
                    let worker_stop = stop.clone();
                    let join = tokio::spawn(async move {
                        run_organization(worker_store, target, credential, worker_stop).await;
                    });
                    workers.insert(org_name, ActiveWorker { fingerprint, join });
                }
            }
        }
    }

    for (_, worker) in workers {
        join_worker_with_grace(worker.join, WORKER_SHUTDOWN_GRACE).await;
    }
}

async fn join_worker_with_grace(mut join: JoinHandle<()>, grace: Duration) {
    if tokio::time::timeout(grace, &mut join).await.is_err() {
        join.abort();
        let _ = join.await;
    }
}

async fn run_organization(
    store: DiscordInboundStore,
    target: GatewayOrganizationTarget,
    credential: Secret,
    mut stop: watch::Receiver<bool>,
) {
    let rest = match DiscordInboundReadinessRest::new() {
        Ok(rest) => rest,
        Err(_) => {
            let _ = store
                .set_gateway_health(
                    target.org,
                    target.credential_version,
                    "failed",
                    "gateway_unavailable",
                    None,
                )
                .await;
            return;
        }
    };
    if let Err(error) = rest.validate_channel(&credential, &target.channel_id).await {
        let safe_error = readiness_error(&error);
        let _ = store
            .set_gateway_health(
                target.org,
                target.credential_version,
                "failed",
                safe_error,
                None,
            )
            .await;
        return;
    }

    let _ = store
        .set_gateway_health(
            target.org.clone(),
            target.credential_version,
            "connecting",
            "",
            None,
        )
        .await;
    let intents = configured_intents();
    let resume = store
        .gateway_resume(&target.org, target.credential_version)
        .await
        .ok()
        .flatten();
    let mut builder = ConfigBuilder::new(credential.expose().to_owned(), intents);
    if let Some(GatewayResume {
        session_id,
        resume_url,
        sequence,
    }) = resume
    {
        builder = builder
            .session(Session::new(sequence, session_id))
            .resume_url(resume_url);
    }
    let config = builder.build();
    let mut shard = Shard::with_config(ShardId::ONE, config);
    let mut pending_ticker = tokio::time::interval(PENDING_FETCH_POLL);
    pending_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let next = tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
                continue;
            }
            _ = pending_ticker.tick() => {
                drain_pending_updates(&store, &target, &credential, &rest).await;
                continue;
            }
            next = shard.next_event(EventTypeFlags::all()) => next,
        };
        let Some(next) = next else {
            break;
        };
        let event = match next {
            Ok(event) => event,
            Err(_) => {
                persist_shard_health(
                    &store,
                    &target,
                    shard_session(&shard),
                    "reconnecting",
                    "gateway_unavailable",
                )
                .await;
                continue;
            }
        };
        let session = shard_session(&shard);

        match event {
            Event::Ready(ready) => {
                if !ready_has_guild(&ready, &target.guild_id) {
                    persist_shard_health(
                        &store,
                        &target,
                        session.clone(),
                        "failed",
                        "guild_access",
                    )
                    .await;
                    break;
                }
                persist_shard_health(&store, &target, session, "ready", "").await;
            }
            Event::Resumed => {
                persist_shard_health(&store, &target, session, "ready", "").await;
            }
            Event::GatewayClose(frame) => {
                let safe_error = match frame.as_ref().map(|frame| frame.code) {
                    Some(4004) => "missing_credential",
                    Some(4013 | 4014) => "message_content_intent",
                    _ => "gateway_unavailable",
                };
                persist_shard_health(&store, &target, session, "failed", safe_error).await;
            }
            Event::GatewayReconnect | Event::GatewayInvalidateSession(_) => {
                increment_metric(|metrics| &mut metrics.reconnects);
                persist_shard_health(
                    &store,
                    &target,
                    session,
                    "reconnecting",
                    "gateway_unavailable",
                )
                .await;
            }
            provider_event => {
                if let Some((gateway_session_id, _, sequence)) = session.clone()
                    && let Some(inbound) =
                        normalize_event(&target.org, gateway_session_id, sequence, provider_event)
                {
                    if inbound.guild_id != target.guild_id {
                        continue;
                    }
                    match store.apply_event(inbound).await {
                        Ok(InboundResult::NeedsFetch) => {
                            drain_pending_updates(&store, &target, &credential, &rest).await;
                        }
                        Ok(InboundResult::Duplicate) => {
                            increment_metric(|metrics| &mut metrics.duplicates);
                        }
                        Ok(_) => {}
                        Err(_) => {
                            increment_metric(|metrics| &mut metrics.application_errors);
                            tracing::warn!(org = %target.org.0, "Discord inbound event application failed");
                        }
                    }
                    persist_shard_health(&store, &target, session, "ready", "").await;
                }
            }
        }
    }
}

async fn persist_shard_health(
    store: &DiscordInboundStore,
    target: &GatewayOrganizationTarget,
    session: Option<(String, String, u64)>,
    health: &'static str,
    safe_error: &'static str,
) {
    let _ = store
        .set_gateway_health(
            target.org.clone(),
            target.credential_version,
            health,
            safe_error,
            session,
        )
        .await;
}

fn shard_session(shard: &Shard) -> Option<(String, String, u64)> {
    shard.session().map(|session| {
        (
            session.id().to_owned(),
            shard.resume_url().unwrap_or_default().to_owned(),
            session.sequence(),
        )
    })
}

fn configured_intents() -> Intents {
    Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT
}

fn ready_has_guild(ready: &Ready, guild_id: &str) -> bool {
    ready
        .guilds
        .iter()
        .any(|guild| guild.id.to_string() == guild_id)
}

fn normalize_event(
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

async fn drain_pending_updates(
    store: &DiscordInboundStore,
    target: &GatewayOrganizationTarget,
    credential: &Secret,
    rest: &DiscordInboundReadinessRest,
) {
    let pending = match store
        .pending_updates(&target.org, PENDING_FETCH_BATCH)
        .await
    {
        Ok(pending) => pending,
        Err(_) => {
            tracing::warn!(org = %target.org.0, "Discord deferred update lookup failed");
            return;
        }
    };
    for mut event in pending {
        let authorized = store
            .authorization_for(&target.org, &event.guild_id, &event.thread_id)
            .await;
        if matches!(authorized, Ok(None)) {
            let _ = store.apply_event(event).await;
            continue;
        }
        if authorized.is_err() {
            continue;
        }
        let Some(message_id) = event.message.as_ref().map(|message| message.id.as_str()) else {
            continue;
        };
        match rest
            .fetch_message(credential, &event.thread_id, message_id)
            .await
        {
            MessageFetch::Found(message) => {
                event.message = Some(normalize_message(&message, message_version(&message)));
                if store.apply_event(event).await.is_err() {
                    tracing::warn!(org = %target.org.0, "Discord deferred update application failed");
                }
            }
            MessageFetch::Missing => {
                let _ = store.complete_missing_update(&event).await;
            }
            MessageFetch::Retryable {
                after_seconds,
                rate_limited,
            } => {
                if store
                    .defer_update_retry(&event, after_seconds, rate_limited)
                    .await
                    .unwrap_or(false)
                {
                    increment_metric(|metrics| &mut metrics.application_errors);
                }
            }
        }
    }
}

fn normalize_message(message: &Message, version: i64) -> DiscordMessage {
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

fn message_version(message: &Message) -> i64 {
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

pub struct DiscordInboundReadinessRest {
    client: reqwest::Client,
    api_base: String,
}

enum MessageFetch {
    Found(Box<Message>),
    Missing,
    Retryable {
        after_seconds: u16,
        rate_limited: bool,
    },
}

impl DiscordInboundReadinessRest {
    pub fn new() -> Result<Self, AppError> {
        Self::with_api_base(DISCORD_API)
    }

    fn with_api_base(api_base: &str) -> Result<Self, AppError> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map(|client| Self {
                client,
                api_base: api_base.trim_end_matches('/').to_owned(),
            })
            .map_err(|_| AppError::Unavailable("Discord inbound readiness unavailable.".to_owned()))
    }

    pub async fn validate_thread(
        &self,
        credential: &Secret,
        thread_id: &str,
    ) -> Result<(), AppError> {
        self.validate_channel(credential, thread_id).await?;
        let url = discord_resource_url(
            &self.api_base,
            &format!("channels/{thread_id}/messages?limit=1"),
        )?;
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
            .map_err(|_| {
                AppError::Unavailable("Discord inbound readiness unavailable.".to_owned())
            })?;
        classify_readiness_status(response.status())
    }

    async fn validate_channel(
        &self,
        credential: &Secret,
        channel_id: &str,
    ) -> Result<(), AppError> {
        let url = discord_resource_url(&self.api_base, &format!("channels/{channel_id}"))?;
        let response = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
            .map_err(|_| {
                AppError::Unavailable("Discord inbound readiness unavailable.".to_owned())
            })?;
        classify_readiness_status(response.status())
    }

    async fn fetch_message(
        &self,
        credential: &Secret,
        channel_id: &str,
        message_id: &str,
    ) -> MessageFetch {
        let Ok(url) = discord_resource_url(
            &self.api_base,
            &format!("channels/{channel_id}/messages/{message_id}"),
        ) else {
            return MessageFetch::Missing;
        };
        let Ok(response) = self
            .client
            .get(url)
            .header(AUTHORIZATION, format!("Bot {}", credential.expose()))
            .send()
            .await
        else {
            return MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            };
        };
        if response.status() == StatusCode::NOT_FOUND {
            return MessageFetch::Missing;
        }
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let after_seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| value.ceil().clamp(1.0, 60.0) as u16)
                .unwrap_or(5);
            return MessageFetch::Retryable {
                after_seconds,
                rate_limited: true,
            };
        }
        if !response.status().is_success() {
            return MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            };
        }
        response.json::<Message>().await.map(Box::new).map_or(
            MessageFetch::Retryable {
                after_seconds: 5,
                rate_limited: false,
            },
            MessageFetch::Found,
        )
    }
}

fn discord_resource_url(api_base: &str, path: &str) -> Result<String, AppError> {
    let identifiers_valid = path
        .split(['/', '?', '='])
        .filter(|part| !matches!(*part, "channels" | "messages" | "limit" | "1"))
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    if !identifiers_valid {
        return Err(AppError::Validation(
            "Discord provider identity is invalid.".to_owned(),
        ));
    }
    Ok(format!("{api_base}/{path}"))
}

fn classify_readiness_status(status: StatusCode) -> Result<(), AppError> {
    match status {
        status if status.is_success() => Ok(()),
        StatusCode::UNAUTHORIZED => Err(AppError::Validation(
            "The organization Discord credential is not valid.".to_owned(),
        )),
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Err(AppError::Conflict(
            "The Discord bot cannot read the selected channel or thread.".to_owned(),
        )),
        _ => Err(AppError::Unavailable(
            "Discord inbound readiness unavailable.".to_owned(),
        )),
    }
}

fn readiness_error(error: &AppError) -> &'static str {
    match error {
        AppError::Validation(_) => "missing_credential",
        AppError::Conflict(_) => "guild_access",
        _ => "gateway_unavailable",
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        sync::watch,
        task::JoinHandle,
    };
    use twilight_gateway::Intents;

    use super::{
        DiscordInboundReadinessRest, MessageFetch, configured_intents, discord_resource_url,
        join_worker_with_grace,
    };
    use crate::{config::Secret, error::AppError};

    async fn fake_rest(responses: Vec<&'static str>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Discord REST");
        let address = listener.local_addr().expect("fake address");
        let join = tokio::spawn(async move {
            for response in responses {
                let (mut socket, _) = listener.accept().await.expect("accept fake request");
                let mut request = vec![0_u8; 4_096];
                let _ = socket.read(&mut request).await.expect("read fake request");
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write fake response");
                socket.shutdown().await.expect("close fake response");
            }
        });
        (format!("http://{address}"), join)
    }

    #[tokio::test]
    async fn fake_rest_validates_exact_thread_access_without_redirects() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (base, join) = fake_rest(vec![ok, ok]).await;
        let rest = DiscordInboundReadinessRest::with_api_base(&base).expect("fake client");
        rest.validate_thread(&Secret::new("synthetic"), "123456789012345678")
            .await
            .expect("channel and message-history access");
        join.await.expect("fake completed");

        let redirect = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (base, join) = fake_rest(vec![redirect]).await;
        let rest = DiscordInboundReadinessRest::with_api_base(&base).expect("fake client");
        assert!(matches!(
            rest.validate_thread(&Secret::new("synthetic"), "123456789012345678")
                .await,
            Err(AppError::Unavailable(_))
        ));
        join.await.expect("fake completed");
    }

    #[tokio::test]
    async fn fake_rest_honors_retry_after_and_classifies_missing_messages() {
        let rate_limited = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17.2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (base, join) = fake_rest(vec![rate_limited]).await;
        let rest = DiscordInboundReadinessRest::with_api_base(&base).expect("fake client");
        assert!(matches!(
            rest.fetch_message(
                &Secret::new("synthetic"),
                "123456789012345678",
                "223456789012345678"
            )
            .await,
            MessageFetch::Retryable {
                after_seconds: 18,
                rate_limited: true
            }
        ));
        join.await.expect("fake completed");

        let missing = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (base, join) = fake_rest(vec![missing]).await;
        let rest = DiscordInboundReadinessRest::with_api_base(&base).expect("fake client");
        assert!(matches!(
            rest.fetch_message(
                &Secret::new("synthetic"),
                "123456789012345678",
                "223456789012345678"
            )
            .await,
            MessageFetch::Missing
        ));
        join.await.expect("fake completed");
    }

    #[test]
    fn gateway_and_rest_adapter_boundaries_are_least_privilege_and_server_validated() {
        assert_eq!(
            configured_intents(),
            Intents::GUILDS | Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT
        );
        assert!(discord_resource_url("http://fake", "channels/not-browser-safe").is_err());
        assert_eq!(
            discord_resource_url(
                "http://fake",
                "channels/123456789012345678/messages/223456789012345678"
            )
            .expect("valid provider path"),
            "http://fake/channels/123456789012345678/messages/223456789012345678"
        );
    }

    #[tokio::test]
    async fn worker_shutdown_allows_the_stop_receiver_to_finish_before_abort() {
        let (stop, mut receive) = watch::channel(false);
        let join = tokio::spawn(async move {
            let _ = receive.changed().await;
            assert!(*receive.borrow());
        });
        stop.send(true).expect("signal stop");
        join_worker_with_grace(join, std::time::Duration::from_millis(100)).await;
    }
}
