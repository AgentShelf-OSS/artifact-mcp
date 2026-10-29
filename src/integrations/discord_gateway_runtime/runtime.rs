//! Guarded production Discord Gateway runtime for PBI-080.
//!
//! Twilight owns heartbeat, reconnect, resume, compression, and intent protocol behavior. This
//! adapter owns only organization-scoped credential resolution, exact server-side routing, safe
//! normalization, bounded REST hydration, and optional-integration health. The runtime is off by
//! default and never affects HTTP/application liveness or the outbound durable outbox.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{sync::watch, task::JoinHandle};
use twilight_gateway::{
    ConfigBuilder, Event, EventTypeFlags, Intents, Session, Shard, ShardId, StreamExt as _,
};
use twilight_model::gateway::payload::incoming::Ready;

use crate::{
    config::Secret,
    integrations::discord_inbound::InboundResult,
    model::OrgId,
    persistence::discord_inbound::{DiscordInboundStore, GatewayOrganizationTarget, GatewayResume},
    ports::discussions::OrganizationDiscordCredentialService,
};

use super::metrics::{increment_metric, refresh_metrics};
use super::normalize::{message_version, normalize_event, normalize_message};
use super::rest::{DiscordInboundReadinessRest, MessageFetch, readiness_error};

const SUPERVISOR_POLL: Duration = Duration::from_secs(2);
const INBOX_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const INBOX_RETENTION_DAYS: u16 = 30;
const INBOX_CLEANUP_BATCH: u16 = 500;
const PENDING_FETCH_BATCH: u16 = 25;
const PENDING_FETCH_POLL: Duration = Duration::from_secs(5);
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
                if !reconcile_workers(
                    &store,
                    credentials.as_ref(),
                    &stop,
                    &mut workers,
                    &mut last_cleanup,
                )
                .await
                {
                    continue;
                }
            }
        }
    }

    for (_, worker) in workers {
        join_worker_with_grace(worker.join, WORKER_SHUTDOWN_GRACE).await;
    }
}

async fn reconcile_workers(
    store: &DiscordInboundStore,
    credentials: &dyn OrganizationDiscordCredentialService,
    stop: &watch::Receiver<bool>,
    workers: &mut BTreeMap<String, ActiveWorker>,
    last_cleanup: &mut Instant,
) -> bool {
    refresh_supervisor_metrics(store).await;
    cleanup_inbox_if_due(store, last_cleanup).await;
    let Some(wanted) = load_gateway_targets(store).await else {
        return false;
    };
    stop_stale_workers(store, workers, &wanted).await;
    start_missing_workers(store, credentials, stop, workers, wanted).await;
    true
}

async fn refresh_supervisor_metrics(store: &DiscordInboundStore) {
    if let Ok(snapshot) = store.operational_metrics().await {
        refresh_metrics(snapshot);
    }
}

async fn cleanup_inbox_if_due(store: &DiscordInboundStore, last_cleanup: &mut Instant) {
    if last_cleanup.elapsed() >= INBOX_CLEANUP_INTERVAL {
        if store
            .cleanup_processed_events(INBOX_RETENTION_DAYS, INBOX_CLEANUP_BATCH)
            .await
            .is_err()
        {
            tracing::warn!("Discord inbound inbox cleanup failed");
        }
        *last_cleanup = Instant::now();
    }
}

async fn load_gateway_targets(
    store: &DiscordInboundStore,
) -> Option<BTreeMap<String, GatewayOrganizationTarget>> {
    let targets = match store.gateway_targets().await {
        Ok(targets) => targets,
        Err(_) => {
            tracing::warn!("Discord inbound supervisor could not load targets");
            return None;
        }
    };
    Some(
        targets
            .into_iter()
            .map(|target| (target.org.0.clone(), target))
            .collect(),
    )
}

async fn stop_stale_workers(
    store: &DiscordInboundStore,
    workers: &mut BTreeMap<String, ActiveWorker>,
    wanted: &BTreeMap<String, GatewayOrganizationTarget>,
) {
    let stale: Vec<String> = workers
        .iter()
        .filter(|(org, worker)| {
            worker.join.is_finished()
                || wanted.get(*org).is_none_or(|target| {
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
                    .set_gateway_health(OrgId::from(org), credential_version, "disabled", "", None)
                    .await;
            }
        }
    }
}

async fn start_missing_workers(
    store: &DiscordInboundStore,
    credentials: &dyn OrganizationDiscordCredentialService,
    stop: &watch::Receiver<bool>,
    workers: &mut BTreeMap<String, ActiveWorker>,
    wanted: BTreeMap<String, GatewayOrganizationTarget>,
) {
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
    let Some((rest, mut shard, mut pending_ticker)) =
        initialize_organization_gateway(&store, &target, &credential).await
    else {
        return;
    };

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
        if !handle_gateway_event(
            &store,
            &target,
            &credential,
            &rest,
            event,
            shard_session(&shard),
        )
        .await
        {
            break;
        }
    }
}

async fn initialize_organization_gateway(
    store: &DiscordInboundStore,
    target: &GatewayOrganizationTarget,
    credential: &Secret,
) -> Option<(DiscordInboundReadinessRest, Shard, tokio::time::Interval)> {
    let rest = match DiscordInboundReadinessRest::new() {
        Ok(rest) => rest,
        Err(_) => {
            let _ = store
                .set_gateway_health(
                    target.org.clone(),
                    target.credential_version,
                    "failed",
                    "gateway_unavailable",
                    None,
                )
                .await;
            return None;
        }
    };
    if let Err(error) = rest.validate_channel(credential, &target.channel_id).await {
        let safe_error = readiness_error(&error);
        let _ = store
            .set_gateway_health(
                target.org.clone(),
                target.credential_version,
                "failed",
                safe_error,
                None,
            )
            .await;
        return None;
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
    let shard = Shard::with_config(ShardId::ONE, config);
    let mut pending_ticker = tokio::time::interval(PENDING_FETCH_POLL);
    pending_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    Some((rest, shard, pending_ticker))
}

async fn handle_gateway_event(
    store: &DiscordInboundStore,
    target: &GatewayOrganizationTarget,
    credential: &Secret,
    rest: &DiscordInboundReadinessRest,
    event: Event,
    session: Option<(String, String, u64)>,
) -> bool {
    match event {
        Event::Ready(ready) => {
            if !ready_has_guild(&ready, &target.guild_id) {
                persist_shard_health(store, target, session.clone(), "failed", "guild_access")
                    .await;
                return false;
            }
            persist_shard_health(store, target, session, "ready", "").await;
        }
        Event::Resumed => {
            persist_shard_health(store, target, session, "ready", "").await;
        }
        Event::GatewayClose(frame) => {
            let safe_error = match frame.as_ref().map(|frame| frame.code) {
                Some(4004) => "missing_credential",
                Some(4013 | 4014) => "message_content_intent",
                _ => "gateway_unavailable",
            };
            persist_shard_health(store, target, session, "failed", safe_error).await;
        }
        Event::GatewayReconnect | Event::GatewayInvalidateSession(_) => {
            increment_metric(|metrics| &mut metrics.reconnects);
            persist_shard_health(
                store,
                target,
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
                    return true;
                }
                match store.apply_event(inbound).await {
                    Ok(InboundResult::NeedsFetch) => {
                        drain_pending_updates(store, target, credential, rest).await;
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
                persist_shard_health(store, target, session, "ready", "").await;
            }
        }
    }
    true
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

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        sync::watch,
        task::JoinHandle,
    };
    use twilight_gateway::Intents;

    use super::super::rest::{DiscordInboundReadinessRest, MessageFetch, discord_resource_url};
    use super::{configured_intents, join_worker_with_grace};
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

    #[test]
    fn readiness_failures_map_to_the_existing_safe_gateway_health_codes() {
        assert_eq!(
            super::super::rest::readiness_error(&AppError::Validation("invalid".to_owned())),
            "missing_credential"
        );
        assert_eq!(
            super::super::rest::readiness_error(&AppError::Conflict("denied".to_owned())),
            "guild_access"
        );
        assert_eq!(
            super::super::rest::readiness_error(&AppError::Unavailable("offline".to_owned())),
            "gateway_unavailable"
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
